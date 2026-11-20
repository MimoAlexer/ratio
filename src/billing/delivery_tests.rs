use super::{handle_webhook, provider::Stripe};
use crate::{AppResult, AppState, Config};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const CUSTOMER: &str = "cus_delivery";
const SUBSCRIPTION: &str = "sub_delivery";
const ITEM: &str = "si_delivery";

#[derive(Default)]
struct ProviderRecords {
    subscriptions: Vec<Value>,
    previous_invoices: Vec<Value>,
    unavailable: bool,
    reads: usize,
}

type Records = Arc<Mutex<ProviderRecords>>;

async fn read_subscriptions(State(records): State<Records>) -> Response {
    let mut records = records.lock().unwrap();
    records.reads += 1;
    if records.unavailable {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(json!({"has_more":false, "data":records.subscriptions})).into_response()
}

async fn read_invoices(State(records): State<Records>) -> Json<Value> {
    Json(json!({"has_more":false, "data":records.lock().unwrap().previous_invoices}))
}

struct Fixture {
    app: AppState,
    stripe: Stripe,
    records: Records,
    owner: Uuid,
}

impl Fixture {
    async fn start(db: PgPool) -> AppResult<Self> {
        let records = Records::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Router::new()
            .route("/subscriptions", get(read_subscriptions))
            .route(
                "/checkout/sessions",
                get(|| async { Json(json!({"has_more":false,"data":[]})) }),
            )
            .route("/invoices", get(read_invoices))
            .with_state(records.clone());
        tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let owner = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES ('billing-delivery@example.test') RETURNING id",
        )
        .fetch_one(&db)
        .await?;
        sqlx::query("INSERT INTO billing_accounts(owner_id,customer_id) VALUES($1,$2)")
            .bind(owner)
            .bind(CUSTOMER)
            .execute(&db)
            .await?;
        let app = AppState {
            db,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            config: Arc::new(Config {
                base_url: "http://localhost".into(),
                production: false,
            }),
        };
        Ok(Self {
            app,
            stripe: Stripe::test_endpoint(format!("http://{addr}")),
            records,
            owner,
        })
    }

    async fn deliver(&self, id: &str, kind: &str) -> AppResult<StatusCode> {
        let now = Utc::now().timestamp();
        // The payload is intentionally inconsistent with several canonical
        // responses. Only the fresh, provider-origin read may change access.
        let body = serde_json::to_vec(&json!({
            "id": id, "type": kind, "livemode": false,
            "data": {"object":{"customer":CUSTOMER,"subscription":SUBSCRIPTION,
                "status":"paid","price":"price_max","metadata":{"plan":"max"}}}
        }))
        .unwrap();
        let mut signer = Hmac::<Sha256>::new_from_slice(b"whsec_fixture").unwrap();
        signer.update(format!("{now}.").as_bytes());
        signer.update(&body);
        let signature = format!("t={now},v1={}", hex::encode(signer.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("stripe-signature", signature.parse().unwrap());
        handle_webhook(&self.app, &self.stripe, headers, Bytes::from(body)).await
    }

    async fn plan(&self) -> String {
        sqlx::query_scalar("SELECT plan FROM users WHERE id=$1")
            .bind(self.owner)
            .fetch_one(&self.app.db)
            .await
            .unwrap()
    }

    async fn deliveries(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM billing_events WHERE owner_id=$1")
            .bind(self.owner)
            .fetch_one(&self.app.db)
            .await
            .unwrap()
    }
}

fn plus_price() -> Value {
    json!({"id":"price_plus","livemode":false,"unit_amount":500,"currency":"usd",
        "type":"recurring","billing_scheme":"per_unit",
        "recurring":{"interval":"month","interval_count":1,"usage_type":"licensed"}})
}

fn invoice(paid: bool, begin: i64, end: i64) -> Value {
    json!({"id":"in_delivery","livemode":false,"customer":CUSTOMER,"subscription":SUBSCRIPTION,
    "status":if paid {"paid"} else {"open"},"paid":paid,"attempted":true,
    "created":begin,"status_transitions":{"finalized_at":begin},
    "lines":{"has_more":false,"data":[{
        "id":"il_delivery","currency":"usd","type":"subscription","subscription":SUBSCRIPTION,
        "subscription_item":ITEM,"quantity":1,"amount":500,"price":plus_price(),
        "period":{"start":begin,"end":end}
    }]}})
}

fn subscription(owner: Uuid, status: &str, end: i64, latest: Value) -> Value {
    json!({"id":SUBSCRIPTION,"livemode":false,"metadata":{"workspace_id":owner},
        "customer":CUSTOMER,"status":status,"current_period_end":end,
        "cancel_at_period_end":false,"latest_invoice":latest,
        "items":{"has_more":false,"data":[{"id":ITEM,"quantity":1,"price":plus_price()}]}})
}

#[sqlx::test]
async fn signed_checkout_waits_for_payment_and_concurrent_or_reordered_delivery_is_safe(
    db: PgPool,
) -> AppResult<()> {
    let f = Fixture::start(db).await?;
    let now = Utc::now().timestamp();
    let end = now + 30 * 86400;
    f.records.lock().unwrap().subscriptions = vec![subscription(
        f.owner,
        "incomplete",
        end,
        invoice(false, now - 100, end),
    )];
    assert_eq!(
        f.deliver("evt_checkout", "checkout.session.completed")
            .await?,
        StatusCode::NO_CONTENT
    );
    assert_eq!(f.plan().await, "free");

    f.records.lock().unwrap().subscriptions = vec![subscription(
        f.owner,
        "active",
        end,
        invoice(true, now - 100, end),
    )];
    let (first, retry) = tokio::join!(
        f.deliver("evt_paid", "invoice.paid"),
        f.deliver("evt_paid", "invoice.paid")
    );
    assert_eq!(first?, StatusCode::NO_CONTENT);
    assert_eq!(retry?, StatusCode::NO_CONTENT);
    assert_eq!(f.plan().await, "plus");
    assert_eq!(f.deliveries().await, 2);
    assert_eq!(f.records.lock().unwrap().reads, 2);

    // Stripe sends an earlier failed-payment event after the settled invoice.
    f.deliver("evt_old_failed", "invoice.payment_failed")
        .await?;
    assert_eq!(f.plan().await, "plus");
    let grace: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT grace_until FROM billing_accounts WHERE owner_id=$1")
            .bind(f.owner)
            .fetch_one(&f.app.db)
            .await?;
    assert!(grace.is_none());
    f.records.lock().unwrap().subscriptions = vec![subscription(
        f.owner,
        "canceled",
        end,
        invoice(true, now - 100, end),
    )];
    f.deliver("evt_cancel", "customer.subscription.deleted")
        .await?;
    assert_eq!(f.plan().await, "plus");
    let scheduled: (String,i64) = sqlx::query_as("SELECT scheduled_plan, EXTRACT(EPOCH FROM plan_changes_at)::bigint FROM billing_accounts WHERE owner_id=$1")
        .bind(f.owner).fetch_one(&f.app.db).await?;
    assert_eq!(scheduled, ("free".into(), end));
    Ok(())
}

#[sqlx::test]
async fn provider_error_rolls_back_the_delivery_and_allows_retry(db: PgPool) -> AppResult<()> {
    let f = Fixture::start(db).await?;
    f.records.lock().unwrap().unavailable = true;
    assert!(f.deliver("evt_retry", "invoice.paid").await.is_err());
    assert_eq!(f.deliveries().await, 0);
    assert_eq!(f.plan().await, "free");

    let now = Utc::now().timestamp();
    let end = now + 86400;
    {
        let mut records = f.records.lock().unwrap();
        records.unavailable = false;
        records.subscriptions = vec![subscription(
            f.owner,
            "active",
            end,
            invoice(true, now - 3600, end),
        )];
    }
    assert_eq!(
        f.deliver("evt_retry", "invoice.paid").await?,
        StatusCode::NO_CONTENT
    );
    assert_eq!(f.deliveries().await, 1);
    assert_eq!(f.plan().await, "plus");
    Ok(())
}

#[sqlx::test]
async fn overdue_paid_invoice_has_seven_days_from_the_first_failure(db: PgPool) -> AppResult<()> {
    let f = Fixture::start(db).await?;
    let now = Utc::now().timestamp();
    let failure = now - 2 * 86400;
    {
        let mut records = f.records.lock().unwrap();
        records.previous_invoices = vec![invoice(true, failure - 30 * 86400, failure)];
        records.subscriptions = vec![subscription(
            f.owner,
            "past_due",
            failure + 30 * 86400,
            invoice(false, failure, failure + 30 * 86400),
        )];
    }
    f.deliver("evt_failed", "invoice.payment_failed").await?;
    assert_eq!(f.plan().await, "plus");
    let deadline: i64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM grace_until)::bigint FROM billing_accounts WHERE owner_id=$1",
    )
    .bind(f.owner)
    .fetch_one(&f.app.db)
    .await?;
    assert_eq!(deadline, failure + 7 * 86400);

    // A delayed correction supplies the actual, earlier first-failure date.
    let actual = now - 8 * 86400;
    {
        let mut records = f.records.lock().unwrap();
        records.previous_invoices = vec![invoice(true, actual - 30 * 86400, actual)];
        records.subscriptions = vec![subscription(
            f.owner,
            "past_due",
            actual + 30 * 86400,
            invoice(false, actual, actual + 30 * 86400),
        )];
    }
    f.deliver("evt_old_failure", "customer.subscription.updated")
        .await?;
    assert_eq!(f.plan().await, "free");
    let renewed = now + 29 * 86400;
    f.records.lock().unwrap().subscriptions = vec![subscription(
        f.owner,
        "active",
        renewed,
        invoice(true, now - 3600, renewed),
    )];
    f.deliver("evt_recovered", "invoice.paid").await?;
    assert_eq!(f.plan().await, "plus");
    Ok(())
}

