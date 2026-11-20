use super::credentials::{self, budget, cookie, digest, form_nonce, session_cookie_raw};
use super::{
    Auth, CsrfForm, check_nonce, new_session, nonce_name, public_page, put_nonce, put_session,
    set_cookie, token,
};
use crate::{AppError, AppResult, AppState};
use axum::{
    Form, Router,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::{Postgres, Transaction};
use std::net::SocketAddr;
use uuid::Uuid;

const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const ISSUE_WORKSPACE: &str = "/issues";

fn device_cookie(production: bool) -> &'static str {
    if production {
        "__Host-workspace_device"
    } else {
        "workspace_device"
    }
}

// Derive the display code from the private device cookie so a reloaded
// desktop can display it without storing either credential in plaintext.
fn display_code(secret: &str) -> String {
    let bytes = digest(&format!("working-set/desktop-code/v1/{secret}"));
    let mut code = String::with_capacity(9);
    for (i, byte) in bytes.iter().take(8).enumerate() {
        if i == 4 {
            code.push('-');
        }
        code.push(CODE_ALPHABET[(byte & 31) as usize] as char);
    }
    code
}

pub(super) fn valid_code(code: &str) -> bool {
    code.len() == 9
        && code.bytes().enumerate().all(|(i, b)| {
            if i == 4 {
                b == b'-'
            } else {
                CODE_ALPHABET.contains(&b)
            }
        })
}

fn nonce_page(
    title: &str,
    headers: &HeaderMap,
    production: bool,
    body: impl FnOnce(&str) -> Markup,
) -> Response {
    let nonce = form_nonce(headers, production);
    let mut response = public_page(title, body(&nonce)).into_response();
    put_nonce(&mut response, production, &nonce);
    response
}

fn unavailable() -> Response {
    (
        StatusCode::GONE,
        public_page(
            "Pairing request unavailable",
            html! {
                p { "This code expired or has already been used. Start again in the desktop app." }
                a href="/desktop/start" { "Return to desktop sign-in" }
            },
        ),
    )
        .into_response()
}

fn too_many() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        public_page(
            "Try again later",
            html! {
                p { "Too many pairing attempts. Try again in 15 minutes." }
            },
        ),
    )
        .into_response()
}

enum Approval {
    Pending,
    Confirm { owner: Uuid, email: String },
}

async fn approval(state: &AppState, secret: &str) -> AppResult<Option<Approval>> {
    let request: Option<(bool, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT d.approved_by IS NOT NULL,u.id,u.email FROM desktop_devices d
         LEFT JOIN user_sessions s ON s.token_hash=d.approved_by AND s.expires_at>now()
         LEFT JOIN users u ON u.id=s.user_id AND u.identity_current
         WHERE d.device_hash=$1 AND d.expires_at>now()",
    )
    .bind(digest(secret))
    .fetch_optional(&state.db)
    .await?;
    Ok(match request {
        Some((false, _, _)) => Some(Approval::Pending),
        Some((true, Some(owner), Some(email))) => Some(Approval::Confirm { owner, email }),
        _ => None,
    })
}

async fn start(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let production = state.config.production;
    if let Some(raw) = session_cookie_raw(&headers, production)
        && credentials::session(&state.db, raw).await?.is_some()
    {
        return Ok(Redirect::to(ISSUE_WORKSPACE).into_response());
    }
    if let Some(secret) = cookie(&headers, device_cookie(production))
        && approval(&state, secret).await?.is_some()
    {
        return Ok(Redirect::to("/desktop/check").into_response());
    }
    Ok(nonce_page(
        "Sign in to Working Set",
        &headers,
        production,
        |nonce| {
            html! {
                p { "Pair this app with your account on " strong { (state.config.base_url) } "." }
                form method="post" action="/desktop/start" {
                    input type="hidden" name="csrf" value=(nonce);
                    button type="submit" { "Pair this desktop" }
                }
                @if !production {
                    p { a href="/login?next=%2Fissues" { "Sign in directly in this app" } }
                }
            }
        },
    ))
}

