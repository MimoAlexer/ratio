use std::net::SocketAddr;

use axum::{
    Router,
    extract::{ConnectInfo, Extension, Form, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    Auth, budget, check_nonce, dev_links, digest, form_nonce, lock_email, mail_link, new_session,
    normalized_email, public_page, put_nonce, put_session, session_cookie_raw, token, valid_token,
};
use crate::{AppError, AppResult, AppState, ui};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings/recovery", post(generate))
        .route("/recover", get(start).post(request))
        .route("/recover/check", get(check_inbox))
        .route("/recover/confirm", get(landing).post(confirm))
}

pub(super) fn settings(auth: &Auth) -> Markup {
    html! {
        section.panel {
            h2 { "Account recovery" }
            p { "Save a recovery code in your password manager so you can replace a lost email address. Generating a code replaces your previous code." }
            form method="post" action="/settings/recovery" {
                (ui::csrf(auth))
                button.secondary type="submit" { "Generate recovery code" }
            }
        }
    }
}

#[derive(Deserialize)]
struct GenerateForm {
    csrf: String,
}

async fn generate(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Form(form): Form<GenerateForm>,
) -> AppResult<Html<String>> {
    auth.check_csrf(&form.csrf)?;
    let session =
        session_cookie_raw(&headers, state.config.production).ok_or(AppError::Forbidden)?;
    let code = token();
    if !replace_code(&state.db, auth.user_id, session, &code).await? {
        return Ok(ui::page(
            &auth,
            "Confirm your email",
            "settings",
            html! {
                section.panel {
                    h1 { "Sign in again to generate a recovery code" }
                    p { "Use a fresh email link, then return to Account recovery in Settings." }
                    a.button href="/login?next=%2Fsettings" { "Sign in again" }
                }
            },
        ));
    }
    Ok(ui::page(
        &auth,
        "Save your recovery code",
        "settings",
        html! {
            section.panel {
                h1 { "Save your recovery code" }
                p { "Keep this code in your password manager. Anyone who has it can move your workspace to a new email address." }
                label.field { "Recovery code" input readonly value=(code) autocomplete="off" spellcheck="false"; }
                p { "If you lose access to your inbox, choose Recover account on the sign-in page. You can generate a replacement code in Settings." }
                a.button href="/settings" { "I’ve saved my code" }
            }
        },
    ))
}

async fn replace_code(db: &PgPool, owner: Uuid, session: &str, code: &str) -> AppResult<bool> {
    let mut tx = db.begin().await?;
    // Recovery and code generation always lock the account before its credential.
    let account = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND identity_current FOR UPDATE",
    )
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    if account.is_none() {
        return Ok(false);
    }
    // Recheck the specific session under the account lock in case another recovery just
    // revoked it after the Auth extractor ran. Lock it against a concurrent sign out too.
    let fresh_session = sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM user_sessions WHERE user_id=$1 AND token_hash=$2
         AND expires_at > clock_timestamp() AND created_at > clock_timestamp() - interval '15 minutes' FOR UPDATE",
    )
    .bind(owner)
    .bind(digest(session))
    .fetch_optional(&mut *tx)
    .await?;
    if fresh_session.is_none() {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO account_recovery (user_id, code_hash) VALUES ($1,$2)
         ON CONFLICT (user_id) DO UPDATE SET code_hash=EXCLUDED.code_hash,
         challenge_hash=NULL, replacement_email=NULL, expires_at=NULL",
    )
    .bind(owner)
    .bind(digest(code))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

fn nonce_page(
    title: &str,
    headers: &HeaderMap,
    state: &AppState,
    body: impl FnOnce(&str) -> Markup,
) -> Response {
    let nonce = form_nonce(headers, state.config.production);
    let mut response = public_page(title, body(&nonce)).into_response();
    put_nonce(&mut response, state.config.production, &nonce);
    response
}

async fn start(State(state): State<AppState>, headers: HeaderMap) -> Response {
    nonce_page("Recover your account", &headers, &state, |nonce| {
        html! {
            p { "Enter your saved recovery code and an email address you can open. After you confirm that inbox, you’ll use it to sign in." }
            form.stack method="post" action="/recover" {
                input type="hidden" name="nonce" value=(nonce);
                label.field { "Recovery code" input required name="code" type="password" autocomplete="off" maxlength="64"; }
                label.field { "New email address" input required name="email" type="email" autocomplete="email" maxlength="254"; }
                button type="submit" { "Send verification link" }
            }
            a href="/login" { "Back to sign in" }
        }
    })
}

