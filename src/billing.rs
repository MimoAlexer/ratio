mod caps;
#[cfg(test)]
mod checkout_tests;
#[cfg(test)]
mod delivery_tests;
mod entitlement;
mod page;
mod provider;
pub(crate) use caps::enforce_repo_cap;

use crate::{AppError, AppResult, AppState, auth::Auth};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Form, State},
    http::{HeaderMap, StatusCode},
    response::Redirect,
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use entitlement::{Access, Evidence, Plan};
use provider::{CheckoutStatus, Stripe, Subscription};
use serde::Deserialize;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/billing", get(page::show))
        .route("/billing/checkout", post(checkout))
        .route("/billing/portal", post(portal))
        .route("/billing/selection", post(caps::selection))
        .route(
            "/billing/webhook",
            post(webhook).layer(DefaultBodyLimit::max(512 * 1024)),
        )
}

#[derive(sqlx::FromRow)]
struct Account {
    customer_id: Option<String>,
    provider_live: bool,
    subscription_id: Option<String>,
    provider_status: String,
    paid_until: Option<DateTime<Utc>>,
    grace_until: Option<DateTime<Utc>>,
    pending_plan: Option<String>,
    pending_key: Option<Uuid>,
    pending_started_at: Option<DateTime<Utc>>,
    pending_expires_at: Option<DateTime<Utc>>,
    pending_session: Option<String>,
    pending_url: Option<String>,
}
type Tx<'a> = Transaction<'a, Postgres>;

async fn lock_owner(tx: &mut Tx<'_>, owner: Uuid) -> AppResult<Plan> {
    let plan = sqlx::query_scalar::<_, String>("SELECT plan FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?;
    Plan::parse(&plan).ok_or_else(|| AppError::Internal("Invalid stored billing tier.".into()))
}
async fn account(tx: &mut Tx<'_>, owner: Uuid) -> AppResult<Account> {
    Ok(
        sqlx::query_as("SELECT * FROM billing_accounts WHERE owner_id=$1")
            .bind(owner)
            .fetch_one(&mut **tx)
            .await?,
    )
}
fn terminal(status: &str) -> bool {
    matches!(status, "free" | "canceled" | "incomplete_expired")
}
async fn clear_pending(tx: &mut Tx<'_>, owner: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE billing_accounts SET pending_plan=NULL,pending_key=NULL,pending_started_at=NULL,pending_expires_at=NULL,pending_session=NULL,pending_url=NULL WHERE owner_id=$1")
        .bind(owner).execute(&mut **tx).await?;
    Ok(())
}
fn conflict(message: &str) -> AppError {
    AppError::Conflict(message.to_owned())
}
fn unavailable() -> AppError {
    AppError::Unavailable("Billing could not be verified. Please try again shortly.".into())
}
fn epoch(value: Option<DateTime<Utc>>) -> Option<i64> {
    value.map(|t| t.timestamp())
}
fn datetime(value: Option<i64>) -> Option<DateTime<Utc>> {
    value.and_then(|t| DateTime::from_timestamp(t, 0))
}

fn select_subscription<'a>(
    subs: &'a [Subscription],
    known: Option<&str>,
) -> AppResult<Option<&'a Subscription>> {
    let mut open = subs.iter().filter(|s| !terminal(&s.status));
    let current = open.next();
    if open.next().is_some() {
        return Err(AppError::Unavailable(
            "Multiple open subscriptions require billing support.".into(),
        ));
    }
    // A reactivated subscription can be canceled again before its first event
    // reaches this service. Recover its paid term even if an older ID is cached.
    Ok(current.or_else(|| {
        subs.iter()
            .max_by_key(|s| (s.paid_through, Some(s.id.as_str()) == known))
    }))
}

async fn set_plan(tx: &mut Tx<'_>, owner: Uuid, old: Plan, new: Plan) -> AppResult<()> {
    if new != old {
        sqlx::query("UPDATE users SET plan=$2 WHERE id=$1")
            .bind(owner)
            .bind(new.name())
            .execute(&mut **tx)
            .await?;
        if new < old {
            caps::enforce(tx, owner, new.name()).await?;
        }
    }
    Ok(())
}

