pub mod auth;
pub mod billing;
pub mod github;
pub mod issues;
pub mod notes;
pub mod operations;
pub mod projects;
pub mod releases;
pub mod roadmap;
pub mod sample;
pub mod store;
pub mod sync;
pub mod transfer;
pub mod ui;
pub mod workflows;

use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use maud::html;
use reqwest::Client;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tower_http::compression::{
    CompressionLayer,
    predicate::{DefaultPredicate, NotForContentType, Predicate},
};

#[derive(Clone, Debug)]
pub struct Config {
    pub base_url: String,
    pub production: bool,
}
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub http: Client,
    pub config: Arc<Config>,
}
impl AppState {
    pub fn new(db: PgPool, config: Config) -> anyhow::Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("solo-tracker/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            db,
            http,
            config: Arc::new(config),
        })
    }
    pub fn for_test(db: PgPool) -> Self {
        Self::new(
            db,
            Config {
                base_url: "http://127.0.0.1".into(),
                production: false,
            },
        )
        .expect("test client")
    }
}
#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("Not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("Access denied")]
    Forbidden,
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}
pub type AppResult<T> = Result<T, AppError>;
impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        if matches!(e, sqlx::Error::RowNotFound) {
            return Self::NotFound;
        }
        if let Some(db) = e.as_database_error() {
            if db.constraint() == Some("workspace_storage_limit") {
                return Self::Conflict("This would exceed your workspace storage limit. Free some space or change plan.".into());
            }
            if db.is_unique_violation() {
                return Self::Conflict("This item already exists. Refresh and try again.".into());
            }
            if db.is_foreign_key_violation() {
                return Self::Conflict("A linked item changed. Refresh and try again.".into());
            }
            if db.is_check_violation() {
                return Self::BadRequest("A value is outside the supported range.".into());
            }
            tracing::error!(db_code=?db.code(),constraint=db.constraint(), "database operation failed");
        } else {
            tracing::error!(kind=?std::mem::discriminant(&e),"database operation failed");
        }
        Self::Internal("Database operation failed".into())
    }
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, detail) = match self {
            Self::BadRequest(s) => (StatusCode::BAD_REQUEST, s),
            Self::Conflict(s) => (StatusCode::CONFLICT, s),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "This action is unavailable for your account or session.".into(),
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "This item could not be found in this workspace.".into(),
            ),
            Self::Unavailable(s) => (StatusCode::SERVICE_UNAVAILABLE, s),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The request could not be completed. Try again in a moment.".into(),
            ),
        };
        (status,ui::public_page(status.canonical_reason().unwrap_or("Error"),html!{h1{(status.canonical_reason().unwrap_or("Error"))}p{(detail)}p{a href="/"{"Return to workspace"}}})).into_response()
    }
}
pub static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
pub async fn schema_ready(pool: &PgPool) -> bool {
    let Ok(applied) = sqlx::query_as::<_, (i64, bool, Vec<u8>)>(
        "SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    else {
        return false;
    };
    applied.iter().all(|row| row.1)
        && MIGRATIONS
            .iter()
            .filter(|m| !m.migration_type.is_down_migration())
            .all(|m| {
                applied
                    .binary_search_by_key(&m.version, |r| r.0)
                    .is_ok_and(|idx| applied[idx].2.as_slice() == m.checksum.as_ref())
            })
}
async fn ready(State(s): State<AppState>) -> Response {
    if schema_ready(&s.db).await {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "database not ready").into_response()
    }
}
#[derive(serde::Serialize, sqlx::FromRow)]
struct SidebarProject {
    id: uuid::Uuid,
    name: String,
    key: String,
}

async fn sidebar_projects(
    State(state): State<AppState>,
    auth: auth::Auth,
) -> AppResult<axum::Json<Vec<SidebarProject>>> {
    let projects=sqlx::query_as("SELECT id,name,key FROM projects WHERE owner_id=$1 AND NOT archived ORDER BY name,id LIMIT 6")
        .bind(auth.user_id).fetch_all(&state.db).await?;
    Ok(axum::Json(projects))
}