#[derive(Deserialize)]
struct RequestForm {
    nonce: String,
    code: String,
    email: String,
}

async fn request(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Form(form): Form<RequestForm>,
) -> AppResult<Response> {
    check_nonce(&headers, state.config.production, &form.nonce)?;
    let email = normalized_email(&form.email)
        .ok_or_else(|| AppError::BadRequest("Enter a valid email address.".into()))?;
    let peer = peer.map(|Extension(ConnectInfo(address))| address);
    let secret = token();
    let mut tx = state.db.begin().await?;
    let mut allowed = budget(&mut tx, "recovery/global", 400).await?;
    if allowed && let Some(address) = peer {
        allowed = budget(&mut tx, &format!("recovery/peer/{}", address.ip()), 40).await?;
    }
    if allowed {
        allowed = budget(
            &mut tx,
            &format!("recovery/mail/{}", hex::encode(digest(&email))),
            4,
        )
        .await?;
    }
    let code = form.code.trim();
    let issued = if allowed && valid_token(code) {
        stage_challenge(&mut tx, code, &email, &secret).await?
    } else {
        false
    };
    tx.commit().await?;
    let url = format!(
        "{}/recover/confirm?token={secret}",
        state.config.base_url.trim_end_matches('/')
    );
    if dev_links(&state, &headers, peer) {
        // A dummy link is shown for invalid or limited requests as well. Its GET
        // landing is identical and never consumes a real recovery credential.
        return Ok(public_page(
            "Check your recovery link",
            html! {
                p { "Local development is displaying the verification link for this request." }
                a.button href=(url) { "Open local recovery link" }
            },
        )
        .into_response());
    }
    if issued && mail_link(&state, &email, &url).await.is_err() {
        tracing::warn!("Recovery verification mail delivery failed");
        sqlx::query("UPDATE account_recovery SET challenge_hash=NULL,replacement_email=NULL,expires_at=NULL WHERE challenge_hash=$1")
            .bind(digest(&secret)).execute(&state.db).await?;
    }
    Ok(Redirect::to("/recover/check").into_response())
}

async fn stage_challenge(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    code: &str,
    email: &str,
    secret: &str,
) -> AppResult<bool> {
    let update = sqlx::query(
        "UPDATE account_recovery r SET challenge_hash=$1,replacement_email=$2,
         expires_at=now()+interval '15 minutes' FROM users u WHERE r.code_hash=$3
         AND u.id=r.user_id AND u.identity_current AND u.email<>$2",
    )
    .bind(digest(secret))
    .bind(email)
    .bind(digest(code))
    .execute(&mut **tx)
    .await?;
    Ok(update.rows_affected() == 1)
}

async fn check_inbox() -> Html<String> {
    public_page(
        "Check your new inbox",
        html! {
            p { "If your recovery code is valid, a verification link will arrive at the new address. The link expires in 15 minutes." }
            a href="/recover" { "Try recovery again" }
        },
    )
}

#[derive(Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: String,
}

async fn landing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    nonce_page("Confirm account recovery", &headers, &state, |nonce| {
        html! {
            p { "Continuing changes your workspace’s sign-in email to this inbox and signs out every existing session. Continue only if you requested this change." }
            form.stack method="post" action="/recover/confirm" {
                input type="hidden" name="nonce" value=(nonce);
                input type="hidden" name="token" value=(if valid_token(&query.token) { query.token.as_str() } else { "" });
                button type="submit" { "Change email and sign in" }
            }
            a href="/login" { "Back to sign in" }
        }
    })
}

#[derive(Deserialize)]
struct ConfirmForm {
    token: String,
    nonce: String,
}

async fn confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ConfirmForm>,
) -> AppResult<Response> {
    check_nonce(&headers, state.config.production, &form.nonce)?;
    let session = apply_recovery(&state.db, &form.token).await?.ok_or_else(|| {
        AppError::BadRequest("This recovery link cannot be used. Request a new link with your code and an unused email address.".into())
    })?;
    let mut response = Redirect::to("/settings?result=recovered").into_response();
    put_session(&mut response, state.config.production, &session);
    Ok(response)
}