// The owner lock spans the canonical read and local application. Reordered
// callbacks therefore cannot overwrite a newer fetch with an older snapshot.
async fn refresh_locked(
    state: &AppState,
    stripe: &Stripe,
    tx: &mut Tx<'_>,
    owner: Uuid,
    old: Plan,
    a: &Account,
) -> AppResult<Plan> {
    let customer = a.customer_id.as_deref().ok_or_else(unavailable)?;
    if a.provider_live != stripe.live_mode() {
        return Err(unavailable());
    }
    let subscriptions = stripe.subscriptions(&state.http, customer, owner).await?;
    let sub = select_subscription(&subscriptions, a.subscription_id.as_deref())?;
    let now = Utc::now();
    if sub.is_none() && a.subscription_id.is_some() {
        return Err(unavailable());
    }
    let id = sub.map(|s| s.id.as_str());
    let status = sub.map_or("free", |s| s.status.as_str());
    let previous = Access {
        plan: old,
        paid_until: epoch(a.paid_until),
        grace_until: if id == a.subscription_id.as_deref() {
            epoch(a.grace_until)
        } else {
            None
        },
        scheduled: None,
    };
    let evidence = match sub {
        Some(s) => {
            if s.owner != owner || s.customer != customer {
                return Err(unavailable());
            }
            let cancel = s.cancel_at.or_else(|| {
                s.cancel_at_period_end
                    .then_some(s.current_period_end)
                    .flatten()
            });
            Evidence {
                status: &s.status,
                listed: s.plan.and_then(Plan::parse),
                payment: s.paid_plan.and_then(Plan::parse).zip(epoch(s.paid_through)),
                failed_at: epoch(s.past_due_since),
                cancellation: epoch(cancel),
                schedule: s
                    .scheduled_change
                    .and_then(|(p, t)| Plan::parse(p).map(|p| (p, t.timestamp()))),
            }
        }
        None => Evidence {
            status: "free",
            ..Default::default()
        },
    };
    let next = entitlement::resolve(&previous, evidence, now.timestamp());
    set_plan(tx, owner, old, next.plan).await?;
    sqlx::query("UPDATE billing_accounts SET subscription_id=$2, provider_status=$3, paid_until=$4, grace_until=$5, scheduled_plan=$6,plan_changes_at=$7,entitlement_version=1,reconciled_at=now() WHERE owner_id=$1")
        .bind(owner).bind(id).bind(status).bind(datetime(next.paid_until)).bind(datetime(next.grace_until))
        .bind(next.scheduled.map(|(p,_)|p.name())).bind(datetime(next.scheduled.map(|(_,t)|t))).execute(&mut **tx).await?;
    if a.pending_key.is_some() {
        let finished = if !terminal(status) {
            true
        } else if a.pending_expires_at.is_some_and(|t| t <= now) {
            match a.pending_session.as_deref() {
                Some(session) => match stripe
                    .checkout_status(&state.http, session, customer)
                    .await?
                {
                    CheckoutStatus::Expired => true,
                    CheckoutStatus::Complete(Some(id)) => {
                        subscriptions.iter().any(|sub| sub.id == id)
                    }
                    _ => false,
                },
                // Dispatch is limited to the first hour. The reservation lasts
                // 26 hours, longer than that plus Stripe's 24-hour session.
                None => true,
            }
        } else {
            false
        };
        if finished {
            clear_pending(tx, owner).await?;
        }
    }
    Ok(next.plan)
}

#[derive(Deserialize)]
struct CheckoutForm {
    csrf: String,
    plan: String,
}
#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
}

async fn checkout(
    State(state): State<AppState>,
    auth: Auth,
    Form(form): Form<CheckoutForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let plan = Plan::parse(&form.plan)
        .filter(|plan| *plan != Plan::Free)
        .ok_or_else(|| AppError::BadRequest("Choose Plus or Max.".into()))?;
    let stripe = Stripe::from_env()?;
    start_checkout(&state, &stripe, &auth, plan).await
}