#[sqlx::test]
async fn paid_subscription_with_another_workspace_marker_cannot_unlock_access(
    db: PgPool,
) -> AppResult<()> {
    let f = Fixture::start(db).await?;
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO users(email) VALUES ('other-billing-owner@example.test') RETURNING id",
    )
    .fetch_one(&f.app.db)
    .await?;
    let now = Utc::now().timestamp();
    let end = now + 86400;
    f.records.lock().unwrap().subscriptions = vec![subscription(
        other,
        "active",
        end,
        invoice(true, now - 100, end),
    )];

    f.deliver("evt_other", "invoice.paid").await?;
    assert_eq!(f.plan().await, "free");
    let other_plan: String = sqlx::query_scalar("SELECT plan FROM users WHERE id=$1")
        .bind(other)
        .fetch_one(&f.app.db)
        .await?;
    assert_eq!(other_plan, "free");
    Ok(())
}

#[sqlx::test]
async fn deleted_workspace_keeps_a_retryable_shutdown_until_the_final_remote_check(
    db: PgPool,
) -> AppResult<()> {
    let f = Fixture::start(db).await?;
    for commit in [false, true] {
        let mut tx = f.app.db.begin().await?;
        super::lock_owner(&mut tx, f.owner).await?;
        super::ensure_deletable(&mut tx, f.owner).await?;
        sqlx::query("DELETE FROM users WHERE id=$1")
            .bind(f.owner)
            .execute(&mut *tx)
            .await?;
        if commit {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        let owner_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
                .bind(f.owner)
                .fetch_one(&f.app.db)
                .await?;
        let queued: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM billing_shutdowns WHERE owner_id=$1)")
                .bind(f.owner)
                .fetch_one(&f.app.db)
                .await?;
        assert_eq!(owner_exists, !commit);
        assert_eq!(queued, commit);
    }
    assert!(super::drain_shutdowns(&f.app, None).await.is_err());
    let (attempts, deferred): (i32, bool) = sqlx::query_as(
        "SELECT attempts, next_attempt_at > now() FROM billing_shutdowns WHERE owner_id=$1",
    )
    .bind(f.owner)
    .fetch_one(&f.app.db)
    .await?;
    assert_eq!((attempts, deferred), (1, true));

    sqlx::query("UPDATE billing_shutdowns SET next_attempt_at=now() WHERE owner_id=$1")
        .bind(f.owner)
        .execute(&f.app.db)
        .await?;
    super::drain_shutdowns(&f.app, Some(&f.stripe)).await?;
    let second_check: bool = sqlx::query_scalar(
        "SELECT attempts=0 AND next_attempt_at=created_at+interval '26 hours' FROM billing_shutdowns WHERE owner_id=$1",
    ).bind(f.owner).fetch_one(&f.app.db).await?;
    assert!(second_check);
    assert_eq!(f.records.lock().unwrap().reads, 1);
    super::drain_shutdowns(&f.app, Some(&f.stripe)).await?;
    assert_eq!(
        f.records.lock().unwrap().reads,
        1,
        "the future pass must wait for its due date"
    );

    // Advance the queue clock past the maximum hosted session lifetime.
    sqlx::query("UPDATE billing_shutdowns SET created_at=now()-interval '27 hours', next_attempt_at=now() WHERE owner_id=$1")
        .bind(f.owner).execute(&f.app.db).await?;
    super::drain_shutdowns(&f.app, Some(&f.stripe)).await?;
    assert_eq!(f.records.lock().unwrap().reads, 2);
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM billing_shutdowns WHERE owner_id=$1")
            .bind(f.owner)
            .fetch_one(&f.app.db)
            .await?;
    assert_eq!(remaining, 0);
    Ok(())
}