async fn apply_recovery(db: &PgPool, raw: &str) -> AppResult<Option<String>> {
    if !valid_token(raw) {
        return Ok(None);
    }
    let hash = digest(raw);
    let mut tx = db.begin().await?;
    let candidate = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT r.user_id,u.email,r.replacement_email FROM account_recovery r JOIN users u ON u.id=r.user_id
         WHERE u.identity_current AND r.challenge_hash=$1 AND r.expires_at>clock_timestamp()",
    ).bind(&hash).fetch_optional(&mut *tx).await?;
    let Some((owner, old, new)) = candidate else {
        return Ok(None);
    };
    // Sign-in, deletion and recovery serialize by address. Lock both addresses in
    // the same order if two owners simultaneously request each other's address.
    let mut addresses = [old.as_str(), new.as_str()];
    addresses.sort_unstable();
    for email in addresses {
        lock_email(&mut tx, email).await?;
    }
    let current = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND email=$2 AND identity_current FOR UPDATE",
    )
    .bind(owner)
    .bind(&old)
    .fetch_optional(&mut *tx)
    .await?;
    if current.is_none() {
        return Ok(None);
    }
    // Check the claim again under the locks. Code rotation, newer challenges,
    // expiry and a concurrent successful confirmation all invalidate this request.
    let consumed = sqlx::query(
        "DELETE FROM account_recovery WHERE user_id=$1 AND challenge_hash=$2
         AND replacement_email=$3 AND expires_at>clock_timestamp()",
    )
    .bind(owner)
    .bind(&hash)
    .bind(&new)
    .execute(&mut *tx)
    .await?;
    if consumed.rows_affected() != 1 {
        return Ok(None);
    }
    let changed = sqlx::query("UPDATE users SET email=$1 WHERE id=$2")
        .bind(&new)
        .bind(owner)
        .execute(&mut *tx)
        .await;
    match changed {
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            tx.rollback().await?;
            return Ok(None);
        }
        result => {
            result?;
        }
    }
    sqlx::query("DELETE FROM login_challenges WHERE email IN ($1,$2)")
        .bind(&old)
        .bind(&new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM user_sessions WHERE user_id=$1")
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO auth_recovery_events(user_id) VALUES($1)
         ON CONFLICT(user_id) DO UPDATE SET event_id=EXCLUDED.event_id,recovered_at=EXCLUDED.recovered_at",
    ).bind(owner).execute(&mut *tx).await?;
    let session = new_session(&mut tx, owner).await?;
    tx.commit().await?;
    Ok(Some(session))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn user(db: &PgPool, email: &str) -> (Uuid, String) {
        let id = sqlx::query_scalar("INSERT INTO users(email) VALUES($1) RETURNING id")
            .bind(email)
            .fetch_one(db)
            .await
            .unwrap();
        let mut tx = db.begin().await.unwrap();
        let session = new_session(&mut tx, id).await.unwrap();
        tx.commit().await.unwrap();
        (id, session)
    }

    async fn stage(db: &PgPool, code: &str, email: &str) -> Option<String> {
        let secret = token();
        let mut tx = db.begin().await.unwrap();
        let staged = stage_challenge(&mut tx, code, email, &secret)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        staged.then_some(secret)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn verification_landing_is_passive_and_confirmation_requires_the_browser_nonce(
        db: PgPool,
    ) {
        use axum::{
            body::Body,
            http::{Request, StatusCode, header},
        };
        use tower::ServiceExt;

        let (id, session) = user(&db, "before@example.test").await;
        let code = token();
        assert!(replace_code(&db, id, &session, &code).await.unwrap());
        let secret = stage(&db, &code, "after@example.test").await.unwrap();
        let state = AppState::for_test(db.clone());
        let production = state.config.production;
        let app = routes().with_state(state);
        let read = || {
            Request::get(format!("/recover/confirm?token={secret}"))
                .body(Body::empty())
                .unwrap()
        };
        let first = app.clone().oneshot(read()).await.unwrap();
        let second = app.clone().oneshot(read()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second.headers().get_all(header::SET_COOKIE).iter().count(),
            1
        );
        let cookie = second.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let (name, nonce) = cookie.split_once('=').unwrap();
        assert_eq!(name, super::super::nonce_name(production));
        let submit = |value: &str| {
            Request::post("/recover/confirm")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie)
                .body(Body::from(format!("token={secret}&nonce={value}")))
                .unwrap()
        };
        assert!(
            app.clone()
                .oneshot(submit(&token()))
                .await
                .unwrap()
                .status()
                .is_client_error()
        );
        assert_eq!(
            super::super::session(&db, &session)
                .await
                .unwrap()
                .unwrap()
                .email,
            "before@example.test"
        );
        let confirmed = app.clone().oneshot(submit(nonce)).await.unwrap();
        assert_eq!(confirmed.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            confirmed.headers()[header::LOCATION],
            "/settings?result=recovered"
        );
        assert!(
            confirmed.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .starts_with(super::super::session_name(production))
        );
        assert!(
            app.oneshot(submit(nonce))
                .await
                .unwrap()
                .status()
                .is_client_error()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recovery_requires_both_proofs_and_revokes_old_access(db: PgPool) {
        let old = "old@example.test";
        let new = "new@example.test";
        let (id, first_session) = user(&db, old).await;
        let (other, other_session) = user(&db, "other@example.test").await;
        let code = token();
        assert!(!replace_code(&db, id, &other_session, &code).await.unwrap());
        assert!(replace_code(&db, id, &first_session, &code).await.unwrap());
        assert!(stage(&db, &token(), new).await.is_none());
        let outdated = stage(&db, &code, new).await.unwrap();
        let rotated_code = token();
        assert!(
            replace_code(&db, id, &first_session, &rotated_code)
                .await
                .unwrap()
        );
        assert!(apply_recovery(&db, &outdated).await.unwrap().is_none());
        assert!(stage(&db, &code, new).await.is_none());
        let expires = stage(&db, &rotated_code, new).await.unwrap();
        sqlx::query(
            "UPDATE account_recovery SET expires_at=now()-interval '1 second' WHERE user_id=$1",
        )
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        assert!(apply_recovery(&db, &expires).await.unwrap().is_none());
        let secret = stage(&db, &rotated_code, new).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        let second_session = new_session(&mut tx, id).await.unwrap();
        tx.commit().await.unwrap();
        for email in [old, new] {
            sqlx::query("INSERT INTO login_challenges(token_hash,email,return_to,expires_at) VALUES($1,$2,'/',now()+interval '1 hour')")
                .bind(digest(&token())).bind(email).execute(&db).await.unwrap();
        }
        let (a, b) = tokio::join!(apply_recovery(&db, &secret), apply_recovery(&db, &secret));
        let issued: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
        assert_eq!(issued.len(), 1);
        let recovered = super::super::session(&db, &issued[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.user_id, id);
        assert_eq!(recovered.email, new);
        let recorded: Option<Uuid> =
            sqlx::query_scalar("SELECT event_id FROM auth_recovery_events WHERE user_id=$1")
                .bind(id)
                .fetch_optional(&db)
                .await
                .unwrap();
        assert!(recorded.is_some());
        assert!(
            super::super::session(&db, &first_session)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            super::super::session(&db, &second_session)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            super::super::session(&db, &other_session)
                .await
                .unwrap()
                .unwrap()
                .user_id,
            other
        );
        assert!(
            stage(&db, &rotated_code, "another@example.test")
                .await
                .is_none()
        );
        let leftovers = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM login_challenges WHERE email IN ($1,$2)",
        )
        .bind(old)
        .bind(new)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(leftovers, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn occupied_replacement_address_leaves_both_accounts_intact(db: PgPool) {
        let (id, session) = user(&db, "recovering@example.test").await;
        let (other, other_session) = user(&db, "occupied@example.test").await;
        let code = token();
        assert!(replace_code(&db, id, &session, &code).await.unwrap());
        let occupied = stage(&db, &code, "occupied@example.test").await.unwrap();
        assert!(apply_recovery(&db, &occupied).await.unwrap().is_none());
        let old = super::super::session(&db, &session).await.unwrap().unwrap();
        assert_eq!(old.email, "recovering@example.test");
        assert_eq!(
            super::super::session(&db, &other_session)
                .await
                .unwrap()
                .unwrap()
                .user_id,
            other
        );
        assert!(stage(&db, &code, "available@example.test").await.is_some());
        sqlx::query(
            "UPDATE user_sessions SET created_at=now()-interval '1 hour' WHERE token_hash=$1",
        )
        .bind(digest(&session))
        .execute(&db)
        .await
        .unwrap();
        assert!(!replace_code(&db, id, &session, &token()).await.unwrap());
    }
}
