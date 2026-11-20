use crate::{AppError, AppResult, AppState};
use axum::{
    Form, Router,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{DOCTYPE, Markup, html};
use reqwest::Url;
use serde::Deserialize;
use std::{env, net::SocketAddr};
use uuid::Uuid;

mod credentials;
mod delivery;
mod desktop;
#[cfg(test)]
mod desktop_tests;
mod recovery;
#[cfg(test)]
mod tests;

pub use credentials::Auth;
#[cfg(test)]
use credentials::session;
use credentials::{
    budget, check_nonce, cleanup_cookie_name, cleanup_owner, cookie, digest, form_nonce,
    lock_email, new_session, nonce_name, normalized_email, purge_expired, put_nonce, put_session,
    redeem, safe_next, session_cookie_raw, session_name, set_cookie, token, valid_token,
};
use delivery::{dev_links, mail_link};

// This marker selects the native handoff UI. Authentication uses the session.
pub(crate) fn is_desktop_client(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|ua| {
            ua.split_ascii_whitespace().any(|product| {
                product.starts_with("RatioDesktop/") || product.starts_with("WorkingSetDesktop/")
            })
        })
}

fn public_page(title: &str, body: Markup) -> Html<String> {
    Html(html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="referrer" content="strict-origin";
                title { (title) " · Ratio" }
                link rel="stylesheet" href="/assets/app.css";
                link rel="stylesheet" href="/assets/support.css";
                link rel="icon" type="image/svg+xml" href="/assets/mark.svg";
                script defer src="/assets/workspace.js" {}
            }
            body.auth-page {
                main.auth-shell {
                    div.auth-card {
                        div.auth-brandbar { a.auth-brand href="/welcome" aria-label="Ratio home" { img src="/assets/mark.svg" width="26" height="26" alt=""; "Ratio" } (crate::ui::desktop_controls()) }
                        h1 { (title) }
                        (body)
                    }
                }
            }
        }
    }.into_string())
}

#[derive(Deserialize, Default)]
struct LoginQuery {
    demo: Option<bool>,
    next: Option<String>,
    result: Option<String>,
}
#[derive(Deserialize)]
struct LoginForm {
    email: String,
    csrf: String,
    #[serde(default)]
    next: String,
}
#[derive(Deserialize)]
struct ChallengeQuery {
    #[serde(default)]
    token: String,
}
#[derive(Deserialize)]
struct VerifyForm {
    token: String,
    csrf: String,
}
#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
}
#[derive(Deserialize)]
struct DeleteForm {
    csrf: String,
    email: String,
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let nonce = form_nonce(&headers, state.config.production);
    let next = safe_next(query.next.as_deref().unwrap_or("/"));
    let cleanup = cleanup_owner(&headers, state.config.production);
    let mut response = public_page(
        "Sign in to your workspace",
        html! {
        @if let Some(owner)=cleanup {
            script src="/assets/note-editor.js" data-clear-note-drafts=(owner) defer {}
        }
        @match query.result.as_deref().filter(|_|cleanup.is_some()){
            Some("deleted")=>{p.status-note role="status" {"Your workspace was deleted."}}
            Some("signed-out")=>{p.status-note role="status" {"You’re signed out."}}
            _=>{}
        }
        p {"Your first sign-in creates a private workspace."}
        form.auth-form method="post" action="/login" {
            input type="hidden" name="csrf" value=(nonce);
            input type="hidden" name="next" value=(next);
            label.field { "Email address" input name="email" type="email" autocomplete="email" autocapitalize="none" spellcheck="false" maxlength="254" value=[(query.demo == Some(true) && !state.config.production).then_some(crate::sample::DEMO_EMAIL)] required; }
            button.action-primary type="submit" {"Email me a sign-in link"}
        }
        div.auth-links {
            a href="/recover" {"Recover your account"}
            @if is_desktop_client(&headers) { a href="/desktop/start" {"Pair this desktop using your browser"} }
        }
    }).into_response();
    if cleanup.is_some() {
        set_cookie(
            &mut response,
            cleanup_cookie_name(state.config.production),
            "",
            0,
            state.config.production,
        );
    }
    put_nonce(&mut response, state.config.production, &nonce);
    response
}