async fn ui_asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    let (kind, bytes): (&str, &'static [u8]) = match path.as_str() {
        "type.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../assets/type.css"),
        ),
        "fonts/instrument-sans.woff2" => (
            "font/woff2",
            include_bytes!("../assets/fonts/instrument-sans.woff2"),
        ),
        "fonts/ibm-plex-mono.woff2" => (
            "font/woff2",
            include_bytes!("../assets/fonts/ibm-plex-mono.woff2"),
        ),
        "issue-detail.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../assets/issue-detail.css"),
        ),
        "projects.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../assets/projects.css"),
        ),
        "issues.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../assets/issues.css"),
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, kind)], bytes).into_response()
}
async fn landing() -> Html<String> {
    ui::public_page(
        "Working Set",
        html! {
            h1 {"Your development workspace"}
            p {"Keep issues, Markdown decisions, release progress and critical repository alerts together in a private workspace."}
            p class="actions" {a class="button" href="/login"{"Open workspace"} " " a href="/pricing"{"See plans"}}
        },
    )
}
async fn pricing() -> Html<String> {
    ui::public_page(
        "Plans",
        html! {
            h1{"Plans"}p{"Monthly prices in USD for one private workspace."}
            table{thead{tr{th{"Plan"}th{"Monthly"}th{"Projects / repositories"}th{"Release history"}}}tbody{
                tr{td{"Free"}td{"$0"}td{"1 / 1"}td{"30 days"}}
                tr{td{"Plus"}td{"$5"}td{"10 / 10"}td{"12 months"}}
                tr{td{"Max"}td{"$15"}td{"50 / 50"}td{"All recorded history"}}
            }} p{"Every plan includes issues, linked notes, project planning, and native exports."}
            a class="button" href="/login"{"Start with Free"}
        },
    )
}
async fn asset_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/app.css"),
    )
}
async fn asset_shortcuts() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/workspace.js"),
    )
}
async fn asset_mark() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("../assets/mark.svg"),
    )
}
async fn protect(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let assets = path.starts_with("/assets/");
    let unsafe_method = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let provider_hook = matches!(
        path.as_str(),
        "/billing/webhook" | "/integrations/github/webhook"
    );
    let mut response = if unsafe_method
        && !provider_hook
        && req
            .headers()
            .get(header::ORIGIN)
            .is_some_and(|v| v.as_bytes() != app.config.base_url.as_bytes())
    {
        AppError::Forbidden.into_response()
    } else {
        next.run(req).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::CONTENT_SECURITY_POLICY,HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self' https://checkout.stripe.com https://billing.stripe.com https://github.com"));
    headers.entry(header::CACHE_CONTROL).or_insert(if assets {
        HeaderValue::from_static("public, max-age=300")
    } else {
        HeaderValue::from_static("private, no-store")
    });
    response
}
pub fn app(state: AppState) -> Router {
    Router::new()
        .merge(auth::routes())
        .merge(sync::routes())
        .merge(issues::routes())
        .merge(notes::routes())
        .merge(projects::routes())
        .merge(roadmap::routes())
        .merge(releases::routes())
        .merge(github::routes())
        .merge(operations::routes())
        .merge(transfer::routes())
        .merge(workflows::routes())
        .merge(billing::routes())
        .route("/api/sidebar/projects", get(sidebar_projects))
        .route("/assets/{*path}", get(ui_asset))
        .route("/readyz", get(ready))
        .route("/healthz", get(|| async { "ok" }))
        .route("/pricing", get(pricing))
        .route("/assets/workspace.js", get(asset_shortcuts))
        .route("/welcome", get(landing))
        .route("/assets/app.css", get(asset_css))
        .route("/assets/mark.svg", get(asset_mark))
        .fallback(|| async { AppError::NotFound })
        .layer(middleware::from_fn(sync::desktop_page_context))
        .layer(CompressionLayer::new().compress_when(
            DefaultPredicate::new().and(NotForContentType::const_new("application/zip")),
        ))
        .layer(middleware::from_fn_with_state(state.clone(), protect))
        .layer(middleware::from_fn(operations::observe))
        .with_state(state)
}

#[cfg(test)]
mod sidebar_tests {
    use super::*;

    #[sqlx::test]
    async fn sidebar_is_limited_to_this_accounts_active_projects(pool: PgPool) {
        let a: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES ('one@sidebar.invalid') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let b: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES ('two@sidebar.invalid') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        for n in 0..8 {
            sqlx::query("INSERT INTO projects(owner_id,key,name) VALUES($1,$2,$3)")
                .bind(a)
                .bind(format!("P{n}"))
                .bind(format!("Project {n}"))
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO projects(owner_id,key,name,archived) VALUES($1,'ZZ','A archived',true),($2,'FF','A foreign',false)").bind(a).bind(b).execute(&pool).await.unwrap();
        let axum::Json(visible) = sidebar_projects(
            State(AppState::for_test(pool)),
            auth::Auth {
                user_id: a,
                email: "one@sidebar.invalid".to_owned(),
                csrf: "test".to_owned(),
            },
        )
        .await
        .unwrap();
        let names: Vec<_> = visible.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Project 0",
                "Project 1",
                "Project 2",
                "Project 3",
                "Project 4",
                "Project 5"
            ]
        );
    }
}
