use super::{
    Binding, Callback, authorize_callback, bind_repo, credentials, live_repo, oauth_state,
    provider::Provider,
};
use crate::{AppError, AppState, auth::Auth};
use axum::{
    Form, Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;

const ACCESS: &str = "fixture-access-before-refresh";
const REFRESH: &str = "fixture-refresh-token";
const ROTATED: &str = "fixture-access-after-refresh";
const ROTATED_REFRESH: &str = "fixture-rotated-refresh-token";

#[derive(Default)]
struct Requests {
    exchanges: AtomicUsize,
    identities: AtomicUsize,
    refreshes: AtomicUsize,
    pause_identity: AtomicBool,
    identity_requested: tokio::sync::Notify,
    allow_identity: tokio::sync::Notify,
}

#[derive(Deserialize)]
struct TokenRequest {
    code: Option<String>,
    code_verifier: Option<String>,
    grant_type: Option<String>,
    refresh_token: Option<String>,
}

async fn token(State(calls): State<Arc<Requests>>, Form(form): Form<TokenRequest>) -> Response {
    let (access, refresh) = if form.grant_type.as_deref() == Some("refresh_token") {
        if form.refresh_token.as_deref() != Some(REFRESH)
            || calls.refreshes.fetch_add(1, Ordering::SeqCst) != 0
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        (ROTATED, ROTATED_REFRESH)
    } else {
        if form.code.as_deref() != Some("fixture-code")
            || form.code_verifier.as_deref().is_none_or(|v| v.len() != 43)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        calls.exchanges.fetch_add(1, Ordering::SeqCst);
        (ACCESS, REFRESH)
    };
    Json(
        json!({"access_token":access,"token_type":"bearer","expires_in":28800,
                "refresh_token":refresh,"refresh_token_expires_in":15552000}),
    )
    .into_response()
}

fn authorized(headers: &HeaderMap, value: Value) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if matches!(bearer, Some(ACCESS | ROTATED)) {
        Json(value).into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

struct Github {
    api: Provider,
    calls: Arc<Requests>,
    task: JoinHandle<()>,
}

async fn identity(State(calls): State<Arc<Requests>>, headers: HeaderMap) -> Response {
    calls.identities.fetch_add(1, Ordering::SeqCst);
    if calls.pause_identity.load(Ordering::SeqCst) {
        calls.identity_requested.notify_one();
        calls.allow_identity.notified().await;
    }
    authorized(&headers, json!({"id":882}))
}

async fn installations(headers: HeaderMap) -> Response {
    authorized(
        &headers,
        json!({
            "installations": [{
                "id": 501,
                "app_slug": "workspace",
                "permissions": {"metadata": "read", "pull_requests": "read", "vulnerability_alerts": "read"}
            }]
        }),
    )
}

async fn repositories(headers: HeaderMap) -> Response {
    authorized(
        &headers,
        json!({
            "repositories": [
                {"id": 601, "full_name": "fixture/first"},
                {"id": 602, "full_name": "fixture/second"}
            ]
        }),
    )
}

impl Github {
    async fn new() -> Self {
        let calls = Arc::new(Requests::default());
        let router = Router::new()
            .route("/login/oauth/access_token", post(token))
            .route("/user", get(identity))
            .route("/user/installations", get(installations))
            .route("/user/installations/501/repositories", get(repositories))
            .with_state(calls.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            api: Provider::fixture_at(&url),
            calls,
            task,
        }
    }
}

impl Drop for Github {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn actor(db: &PgPool) -> Auth {
    let user_id = Uuid::new_v4();
    let email = format!("{user_id}@example.invalid");
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(user_id)
        .bind(&email)
        .execute(db)
        .await
        .unwrap();
    Auth {
        user_id,
        email,
        csrf: "d".repeat(64),
    }
}

fn callback(state: &str) -> Callback {
    Callback {
        state: state.into(),
        code: Some("fixture-code".into()),
    }
}

async fn connect(db: &PgPool, s: &AppState, mock: &Github, auth: &Auth) {
    let nonce = oauth_state(db, auth).await.unwrap();
    let result = authorize_callback(s, &mock.api, auth, callback(&nonce))
        .await
        .unwrap();
    assert_eq!(result.into_response().status(), StatusCode::SEE_OTHER);
}

#[sqlx::test(migrations = "./migrations")]
async fn oauth_state_is_owner_session_expiry_and_single_use_bound(db: PgPool) {
    let s = AppState::for_test(db.clone());
    let owner = actor(&db).await;
    let outsider = actor(&db).await;
    let other_session = Auth {
        csrf: "e".repeat(64),
        ..owner.clone()
    };
    let mock = Github::new().await;
    let old = oauth_state(&db, &owner).await.unwrap();
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT state_hash FROM github_oauth_states WHERE owner_id=$1")
            .bind(owner.user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(stored, super::hash(&old));
    for actor in [&outsider, &other_session] {
        assert!(matches!(
            authorize_callback(&s, &mock.api, actor, callback(&old)).await,
            Err(AppError::Forbidden)
        ));
    }
    sqlx::query(
        "UPDATE github_oauth_states SET expires_at=now()-interval '1 second' WHERE owner_id=$1",
    )
    .bind(owner.user_id)
    .execute(&db)
    .await
    .unwrap();
    assert!(matches!(
        authorize_callback(&s, &mock.api, &owner, callback(&old)).await,
        Err(AppError::Forbidden)
    ));
    assert_eq!(mock.calls.exchanges.load(Ordering::SeqCst), 0);
    assert_eq!(mock.calls.identities.load(Ordering::SeqCst), 0);

    let nonce = oauth_state(&db, &owner).await.unwrap();
    assert_ne!(nonce, old);
    let (first, second) = tokio::join!(
        authorize_callback(&s, &mock.api, &owner, callback(&nonce)),
        authorize_callback(&s, &mock.api, &owner, callback(&nonce))
    );
    let results = [first, second];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(AppError::Forbidden)))
            .count(),
        1
    );
    assert!(matches!(
        authorize_callback(&s, &mock.api, &owner, callback(&nonce)).await,
        Err(AppError::Forbidden)
    ));
    assert_eq!(mock.calls.exchanges.load(Ordering::SeqCst), 1);
    assert_eq!(mock.calls.identities.load(Ordering::SeqCst), 1);

    let saved: (i64, Vec<u8>, Vec<u8>, bool) = sqlx::query_as(
        "SELECT user_remote_id,access_cipher,refresh_cipher,
                expires_at>now() AND refresh_expires_at>expires_at FROM github_connections WHERE owner_id=$1",
    ).bind(owner.user_id).fetch_one(&db).await.unwrap();
    assert_eq!(saved.0, 882);
    assert!(saved.3);
    for (cipher, clear) in [(&saved.1, ACCESS), (&saved.2, REFRESH)] {
        assert!(!cipher.windows(clear.len()).any(|s| s == clear.as_bytes()));
        assert_eq!(mock.api.unseal(owner.user_id, cipher).unwrap(), clear);
        assert!(mock.api.unseal(outsider.user_id, cipher).is_err());
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn selected_installation_and_repo_are_verified_before_enforcing_free_quota(db: PgPool) {
    let s = AppState::for_test(db.clone());
    let owner = actor(&db).await;
    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Binding','BIND') RETURNING id",
    )
    .bind(owner.user_id)
    .fetch_one(&db)
    .await
    .unwrap();
    let mock = Github::new().await;
    connect(&db, &s, &mock, &owner).await;
    let binding = |repo_id, installation_id| Binding {
        csrf: owner.csrf.clone(),
        project,
        repo_id,
        installation_id,
    };
    for (repo, install) in [(601, 502), (603, 501)] {
        assert!(matches!(
            bind_repo(&s, &mock.api, &owner, binding(repo, install)).await,
            Err(AppError::Forbidden)
        ));
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM github_repos")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(rows, 0);

    let (first, second) = tokio::join!(
        bind_repo(&s, &mock.api, &owner, binding(601, 501)),
        bind_repo(&s, &mock.api, &owner, binding(602, 501))
    );
    let results = [first, second];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r,Err(AppError::Conflict(message)) if message.contains("limit")))
            .count(),
        1
    );
    let rows: Vec<(i64, i64, Uuid, String, bool)> = sqlx::query_as(
        "SELECT repo_id,installation_id,project_id,display_name,
                repo_grant_until IS NULL AND alert_grant_until IS NULL
         FROM github_repos WHERE owner_id=$1",
    )
    .bind(owner.user_id)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    let (repo, install, mapped, name, grant) = &rows[0];
    assert_eq!((*install, *mapped, *grant), (501, project, true));
    assert!(name.is_empty());
    assert!(matches!(repo, 601 | 602));

    let old_access = credentials(&s, &mock.api, owner.user_id).await.unwrap();
    connect(&db, &s, &mock, &owner).await;
    let new_access = credentials(&s, &mock.api, owner.user_id).await.unwrap();
    assert_ne!(new_access.grant_id, old_access.grant_id);
    assert!(
        live_repo(&s, &mock.api, owner.user_id, &old_access, *repo)
            .await
            .is_err()
    );
    let verified = live_repo(&s, &mock.api, owner.user_id, &new_access, *repo)
        .await
        .unwrap();
    assert_eq!(verified.id, *repo);
    assert!(matches!(
        (*repo, verified.name.as_str()),
        (601, "fixture/first") | (602, "fixture/second")
    ));
    let active: bool = sqlx::query_scalar(
        "SELECT repo_grant_until>now() AND alert_grant_until IS NULL FROM github_repos
         WHERE owner_id=$1 AND repo_id=$2",
    )
    .bind(owner.user_id)
    .bind(*repo)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(active);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_expired_credentials_rotate_the_refresh_token_once(db: PgPool) {
    let s = AppState::for_test(db.clone());
    let owner = actor(&db).await;
    let mock = Github::new().await;
    connect(&db, &s, &mock, &owner).await;
    sqlx::query(
        "UPDATE github_connections SET expires_at=now()-interval '1 second' WHERE owner_id=$1",
    )
    .bind(owner.user_id)
    .execute(&db)
    .await
    .unwrap();
    let (first, second) = tokio::join!(
        credentials(&s, &mock.api, owner.user_id),
        credentials(&s, &mock.api, owner.user_id)
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!((&*first, &*second), (ROTATED, ROTATED));
    assert_eq!(first.grant_id, second.grant_id);
    assert_eq!(mock.calls.refreshes.load(Ordering::SeqCst), 1);
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT refresh_cipher FROM github_connections WHERE owner_id=$1")
            .bind(owner.user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        mock.api.unseal(owner.user_id, &stored).unwrap(),
        ROTATED_REFRESH
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn disconnect_during_exchange_prevents_a_late_callback_from_restoring_access(db: PgPool) {
    let state = AppState::for_test(db.clone());
    let owner = actor(&db).await;
    let github = Github::new().await;
    let nonce = oauth_state(&db, &owner).await.unwrap();
    github.calls.pause_identity.store(true, Ordering::SeqCst);
    let cancel = async {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            github.calls.identity_requested.notified(),
        )
        .await
        .expect("identity request should reach fixture");
        let redirect = super::disconnect(
            State(state.clone()),
            owner.clone(),
            Form(super::Command {
                csrf: owner.csrf.clone(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(redirect.into_response().status(), StatusCode::SEE_OTHER);
        github.calls.allow_identity.notify_one();
    };
    let (result, ()) = tokio::join!(
        authorize_callback(&state, &github.api, &owner, callback(&nonce)),
        cancel
    );
    assert!(matches!(result, Err(AppError::Forbidden)));
    let connections: i64 =
        sqlx::query_scalar("SELECT count(*) FROM github_connections WHERE owner_id=$1")
            .bind(owner.user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(connections, 0);
}
