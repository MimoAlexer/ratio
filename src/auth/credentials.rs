use crate::{AppError, AppResult, AppState};
use axum::{
    extract::{FromRequestParts, OriginalUri},
    http::{HeaderMap, HeaderValue, Method, Uri, header, request::Parts},
    response::{IntoResponse, Redirect, Response},
};
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use reqwest::Url;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use subtle::ConstantTimeEq;
use uuid::Uuid;

#[derive(Clone)]
pub struct Auth {
    pub user_id: Uuid,
    pub email: String,
    pub csrf: String,
}

impl Auth {
    pub fn check_csrf(&self, value: &str) -> Result<(), AppError> {
        if valid_token(value) && bool::from(self.csrf.as_bytes().ct_eq(value.as_bytes())) {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }
}

pub(super) fn token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub(super) fn valid_token(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn digest(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

fn csrf_for(raw: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(raw.as_bytes()).expect("HMAC accepts every key size");
    mac.update(b"workspace/form-csrf/v1");
    hex::encode(mac.finalize().into_bytes())
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|line| line.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter_map(|(key, value)| (key == name).then_some(value));
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

pub(super) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    cookie_value(headers, name).filter(|v| valid_token(v))
}

pub(super) fn cleanup_cookie_name(production: bool) -> &'static str {
    if production {
        "__Host-workspace_draft_cleanup"
    } else {
        "workspace_draft_cleanup"
    }
}

pub(super) fn cleanup_owner(headers: &HeaderMap, production: bool) -> Option<Uuid> {
    let value = cookie_value(headers, cleanup_cookie_name(production))?;
    Uuid::parse_str(value).ok()
}

pub(super) fn session_name(production: bool) -> &'static str {
    if production {
        "__Host-workspace_session"
    } else {
        "workspace_session"
    }
}

pub(super) fn nonce_name(production: bool) -> &'static str {
    if production {
        "__Host-workspace_form"
    } else {
        "workspace_form"
    }
}

pub(super) fn session_cookie_raw(headers: &HeaderMap, production: bool) -> Option<&str> {
    cookie(headers, session_name(production))
}

pub(super) fn set_cookie(response: &mut Response, name: &str, value: &str, age: u32, secure: bool) {
    let suffix = if secure { "; Secure" } else { "" };
    let data = format!("{name}={value}; Path=/; Max-Age={age}; HttpOnly; SameSite=Lax{suffix}");
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&data).expect("cookie uses fixed names and hex tokens"),
    );
}

pub(super) fn put_session(response: &mut Response, production: bool, raw: &str) {
    set_cookie(
        response,
        session_name(production),
        raw,
        30 * 24 * 3600,
        production,
    );
}

pub(super) fn form_nonce(headers: &HeaderMap, production: bool) -> String {
    cookie(headers, nonce_name(production))
        .map(str::to_owned)
        .unwrap_or_else(token)
}

pub(super) fn put_nonce(response: &mut Response, production: bool, value: &str) {
    set_cookie(response, nonce_name(production), value, 1800, production);
}

pub(super) fn check_nonce(headers: &HeaderMap, production: bool, supplied: &str) -> AppResult<()> {
    let current = cookie(headers, nonce_name(production)).ok_or(AppError::Forbidden)?;
    if valid_token(supplied) && bool::from(current.as_bytes().ct_eq(supplied.as_bytes())) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

pub(super) fn normalized_email(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() > 254 || !value.is_ascii() {
        return None;
    }
    let (local, domain) = value.split_once('@')?;
    if local.is_empty()
        || local.len() > 64
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || !local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&b))
        || !domain.contains('.')
        || !domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