async fn start_checkout(
    state: &AppState,
    stripe: &Stripe,
    auth: &Auth,
    plan: Plan,
) -> AppResult<Redirect> {
    let owner = auth.user_id;

    let mut tx = state.db.begin().await?;
    let old = lock_owner(&mut tx, owner).await?;
    sqlx::query("INSERT INTO billing_accounts(owner_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    let mut a = account(&mut tx, owner).await?;
    let customer = match a.customer_id.clone() {
        Some(customer) => customer,
        None => {
            let key = format!("workspace-customer-{owner}");
            let id = stripe
                .create_customer(&state.http, owner, &auth.email, &key)
                .await?;
            sqlx::query(
                "UPDATE billing_accounts SET customer_id=$2,provider_live=$3 WHERE owner_id=$1",
            )
            .bind(owner)
            .bind(&id)
            .bind(stripe.live_mode())
            .execute(&mut *tx)
            .await?;
            a.customer_id = Some(id.clone());
            a.provider_live = stripe.live_mode();
            id
        }
    };
    let current = refresh_locked(state, stripe, &mut tx, owner, old, &a).await?;
    a = account(&mut tx, owner).await?;
    if current != Plan::Free || !terminal(&a.provider_status) {
        tx.commit().await?;
        return Err(conflict(
            "This account already has a subscription. Manage it in the billing portal.",
        ));
    }
    if let Some(session) = a.pending_session.as_deref() {
        match stripe
            .checkout_status(&state.http, session, &customer)
            .await?
        {
            CheckoutStatus::Expired => {
                clear_pending(&mut tx, owner).await?;
                a = account(&mut tx, owner).await?;
            }
            CheckoutStatus::Complete(_) => {
                tx.commit().await?;
                return Err(conflict(
                    "Checkout is processing. Refresh billing after payment confirmation.",
                ));
            }
            CheckoutStatus::Open if a.pending_plan.as_deref() == Some(plan.name()) => {
                let url = a.pending_url.ok_or_else(unavailable)?;
                tx.commit().await?;
                return Ok(Redirect::to(&url));
            }
            CheckoutStatus::Open => {
                tx.commit().await?;
                return Err(conflict(
                    "A Checkout session is already open for another plan. Resume it or wait for it to expire.",
                ));
            }
        }
    }
    let key = match a.pending_key {
        Some(key) if a.pending_plan.as_deref() == Some(plan.name()) => key,
        Some(_) => return Err(conflict("Checkout for another plan is pending.")),
        None => {
            let key = Uuid::new_v4();
            // Uncertain dispatches stay reserved beyond a hosted session's
            // maximum 24-hour lifetime. A successful response sets Stripe's expiry.
            sqlx::query("UPDATE billing_accounts SET pending_key=$2,pending_plan=$3,pending_started_at=now(),pending_expires_at=now()+interval '26 hours' WHERE owner_id=$1")
                .bind(owner).bind(key).bind(plan.name()).execute(&mut *tx).await?;
            key
        }
    };
    tx.commit().await?;

    // Persist the request key before dispatch, including when the process
    // crashes after Stripe creates the session and before saving its response.
    let mut tx = state.db.begin().await?;
    let current = lock_owner(&mut tx, owner).await?;
    let a = account(&mut tx, owner).await?;
    if a.pending_key != Some(key) || current != Plan::Free || !terminal(&a.provider_status) {
        return Err(conflict("Billing changed. Reload the billing page."));
    }
    if let Some(url) = a.pending_url {
        tx.commit().await?;
        return Ok(Redirect::to(&url));
    }
    if a.pending_started_at
        .is_none_or(|start| Utc::now() - start >= Duration::hours(1))
    {
        return Err(conflict(
            "An earlier Checkout request is awaiting verification. Try again after its reservation expires.",
        ));
    }
    let session = stripe
        .checkout(
            &state.http,
            owner,
            &customer,
            plan.name(),
            &state.config.base_url,
            &key.to_string(),
        )
        .await?;
    sqlx::query("UPDATE billing_accounts SET pending_session=$2,pending_url=$3,pending_expires_at=$4 WHERE owner_id=$1")
        .bind(owner).bind(session.id).bind(&session.url).bind(session.expires_at).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Redirect::to(&session.url))
}

async fn portal(
    State(state): State<AppState>,
    auth: Auth,
    Form(form): Form<CsrfForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let stripe = Stripe::from_env()?;
    let customer = sqlx::query_scalar::<_, String>(
        "SELECT customer_id FROM billing_accounts WHERE owner_id=$1 AND customer_id IS NOT NULL AND provider_live=$2",
    )
    .bind(auth.user_id)
    .bind(stripe.live_mode())
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| conflict("This account has no billing profile."))?;
    let url = stripe
        .portal(&state.http, &customer, &state.config.base_url)
        .await?;
    Ok(Redirect::to(&url))
}

async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<StatusCode> {
    let stripe = Stripe::from_env()?;
    handle_webhook(&state, &stripe, headers, body).await
}

