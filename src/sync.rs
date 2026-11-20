//! Desktop windows share the web workspace. An authenticated page records the revision
//! before its handler reads any content, so a commit during rendering is seen on the
//! first poll. The request-scoped value never crosses accounts or requests.
use std::cell::Cell;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use maud::{Markup, html};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{AppResult, AppState, auth::Auth};

tokio::task_local! {
    static PAGE_REVISION: Cell<Option<(Uuid, i64)>>;
}

pub(crate) async fn desktop_page_context(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let desktop_read = req.method() == Method::GET
        && !path.starts_with("/api/")
        && !path.starts_with("/assets/")
        && req
            .headers()
            .get("sec-fetch-dest")
            .is_none_or(|destination| destination == "document" || destination == "iframe")
        && crate::auth::is_desktop_client(req.headers());
    if desktop_read {
        PAGE_REVISION.scope(Cell::new(None), next.run(req)).await
    } else {
        next.run(req).await
    }
}

async fn revision(db: &PgPool, owner: Uuid) -> AppResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE((SELECT revision FROM workspace_device_changes WHERE owner_id=$1), 0)",
    )
    .bind(owner)
    .fetch_one(db)
    .await?)
}

pub(crate) async fn capture_baseline(db: &PgPool, owner: Uuid) -> AppResult<()> {
    if PAGE_REVISION.try_with(|slot| slot.get().is_none()) == Ok(true) {
        let value = revision(db, owner).await?;
        PAGE_REVISION.with(|slot| slot.set(Some((owner, value))));
    }
    Ok(())
}

fn token(owner: Uuid, revision: i64) -> String {
    // Two workspaces at the same revision must compare unequal after an account switch.
    let workspace = Sha256::digest(owner.as_bytes());
    format!("{}.{revision}", hex::encode(&workspace[..16]))
}

pub(crate) fn head(auth: &Auth) -> Markup {
    let baseline = PAGE_REVISION
        .try_with(Cell::get)
        .ok()
        .flatten()
        .filter(|(owner, _)| *owner == auth.user_id);
    html! {
        @if let Some((owner, revision)) = baseline {
            meta name="workspace-revision" content=(token(owner, revision));
            script defer src="/assets/device-sync.js" {}
        }
    }
}

#[derive(Serialize)]
struct Changes {
    revision: String,
}