async fn request_link(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> AppResult<Response> {
    check_nonce(&headers, state.config.production, &form.csrf)?;
    let email = normalized_email(&form.email)
        .ok_or_else(|| AppError::BadRequest("Enter a valid email address.".into()))?;
    let local = dev_links(&state, &headers, Some(peer.0));
    let raw = token();
    let mut tx = state.db.begin().await?;
    purge_expired(&mut tx).await?;
    let allowed = budget(&mut tx, "issue/all", 300).await?
        && budget(&mut tx, &format!("issue/peer/{}", peer.0.ip()), 120).await?
        && budget(&mut tx, &format!("issue/email/{email}"), 5).await?;
    let issued = if allowed {
        lock_email(&mut tx, &email).await?;
        sqlx::query(
            "INSERT INTO login_challenges(token_hash,email,return_to) SELECT $1,$2,$3
             WHERE NOT EXISTS(SELECT 1 FROM users WHERE email=$2 AND NOT identity_current)",
        )
        .bind(digest(&raw))
        .bind(&email)
        .bind(safe_next(&form.next))
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1
    } else {
        false
    };
    tx.commit().await?;
    let mut response = Redirect::to("/login/check").into_response();
    if local {
        set_cookie(
            &mut response,
            "workspace_dev_preview",
            if issued { &raw } else { "" },
            if issued { 900 } else { 0 },
            false,
        );
    } else if issued {
        let url = format!(
            "{}/login/confirm?token={raw}",
            state.config.base_url.trim_end_matches('/')
        );
        if let Err(err) = mail_link(&state, &email, &url).await {
            sqlx::query("DELETE FROM login_challenges WHERE token_hash=$1")
                .bind(digest(&raw))
                .execute(&state.db)
                .await?;
            return Err(err);
        }
    }
    Ok(response)
}

async fn check_inbox(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Html<String> {
    let preview = dev_links(&state, &headers, Some(peer.0))
        .then(|| cookie(&headers, "workspace_dev_preview"))
        .flatten();
    public_page(
        "Check your email",
        html! {
            p {"If a link can be sent to that address, it will arrive shortly. Sign-in links expire after 15 minutes."}
            @if let Some(raw)=preview {
                div.auth-preview {
                    p { "Local development email preview" }
                    a href=(format!("/login/confirm?token={raw}")) {"Open local sign-in link"}
                }
            }
            a href="/login" {"Use another email address"}
        },
    )
}

fn invalid_link() -> Response {
    (
        StatusCode::GONE,
        public_page(
            "This link has expired",
            html! {
                p {"Sign-in links work once and expire after 15 minutes."}
                a.button.action-primary href="/login" {"Request a new sign-in link"}
            },
        ),
    )
        .into_response()
}

async fn confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ChallengeQuery>,
) -> AppResult<Response> {
    if !valid_token(&query.token) {
        return Ok(invalid_link());
    }
    let email: Option<String> = sqlx::query_scalar(
        "SELECT email FROM login_challenges WHERE token_hash=$1 AND expires_at>now()",
    )
    .bind(digest(&query.token))
    .fetch_optional(&state.db)
    .await?;
    let Some(email) = email else {
        return Ok(invalid_link());
    };
    let nonce = form_nonce(&headers, state.config.production);
    let mut response = public_page(
        "Confirm sign-in",
        html! {
            p {"Continue to the private workspace for " strong {(email)} "."}
            form.auth-form action="/login/confirm" method="post" {
                input type="hidden" name="token" value=(query.token);
                input type="hidden" name="csrf" value=(nonce);
                button.action-primary type="submit" {"Continue to workspace"}
            }
            a href="/login" {"Use another email address"}
        },
    )
    .into_response();
    put_nonce(&mut response, state.config.production, &nonce);
    Ok(response)
}

async fn verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<VerifyForm>,
) -> AppResult<Response> {
    check_nonce(&headers, state.config.production, &form.csrf)?;
    let Some((raw, path)) = redeem(
        &state.db,
        &form.token,
        session_cookie_raw(&headers, state.config.production),
    )
    .await?
    else {
        return Ok(invalid_link());
    };
    let mut response = Redirect::to(&path).into_response();
    put_session(&mut response, state.config.production, &raw);
    set_cookie(
        &mut response,
        nonce_name(state.config.production),
        "",
        0,
        state.config.production,
    );
    if !state.config.production {
        set_cookie(&mut response, "workspace_dev_preview", "", 0, false)
    }
    Ok(response)
}