async fn begin(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let production = state.config.production;
    check_nonce(&headers, production, &form.csrf)?;
    let mut tx = state.db.begin().await?;
    if !budget(&mut tx, &format!("device/start/peer/{}", peer.0.ip()), 40).await?
        || !budget(&mut tx, "device/start/all", 600).await?
    {
        tx.commit().await?;
        return Ok(too_many());
    }
    credentials::purge_expired(&mut tx).await?;
    if let Some(old) = cookie(&headers, device_cookie(production)) {
        sqlx::query("DELETE FROM desktop_devices WHERE device_hash=$1")
            .bind(digest(old))
            .execute(&mut *tx)
            .await?;
    }
    // A collision on the short public code must never replace another device.
    let secret = loop {
        let secret = token();
        let created = sqlx::query(
            "INSERT INTO desktop_devices(device_hash,code_hash) VALUES($1,$2)
            ON CONFLICT DO NOTHING",
        )
        .bind(digest(&secret))
        .bind(digest(&display_code(&secret)))
        .execute(&mut *tx)
        .await?;
        if created.rows_affected() == 1 {
            break secret;
        }
    };
    tx.commit().await?;
    let mut response = Redirect::to("/desktop/check").into_response();
    set_cookie(
        &mut response,
        device_cookie(production),
        &secret,
        600,
        production,
    );
    Ok(response)
}

async fn check(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let production = state.config.production;
    let Some(secret) = cookie(&headers, device_cookie(production)) else {
        return Ok(unavailable());
    };
    let Some(request) = approval(&state, secret).await? else {
        return Ok(unavailable());
    };
    let title = match request {
        Approval::Pending => "Pair your desktop",
        Approval::Confirm { .. } => "Confirm your account",
    };
    Ok(nonce_page(title, &headers, production, |nonce| {
        html! {
            @if let Approval::Confirm { owner, email } = request {
                p { "Continue on this desktop as " strong { (email) } "?" }
                p { (state.config.base_url) }
                form method="post" action="/desktop/finish" {
                    input type="hidden" name="csrf" value=(nonce);
                    input type="hidden" name="account" value=(owner);
                    button type="submit" { "Continue to workspace" }
                }
            } @else {
                @let code = display_code(secret);
                p { "Open your browser and match this code before approving. Codes are valid for 10 minutes." }
                p { strong { code { (code) } } }
                p { a.button href=(format!("/desktop/authorize?code={code}")) target="_blank" rel="noopener noreferrer" { "Open in browser" } }
                p { a href="/desktop/check" { "Check approval" } }
            }
            form method="post" action="/desktop/start" {
                input type="hidden" name="csrf" value=(nonce);
                button.secondary type="submit" { "Start over" }
            }
        }
    }))
}

#[derive(Deserialize)]
struct CodeQuery {
    code: String,
}

async fn authorize(
    auth: Auth,
    State(state): State<AppState>,
    Query(query): Query<CodeQuery>,
) -> Response {
    if !valid_code(&query.code) {
        return unavailable();
    }
    public_page("Approve desktop sign-in", html! {
        p { "Approve only if this code matches the code in the Working Set desktop app you opened." }
        p { strong { code { (query.code) } } }
        dl {
            dt { "Account" } dd { (auth.email) }
            dt { "Server" } dd { (state.config.base_url) }
        }
        form method="post" action="/desktop/authorize" {
            (crate::ui::csrf(&auth))
            input type="hidden" name="code" value=(query.code);
            button type="submit" { "Approve desktop" }
        }
    }).into_response()
}

#[derive(Deserialize)]
struct ApprovalForm {
    csrf: String,
    code: String,
}

// Follow account recovery's lock order. Recheck the browser session under
// lock because Auth may have been extracted before a concurrent sign-out.
pub(super) async fn lock_browser(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    raw: &str,
) -> AppResult<()> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND identity_current FOR UPDATE",
    )
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::Forbidden)?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM user_sessions WHERE user_id=$1 AND token_hash=$2
         AND expires_at>clock_timestamp() FOR SHARE",
    )
    .bind(owner)
    .bind(digest(raw))
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::Forbidden)?;
    Ok(())
}