#[sqlx::test]
async fn canceled_reactivation_uses_the_later_verified_paid_term(db: PgPool) -> AppResult<()> {
    let f = Fixture::start(db).await?;
    let now = Utc::now().timestamp();
    let ended = now - 86400;
    let current_end = now + 28 * 86400;
    let old = subscription(
        f.owner,
        "canceled",
        ended,
        invoice(true, ended - 30 * 86400, ended),
    );
    f.records.lock().unwrap().subscriptions = vec![old.clone()];
    f.deliver("evt_old_cancel", "customer.subscription.deleted")
        .await?;
    assert_eq!(f.plan().await, "free");

    // A second subscription was paid, then canceled before either webhook
    // arrived. Reconciliation sees both canceled subscriptions for this owner.
    let mut new = subscription(
        f.owner,
        "canceled",
        current_end,
        invoice(true, now - 86400, current_end),
    );
    new["id"] = "sub_reactivated".into();
    new["latest_invoice"]["id"] = "in_reactivated".into();
    new["latest_invoice"]["subscription"] = "sub_reactivated".into();
    new["latest_invoice"]["lines"]["data"][0]["subscription"] = "sub_reactivated".into();
    f.records.lock().unwrap().subscriptions = vec![new, old];
    f.deliver("evt_reactivated_cancel", "customer.subscription.deleted")
        .await?;
    assert_eq!(
        f.plan().await,
        "plus",
        "a previous canceled subscription cannot hide the current paid-through term"
    );
    let (sub, until): (String,i64) = sqlx::query_as(
        "SELECT subscription_id, EXTRACT(EPOCH FROM paid_until)::bigint FROM billing_accounts WHERE owner_id=$1",
    ).bind(f.owner).fetch_one(&f.app.db).await?;
    assert_eq!((sub.as_str(), until), ("sub_reactivated", current_end));
    Ok(())
}