async fn handle_webhook(
    state: &AppState,
    stripe: &Stripe,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<StatusCode> {
    let signature = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Forbidden)?;
    let event = stripe.event(signature, &body, Utc::now())?;
    if !(event.kind.starts_with("customer.subscription.")
        || event.kind.starts_with("invoice.")
        || event.kind.starts_with("checkout.session.")
        || event.kind.starts_with("subscription_schedule."))
    {
        return Ok(StatusCode::NO_CONTENT);
    }
    if event.kind.starts_with("invoice.") && event.subscription.is_none() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let Some(customer) = event.customer.as_deref() else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let mut tx = state.db.begin().await?;
    let owner =
        sqlx::query_scalar::<_, Uuid>("SELECT owner_id FROM billing_accounts WHERE customer_id=$1")
            .bind(customer)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(owner) = owner else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let old = lock_owner(&mut tx, owner).await?;
    let inserted = sqlx::query(
        "INSERT INTO billing_events(owner_id,event_id) VALUES($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(owner)
    .bind(event.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted != 0 {
        let a = account(&mut tx, owner).await?;
        refresh_locked(state, stripe, &mut tx, owner, old, &a).await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Called under the owner lock, in the transaction that deletes the workspace.
pub async fn ensure_deletable(tx: &mut Tx<'_>, owner: Uuid) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO billing_shutdowns(owner_id, customer_id, provider_live)
        SELECT owner_id, customer_id, provider_live FROM billing_accounts
        WHERE owner_id=$1 AND customer_id IS NOT NULL ON CONFLICT(owner_id) DO NOTHING",
    )
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Expire hosted sessions and cancel only this deleted workspace's subscriptions.
/// The customer record and receipts stay with Stripe.
pub async fn close_deleted_accounts(state: &AppState) -> AppResult<()> {
    let stripe = Stripe::from_env().ok();
    drain_shutdowns(state, stripe.as_ref()).await
}

async fn drain_shutdowns(state: &AppState, provider: Option<&Stripe>) -> AppResult<()> {
    let mut failed = false;
    for _ in 0..50 {
        let mut tx = state.db.begin().await?;
        let row = sqlx::query_as::<_, (Uuid, String, bool, DateTime<Utc>)>(
            "SELECT owner_id,customer_id,provider_live,created_at FROM billing_shutdowns
             WHERE next_attempt_at<=now() ORDER BY next_attempt_at LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some((owner, customer, live, created)) = row else {
            break;
        };
        let result = match provider {
            Some(stripe) if stripe.live_mode() == live => {
                stripe.stop_billing(&state.http, &customer, owner).await
            }
            _ => Err(unavailable()),
        };
        if result.is_err() {
            failed = true;
            sqlx::query(
                "UPDATE billing_shutdowns SET attempts=attempts+1,
                next_attempt_at=now()+make_interval(mins=>LEAST(attempts+1,15)) WHERE owner_id=$1",
            )
            .bind(owner)
            .execute(&mut *tx)
            .await?;
        } else if Utc::now() < created + Duration::hours(26) {
            sqlx::query("UPDATE billing_shutdowns SET next_attempt_at=created_at+interval '26 hours',attempts=0 WHERE owner_id=$1")
                .bind(owner).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM billing_shutdowns WHERE owner_id=$1")
                .bind(owner)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    if failed {
        return Err(unavailable());
    }
    Ok(())
}

async fn expire_cached(state: &AppState, owner: Uuid) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    let old = lock_owner(&mut tx, owner).await?;
    let a = account(&mut tx, owner).await?;
    let end = a.paid_until.max(a.grace_until);
    if old != Plan::Free && end.is_none_or(|end| end <= Utc::now()) {
        set_plan(&mut tx, owner, old, Plan::Free).await?;
    }
    tx.commit().await?;
    Ok(())
}

// Also callable by a one-shot maintenance command. Keep per-account failures
// isolated so a stale provider record cannot starve other workspaces.
pub async fn reconcile(state: &AppState) -> AppResult<()> {
    let stripe = Stripe::from_env();
    let mut after = None::<Uuid>;
    let mut failed = false;
    loop {
        let ids=sqlx::query_scalar::<_,Uuid>("SELECT owner_id FROM billing_accounts WHERE customer_id IS NOT NULL AND ($1::uuid IS NULL OR owner_id>$1) ORDER BY owner_id LIMIT 100")
            .bind(after).fetch_all(&state.db).await?;
        if ids.is_empty() {
            break;
        }
        for owner in ids {
            after = Some(owner);
            let result: AppResult<()> = async {
                let provider = stripe.as_ref().map_err(|_| unavailable())?;
                let mut tx = state.db.begin().await?;
                let old = lock_owner(&mut tx, owner).await?;
                let a = account(&mut tx, owner).await?;
                refresh_locked(state, provider, &mut tx, owner, old, &a).await?;
                tx.commit().await?;
                Ok(())
            }
            .await;
            if result.is_err() {
                failed = true;
                if expire_cached(state, owner).await.is_err() {
                    tracing::warn!("billing cache expiry failed");
                }
            }
        }
    }
    if failed {
        return Err(unavailable());
    }
    Ok(())
}