async fn changes(State(state): State<AppState>, auth: Result<Auth, Response>) -> Response {
    let status = match auth {
        Ok(auth) => match revision(&state.db, auth.user_id).await {
            Ok(revision) => {
                return Json(Changes {
                    revision: token(auth.user_id, revision),
                })
                .into_response();
            }
            Err(_) => StatusCode::SERVICE_UNAVAILABLE,
        },
        Err(error) if error.status().is_redirection() => StatusCode::UNAUTHORIZED,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    status.into_response()
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/device/changes", get(changes))
        .route(
            "/assets/device-sync.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../assets/device-sync.js"),
                )
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use serde_json::Value;
    use tower::ServiceExt;

    const POLL: &str = "/api/device/changes";
    const DESKTOP: &str = "RatioDesktop/test";

    async fn owner(db: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id, email) VALUES ($1,$2)")
            .bind(id)
            .bind(format!("{id}@example.invalid"))
            .execute(db)
            .await
            .unwrap();
        id
    }

    async fn cookie(db: &PgPool, owner: Uuid) -> String {
        let raw = hex::encode(Sha256::digest(Uuid::new_v4().as_bytes()));
        sqlx::query("INSERT INTO user_sessions(user_id, token_hash) VALUES ($1,$2)")
            .bind(owner)
            .bind(Sha256::digest(raw.as_bytes()).to_vec())
            .execute(db)
            .await
            .unwrap();
        format!("workspace_session={raw}")
    }

    async fn request(
        app: &Router,
        cookie: &str,
        agent: &str,
        path: &str,
        form: Option<&[(&str, &str)]>,
    ) -> Response {
        let body = form
            .map(|fields| {
                url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(fields.iter().copied())
                    .finish()
            })
            .unwrap_or_default();
        let request = HttpRequest::builder()
            .method(if form.is_some() {
                Method::POST
            } else {
                Method::GET
            })
            .uri(path)
            .header(header::USER_AGENT, agent)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(request).await.unwrap()
    }

    async fn body(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    async fn poll(app: &Router, cookie: &str) -> String {
        let response = request(app, cookie, DESKTOP, POLL, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let report: Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(report.as_object().unwrap().len(), 1);
        report["revision"].as_str().unwrap().to_owned()
    }

    fn attribute<'a>(page: &'a str, prefix: &str) -> &'a str {
        page.split_once(prefix)
            .unwrap()
            .1
            .split_once('"')
            .unwrap()
            .0
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn independent_sessions_observe_commits_that_follow_the_page_baseline(db: PgPool) {
        let id = owner(&db).await;
        let writer = cookie(&db, id).await;
        let reader = cookie(&db, id).await;
        let outsider = cookie(&db, owner(&db).await).await;
        let app = crate::app(AppState::for_test(db.clone()));

        let initial = poll(&app, &writer).await;
        let other_initial = poll(&app, &outsider).await;
        assert_ne!(
            initial, other_initial,
            "different workspaces at revision zero"
        );
        let page = body(request(&app, &reader, DESKTOP, "/projects", None).await).await;
        let baseline = attribute(&page, "name=\"workspace-revision\" content=\"");
        assert_eq!(baseline, initial);
        assert!(page.contains("/assets/device-sync.js"));
        let legacy =
            body(request(&app, &reader, "WorkingSetDesktop/0.1.0", "/projects", None).await).await;
        assert_eq!(
            attribute(&legacy, "name=\"workspace-revision\" content=\""),
            initial
        );
        assert!(legacy.contains("/assets/device-sync.js"));
        let web = body(request(&app, &writer, "Browser", "/projects", None).await).await;
        assert!(!web.contains("workspace-revision"));
        assert!(!web.contains("/assets/device-sync.js"));
        let csrf = attribute(&web, "name=\"csrf\" value=\"");
        let fields = [("csrf", csrf), ("name", "Peer project"), ("key", "PEER")];
        let created = request(&app, &writer, "Browser", "/projects", Some(&fields)).await;
        assert_eq!(created.status(), StatusCode::SEE_OTHER);
        let path = created.headers()[header::LOCATION].to_str().unwrap();
        // The reader's first poll happens *after* the peer's commit.
        let after_create = poll(&app, &reader).await;
        assert_ne!(baseline, after_create);
        assert_eq!(poll(&app, &outsider).await, other_initial);
        let refreshed = body(request(&app, &reader, DESKTOP, path, None).await).await;
        assert!(refreshed.contains("Peer project"));
        assert_eq!(
            attribute(&refreshed, "name=\"workspace-revision\" content=\""),
            after_create
        );
        let csrf = attribute(&refreshed, "name=\"csrf\" value=\"");
        let edit_path = format!("{path}/edit");
        let edit = |name| [("csrf", csrf), ("name", name), ("key", "PEER")];
        let unchanged = request(
            &app,
            &reader,
            DESKTOP,
            &edit_path,
            Some(&edit("Peer project")),
        )
        .await;
        assert_eq!(unchanged.status(), StatusCode::SEE_OTHER);
        assert_eq!(poll(&app, &writer).await, after_create);
        let renamed = request(
            &app,
            &reader,
            DESKTOP,
            &edit_path,
            Some(&edit("Renamed on desktop")),
        )
        .await;
        assert_eq!(renamed.status(), StatusCode::SEE_OTHER);
        let after_rename = poll(&app, &writer).await;
        assert_ne!(after_create, after_rename);
        let web = body(request(&app, &writer, "Browser", path, None).await).await;
        assert!(web.contains("Renamed on desktop"));

        let mut tx = db.begin().await.unwrap();
        sqlx::query("UPDATE projects SET name='uncommitted' WHERE owner_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        assert_eq!(poll(&app, &reader).await, after_rename);
        tx.rollback().await.unwrap();
        assert_eq!(poll(&app, &reader).await, after_rename);

        let project = path.trim_start_matches("/projects/");
        let command = Uuid::new_v4().to_string();
        let fields = [
            ("csrf", csrf),
            ("project", project),
            ("command_id", &command),
            ("title", "Critical device report"),
            ("severity", "critical"),
            ("next_action", "Review the report"),
        ];
        let issue = request(&app, &reader, DESKTOP, "/issues", Some(&fields)).await;
        assert_eq!(issue.status(), StatusCode::SEE_OTHER);
        assert_ne!(poll(&app, &writer).await, after_rename);
        let home = body(request(&app, &writer, DESKTOP, "/", None).await).await;
        assert!(home.contains("Critical device report"));
        assert_eq!(poll(&app, &outsider).await, other_initial);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unauthenticated_expired_and_held_sessions_get_no_change_information(db: PgPool) {
        let id = owner(&db).await;
        let expired = cookie(&db, id).await;
        sqlx::query("UPDATE user_sessions SET created_at=now()-interval '2 days', expires_at=now()-interval '1 day' WHERE user_id=$1")
            .bind(id).execute(&db).await.unwrap();
        let app = crate::app(AppState::for_test(db.clone()));
        for cookie in ["", "workspace_session=invalid", &expired] {
            let result = request(&app, cookie, DESKTOP, POLL, None).await;
            assert_eq!(result.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(result.headers()[header::CACHE_CONTROL], "private, no-store");
            assert!(body(result).await.is_empty());
        }
        let held = cookie(&db, id).await;
        sqlx::query("UPDATE users SET identity_current=false WHERE id=$1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let result = request(&app, &held, DESKTOP, POLL, None).await;
        assert_eq!(result.status(), StatusCode::UNAUTHORIZED);
        assert!(body(result).await.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_commit_during_rendering_is_visible_to_the_first_poll(db: PgPool) {
        let id = owner(&db).await;
        let cookie = cookie(&db, id).await;
        let state = AppState::for_test(db);
        let app = crate::app(state.clone());
        let initial = poll(&app, &cookie).await;
        let render = Router::new()
            .route("/", get(|State(state): State<AppState>, auth: Auth| async move {
                sqlx::query("INSERT INTO projects(owner_id, name, key) VALUES ($1,'Peer commit','PEER')")
                    .bind(auth.user_id).execute(&state.db).await.unwrap();
                crate::ui::page(&auth, "Projects", "projects", html! {})
            }))
            .layer(axum::middleware::from_fn(desktop_page_context))
            .with_state(state);
        let page = body(request(&render, &cookie, DESKTOP, "/", None).await).await;
        let baseline = attribute(&page, "name=\"workspace-revision\" content=\"");
        assert_eq!(baseline, initial);
        assert_ne!(poll(&app, &cookie).await, baseline);
    }
}