async fn approve(
    auth: Auth,
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<ApprovalForm>,
) -> AppResult<Response> {
    auth.check_csrf(&form.csrf)?;
    let session =
        session_cookie_raw(&headers, state.config.production).ok_or(AppError::Forbidden)?;
    let mut tx = state.db.begin().await?;
    if !budget(
        &mut tx,
        &format!("device/approval/user/{}", auth.user_id),
        30,
    )
    .await?
        || !budget(
            &mut tx,
            &format!("device/approval/peer/{}", peer.0.ip()),
            120,
        )
        .await?
        || !budget(&mut tx, "device/approval/all", 1500).await?
    {
        tx.commit().await?;
        return Ok(too_many());
    }
    lock_browser(&mut tx, auth.user_id, session).await?;
    let granted = valid_code(&form.code)
        && sqlx::query(
            "UPDATE desktop_devices SET approved_by=$2
             WHERE code_hash=$1 AND approved_by IS NULL AND expires_at>clock_timestamp()",
        )
        .bind(digest(&form.code))
        .bind(digest(session))
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
    tx.commit().await?;
    if !granted {
        return Ok(unavailable());
    }
    Ok(public_page("Desktop approved", html! {
        p { "Return to Working Set on your desktop, choose Check approval, and confirm " strong { (auth.email) } "." }
    }).into_response())
}

#[derive(Deserialize)]
struct FinishForm {
    csrf: String,
    account: Uuid,
}

async fn finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<FinishForm>,
) -> AppResult<Response> {
    let production = state.config.production;
    check_nonce(&headers, production, &form.csrf)?;
    let Some(secret) = cookie(&headers, device_cookie(production)) else {
        return Ok(unavailable());
    };
    let device_hash = digest(secret);
    let mut tx = state.db.begin().await?;
    let owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT u.id FROM desktop_devices d JOIN user_sessions s ON s.token_hash=d.approved_by
         JOIN users u ON u.id=s.user_id WHERE d.device_hash=$1 AND u.id=$2 AND u.identity_current
         AND d.expires_at>clock_timestamp() AND s.expires_at>clock_timestamp() FOR UPDATE OF u",
    )
    .bind(&device_hash)
    .bind(form.account)
    .fetch_optional(&mut *tx)
    .await?;
    if owner.is_none() {
        return Ok(unavailable());
    }
    // Lock the browser session against sign-out before consuming the request.
    // Inherit its verified-email time and expiry so pairing cannot
    // satisfy a fresh-email requirement or extend a session's lifetime.
    let lifetime: Option<(DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT s.created_at,s.expires_at FROM user_sessions s JOIN desktop_devices d ON d.approved_by=s.token_hash
         WHERE d.device_hash=$1 AND s.user_id=$2 AND d.expires_at>clock_timestamp()
         AND s.expires_at>clock_timestamp() FOR SHARE OF s"
    ).bind(&device_hash).bind(form.account).fetch_optional(&mut *tx).await?;
    let Some((verified, expires)) = lifetime else {
        return Ok(unavailable());
    };
    let consumed = sqlx::query(
        "DELETE FROM desktop_devices WHERE device_hash=$1 AND expires_at>clock_timestamp()",
    )
    .bind(device_hash)
    .execute(&mut *tx)
    .await?;
    if consumed.rows_affected() != 1 {
        return Ok(unavailable());
    }
    if let Some(previous) = session_cookie_raw(&headers, production) {
        sqlx::query("DELETE FROM user_sessions WHERE token_hash=$1")
            .bind(digest(previous))
            .execute(&mut *tx)
            .await?;
    }
    let raw = new_session(&mut tx, form.account).await?;
    sqlx::query("UPDATE user_sessions SET created_at=$1,expires_at=$2 WHERE token_hash=$3")
        .bind(verified)
        .bind(expires)
        .bind(digest(&raw))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut response = Redirect::to(ISSUE_WORKSPACE).into_response();
    put_session(&mut response, production, &raw);
    set_cookie(&mut response, device_cookie(production), "", 0, production);
    set_cookie(&mut response, nonce_name(production), "", 0, production);
    Ok(response)
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/desktop/start", get(start).post(begin))
        .route("/desktop/check", get(check))
        .route("/desktop/authorize", get(authorize).post(approve))
        .route("/desktop/finish", post(finish))
}