fn signout_response(production: bool, owner: Uuid, path: &str) -> Response {
    let mut response = Redirect::to(path).into_response();
    set_cookie(
        &mut response,
        cleanup_cookie_name(production),
        &owner.to_string(),
        300,
        production,
    );
    set_cookie(&mut response, session_name(production), "", 0, production);
    set_cookie(&mut response, nonce_name(production), "", 0, production);
    if !production {
        set_cookie(&mut response, "workspace_dev_preview", "", 0, false)
    }
    response
}

async fn logout(
    auth: Auth,
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    auth.check_csrf(&form.csrf)?;
    let raw = session_cookie_raw(&headers, state.config.production).ok_or(AppError::Forbidden)?;
    sqlx::query("DELETE FROM user_sessions WHERE token_hash=$1 AND user_id=$2")
        .bind(digest(raw))
        .bind(auth.user_id)
        .execute(&state.db)
        .await?;
    Ok(signout_response(
        state.config.production,
        auth.user_id,
        "/login?result=signed-out",
    ))
}

async fn revoke(
    auth: Auth,
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let raw = session_cookie_raw(&headers, state.config.production).ok_or(AppError::Forbidden)?;
    let mut tx = state.db.begin().await?;
    desktop::lock_browser(&mut tx, auth.user_id, raw).await?;
    // An explicit sign-out of other devices also cancels approvals that have
    // not been exchanged for a session, including those made by this browser.
    sqlx::query(
        "DELETE FROM desktop_devices d USING user_sessions s
                 WHERE d.approved_by=s.token_hash AND s.user_id=$1",
    )
    .bind(auth.user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM user_sessions WHERE user_id=$1 AND token_hash<>$2")
        .bind(auth.user_id)
        .bind(digest(raw))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Redirect::to("/settings?result=revoked"))
}

async fn settings(
    auth: Auth,
    State(state): State<AppState>,
    Query(query): Query<LoginQuery>,
) -> AppResult<Html<String>> {
    let feedback = match query.result.as_deref() {
        Some("revoked") => SettingsFeedback::SessionsRevoked,
        Some("timezone") => SettingsFeedback::TimezoneSaved,
        Some("recovered") => SettingsFeedback::EmailRecovered,
        _ => SettingsFeedback::None,
    };
    settings_screen(&auth, &state, feedback).await
}

enum SettingsFeedback<'a> {
    None,
    SessionsRevoked,
    TimezoneSaved,
    EmailRecovered,
    InvalidTimezone(&'a str),
    InvalidDeletionEmail,
}