// Only known, navigable workspace pages are valid post-login destinations.
// Preserve only explicitly supported query parameters.
pub(super) fn safe_next(value: &str) -> String {
    if value.len() > 512 {
        return "/".into();
    }
    let Ok(uri) = value.parse::<Uri>() else {
        return "/".into();
    };
    if uri.scheme().is_some() || uri.authority().is_some() || !value.starts_with('/') {
        return "/".into();
    }
    let path = uri.path();
    if path == "/" {
        return "/".into();
    }
    let pieces = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    if path.starts_with("//") {
        return "/".into();
    }
    if path == "/desktop/authorize" {
        return uri
            .query()
            .and_then(|q| q.strip_prefix("code="))
            .filter(|code| super::desktop::valid_code(code))
            .map(|code| format!("/desktop/authorize?code={code}"))
            .unwrap_or_else(|| "/".into());
    }
    // The owner in a desktop GitHub handoff is displayed and checked by the
    // browser confirmation page before it creates an OAuth state.
    if path == "/integrations/github/browser" {
        return uri
            .query()
            .and_then(|q| q.strip_prefix("owner="))
            .and_then(|owner| {
                let id = Uuid::parse_str(owner).ok()?;
                (id.hyphenated().to_string() == owner).then(|| format!("{path}?owner={id}"))
            })
            .unwrap_or_else(|| "/".into());
    }
    let collection = match pieces.as_slice() {
        [
            top @ ("projects" | "issues" | "notes" | "roadmap" | "releases" | "insights"
            | "integrations" | "billing" | "transfer" | "settings" | "search" | "workflows"
            | "health"),
        ] => *top,
        ["notes", "new"] => "notes/new",
        ["notes", id, "edit"] if Uuid::parse_str(id).is_ok() => return format!("/notes/{id}/edit"),
        ["workflows", kind @ ("views" | "templates" | "digest"), id]
            if Uuid::parse_str(id).is_ok() =>
        {
            return format!("/workflows/{kind}/{id}");
        }
        ["transfer", "review", id] if Uuid::parse_str(id).is_ok() => {
            return format!("/transfer/review/{id}");
        }
        [top @ ("projects" | "issues" | "notes" | "releases"), id]
            if Uuid::parse_str(id).is_ok() =>
        {
            return format!("/{top}/{id}");
        }
        _ => return "/".into(),
    };
    let project = uri
        .query()
        .and_then(|q| q.strip_prefix("project="))
        .and_then(|id| Uuid::parse_str(id).ok());
    match project {
        Some(id) => format!("/{collection}?project={id}"),
        None => format!("/{collection}"),
    }
}

fn signin_redirect(parts: &Parts, state: &AppState) -> Response {
    let uri = parts
        .extensions
        .get::<OriginalUri>()
        .map(|uri| &uri.0)
        .unwrap_or(&parts.uri);
    let path = if parts.method == Method::GET || parts.method == Method::HEAD {
        safe_next(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"))
    } else {
        let referer = parts
            .headers
            .get(header::REFERER)
            .and_then(|r| r.to_str().ok())
            .and_then(|r| Url::parse(r).ok());
        let base = Url::parse(&state.config.base_url).ok();
        if let Some(page) = referer.filter(|url| {
            base.as_ref()
                .is_some_and(|base| base.origin() == url.origin())
        }) {
            safe_next(page.path())
        } else {
            let root = uri.path().split('/').take(3).collect::<Vec<_>>().join("/");
            safe_next(&root)
        }
    };
    let mut url = Url::parse("http://localhost/login").expect("fixed URL");
    if path != "/" {
        url.query_pairs_mut().append_pair("next", &path);
    }
    let target = url
        .query()
        .map(|q| format!("/login?{q}"))
        .unwrap_or_else(|| "/login".into());
    Redirect::to(&target).into_response()
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = Response;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(raw) = session_cookie_raw(&parts.headers, state.config.production) else {
            return Err(signin_redirect(parts, state));
        };
        match session(&state.db, raw).await {
            Ok(Some(auth)) => {
                crate::sync::capture_baseline(&state.db, auth.user_id)
                    .await
                    .map_err(IntoResponse::into_response)?;
                Ok(auth)
            }
            Ok(None) => Err(signin_redirect(parts, state)),
            Err(err) => Err(err.into_response()),
        }
    }
}