async fn settings_screen(
    auth: &Auth,
    state: &AppState,
    feedback: SettingsFeedback<'_>,
) -> AppResult<Html<String>> {
    let (plan, timezone, sessions, pairing): (String, String, i64, bool) = sqlx::query_as(
        "SELECT plan, timezone, (SELECT count(*) FROM user_sessions WHERE user_id=u.id
         AND expires_at>now()), EXISTS(SELECT 1 FROM desktop_devices d JOIN user_sessions s
         ON s.token_hash=d.approved_by WHERE s.user_id=u.id AND d.expires_at>now() AND s.expires_at>now())
         FROM users u WHERE id=$1",
    )
    .bind(auth.user_id)
    .fetch_one(&state.db)
    .await?;
    let support = env::var("SUPPORT_EMAIL")
        .ok()
        .and_then(|v| normalized_email(&v));
    let status = env::var("STATUS_URL")
        .ok()
        .and_then(|v| Url::parse(&v).ok())
        .filter(|u| {
            u.scheme() == "https"
                && u.has_host()
                && u.username().is_empty()
                && u.password().is_none()
        });
    let zone_error = matches!(feedback, SettingsFeedback::InvalidTimezone(_));
    let delete_error = matches!(feedback, SettingsFeedback::InvalidDeletionEmail);
    let zone_value = match feedback {
        SettingsFeedback::InvalidTimezone(value) => value,
        _ => &timezone,
    };
    Ok(crate::ui::section_page(
        auth,
        "Settings",
        "settings",
        html! {
            header.view-head { h1 { "Settings" } }
            div.settings-sheet {
                @match feedback {
                    SettingsFeedback::SessionsRevoked => { p.status-note role="status" { "Other sessions were signed out." } }
                    SettingsFeedback::TimezoneSaved => { p.status-note role="status" { "Time zone saved." } }
                    SettingsFeedback::EmailRecovered => { p.status-note role="status" { "Sign-in email updated. If you use a paid plan, review the email for receipts in " a href="/billing" {"Billing"} "." } }
                    _ => {}
                }
                section.settings-section aria-labelledby="workspace-settings" {
                    h2 #workspace-settings { "Workspace" }
                    dl.account-properties {
                        div.account-property { dt { "Sign-in email" } dd { (auth.email) } }
                        div.account-property {
                            dt { "Plan" }
                            dd { span.account-plan { (plan) } a href="/billing" { "Manage billing" } }
                        }
                    }
                    form.account-property.zone-form method="post" action="/settings/timezone" {
                        (crate::ui::csrf(auth))
                        label for="workspace-timezone" { "Workspace time zone" }
                        div {
                            div.zone-entry {
                                input #workspace-timezone name="timezone" value=(zone_value) maxlength="128"
                                    autocapitalize="none" spellcheck="false" required
                                    autofocus[zone_error] aria-invalid=[zone_error.then_some("true")]
                                    aria-describedby=(if zone_error { "zone-error zone-help" } else { "zone-help" });
                                button.action-primary type="submit" { "Save time zone" }
                            }
                            @if zone_error { p.field-error #zone-error role="alert" { "Unknown time zone. Check the spelling and try again." } }
                            p.form-hint #zone-help { "Use an IANA time zone such as Europe/Zurich or America/New_York." }
                        }
                    }
                }
                section.settings-section aria-labelledby="account-access" {
                    h2 #account-access { "Account access" }
                    details.account-disclosure {
                        summary {
                            span { "Sessions" }
                            span.account-disclosure-note {
                                (sessions) @if sessions==1 { " active session" } @else { " active sessions" }
                                ", including this browser"
                            }
                        }
                        div.account-detail {
                            @if pairing { p { "A desktop sign-in is awaiting confirmation." } }
                            div.session-actions {
                                form method="post" action="/logout" {
                                    (crate::ui::csrf(auth))
                                    button.action-secondary type="submit" { "Sign out of this browser" }
                                }
                                @if sessions>1 || pairing {
                                    form method="post" action="/settings/sessions/revoke" {
                                        (crate::ui::csrf(auth))
                                        button.action-secondary type="submit" { "Sign out of other devices" }
                                    }
                                }
                            }
                        }
                    }
                    (recovery::settings(auth))
                }
                @if support.is_some() || status.is_some() {
                    section.settings-section aria-labelledby="workspace-support" {
                        h2 #workspace-support { "Support" }
                        div.settings-support {
                            @if let Some(addr)=support { a href=(format!("mailto:{addr}")) { (addr) } }
                            @if let Some(url)=status { a href=(url.as_str()) { "Service status" } }
                        }
                    }
                }
                section.settings-section aria-labelledby="workspace-data" {
                    h2 #workspace-data { "Workspace data" }
                    details.account-disclosure open[delete_error] {
                        summary { span { "Delete workspace" } span.account-disclosure-note { "Permanently remove your projects, issues and notes" } }
                        div.account-detail {
                            p { "Deleting your account signs out every device and removes your projects, issues, notes and connections. "
                                a href="/transfer" { "Export workspace data" } " before continuing." }
                            p { "Deletion also requests cancellation of your subscription and any open checkout. Stripe may retain billing records." }
                            form.delete-form method="post" action="/settings/delete" {
                                (crate::ui::csrf(auth))
                                label for="delete-email" { "Type your sign-in email to confirm deletion" }
                                input #delete-email type="email" name="email" autocomplete="off" autocapitalize="none" spellcheck="false" required
                                    autofocus[delete_error] aria-invalid=[delete_error.then_some("true")]
                                    aria-describedby=[delete_error.then_some("delete-email-error")];
                                @if delete_error { p.field-error #delete-email-error role="alert" { "Enter your sign-in email, " (auth.email) ", to confirm deletion." } }
                                button.action-secondary.danger type="submit" { "Delete my workspace permanently" }
                            }
                        }
                    }
                }
            }
        },
        None,
    ))
}

#[derive(Deserialize)]
struct TimezoneForm {
    csrf: String,
    timezone: String,
}

async fn save_timezone(
    auth: Auth,
    State(state): State<AppState>,
    Form(form): Form<TimezoneForm>,
) -> AppResult<Response> {
    auth.check_csrf(&form.csrf)?;
    let zone = form.timezone.trim();
    if zone.is_empty()
        || zone.len() > 128
        || zone.starts_with("posix/")
        || zone.starts_with("right/")
        || zone == "localtime"
        || !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_timezone_names WHERE name=$1)",
        )
        .bind(zone)
        .fetch_one(&state.db)
        .await?
    {
        let page = settings_screen(&auth, &state, SettingsFeedback::InvalidTimezone(zone)).await?;
        return Ok((StatusCode::BAD_REQUEST, page).into_response());
    }
    sqlx::query("UPDATE users SET timezone=$1 WHERE id=$2")
        .bind(zone)
        .bind(auth.user_id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/settings?result=timezone").into_response())
}

async fn delete_account(
    auth: Auth,
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DeleteForm>,
) -> AppResult<Response> {
    auth.check_csrf(&form.csrf)?;
    if normalized_email(&form.email).as_deref() != Some(auth.email.as_str()) {
        let page = settings_screen(&auth, &state, SettingsFeedback::InvalidDeletionEmail).await?;
        return Ok((StatusCode::BAD_REQUEST, page).into_response());
    }
    let raw = session_cookie_raw(&headers, state.config.production).ok_or(AppError::Forbidden)?;
    let mut tx = state.db.begin().await?;
    lock_email(&mut tx, &auth.email).await?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE id=$1 AND email=$2 FOR UPDATE")
            .bind(auth.user_id)
            .bind(&auth.email)
            .fetch_optional(&mut *tx)
            .await?;
    if owner.is_none() {
        return Err(AppError::Forbidden);
    }
    let active: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM user_sessions WHERE token_hash=$1 AND user_id=$2
         AND expires_at>now() AND created_at>now()-interval '15 minutes' FOR UPDATE",
    )
    .bind(digest(raw))
    .bind(auth.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if active.is_none() {
        return Ok(public_page(
            "Verify your email to delete this workspace",
            html! {
                p { "Sign in again and confirm deletion from Settings within 15 minutes." }
                a.button.action-primary href="/login?next=%2Fsettings" { "Sign in again" }
            },
        )
        .into_response());
    }
    crate::billing::ensure_deletable(&mut tx, auth.user_id).await?;
    sqlx::query("INSERT INTO account_deletions(user_id) VALUES($1)")
        .bind(auth.user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM login_challenges WHERE email=$1")
        .bind(&auth.email)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(auth.user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(signout_response(
        state.config.production,
        auth.user_id,
        "/login?result=deleted",
    ))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/login", get(login).post(request_link))
        .route("/login/check", get(check_inbox))
        .route("/login/confirm", get(confirm).post(verify))
        .route("/logout", post(logout))
        .route("/settings", get(settings))
        .route("/settings/sessions/revoke", post(revoke))
        .route("/settings/delete", post(delete_account))
        .route("/settings/timezone", post(save_timezone))
        .merge(desktop::routes())
        .merge(recovery::routes())
}