pub(super) async fn session(db: &PgPool, raw: &str) -> AppResult<Option<Auth>> {
    if !valid_token(raw) {
        return Ok(None);
    }
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT u.id,u.email FROM user_sessions s JOIN users u ON u.id=s.user_id WHERE u.identity_current AND s.token_hash=$1 AND s.expires_at>now()"
    ).bind(digest(raw)).fetch_optional(db).await?;
    Ok(row.map(|(user_id, email)| Auth {
        user_id,
        email,
        csrf: csrf_for(raw),
    }))
}

pub(super) async fn new_session(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> AppResult<String> {
    let raw = token();
    sqlx::query("INSERT INTO user_sessions(token_hash,user_id) VALUES($1,$2)")
        .bind(digest(&raw))
        .bind(user)
        .execute(&mut **tx)
        .await?;
    Ok(raw)
}

pub(super) async fn lock_email(tx: &mut Transaction<'_, Postgres>, email: &str) -> AppResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 42))")
        .bind(email)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(super) async fn redeem(
    db: &PgPool,
    raw: &str,
    previous: Option<&str>,
) -> AppResult<Option<(String, String)>> {
    if !valid_token(raw) {
        return Ok(None);
    }
    let hash = digest(raw);
    let mut tx = db.begin().await?;
    let maybe_email: Option<String> = sqlx::query_scalar(
        "SELECT email FROM login_challenges WHERE token_hash=$1 AND expires_at>now()",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(email) = maybe_email else {
        return Ok(None);
    };
    lock_email(&mut tx, &email).await?;
    let returned_to: Option<String> = sqlx::query_scalar(
        "DELETE FROM login_challenges WHERE token_hash=$1 AND expires_at>now() RETURNING return_to",
    )
    .bind(hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(returned_to) = returned_to else {
        return Ok(None);
    };
    let user: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO users(email) VALUES($1)
         ON CONFLICT(email) DO UPDATE SET email=EXCLUDED.email WHERE users.identity_current RETURNING id",
    )
    .bind(&email)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else {
        tx.commit().await?;
        return Ok(None);
    };
    if let Some(old) = previous.filter(|s| valid_token(s)) {
        sqlx::query("DELETE FROM user_sessions WHERE token_hash=$1")
            .bind(digest(old))
            .execute(&mut *tx)
            .await?;
    }
    let new_raw = new_session(&mut tx, user).await?;
    tx.commit().await?;
    Ok(Some((new_raw, safe_next(&returned_to))))
}

pub(super) async fn budget(
    tx: &mut Transaction<'_, Postgres>,
    bucket: &str,
    limit: i32,
) -> AppResult<bool> {
    let result = sqlx::query(
        "INSERT INTO login_rate_limits(bucket,hits) VALUES($1,1) ON CONFLICT(bucket) DO UPDATE SET
        started_at=CASE WHEN login_rate_limits.started_at<=now()-interval '15 minutes' THEN now() ELSE login_rate_limits.started_at END,
        hits=CASE WHEN login_rate_limits.started_at<=now()-interval '15 minutes' THEN 1 ELSE login_rate_limits.hits+1 END
        WHERE login_rate_limits.started_at<=now()-interval '15 minutes' OR login_rate_limits.hits<$2")
    .bind(digest(bucket))
    .bind(limit)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn purge_expired(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    sqlx::query("DELETE FROM desktop_devices WHERE expires_at<=now()")
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM login_rate_limits WHERE started_at<now()-interval '1 day'")
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM login_challenges WHERE expires_at<=now()")
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM user_sessions WHERE expires_at<=now()")
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "UPDATE account_recovery SET challenge_hash=NULL, replacement_email=NULL, expires_at=NULL
         WHERE expires_at<=now()",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}
