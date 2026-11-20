//! Project-scoped planning boards shared by the desktop clients and the web workspace.
#[cfg(test)]
mod api_tests;
mod checkpoints;
mod export;
pub(crate) mod model;
mod views;
pub(crate) use checkpoints::{
    NAMED_LIMIT as CHECKPOINT_LIMIT, RECOVERY_LIMIT as RECOVERY_CHECKPOINT_LIMIT,
};

use crate::{AppError, AppResult, AppState, auth::Auth, store};
use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use model::{Document, Source, SourceKind};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, PgConnection, PgPool, Postgres, Transaction};
use std::collections::HashSet;
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    let api = Router::new()
        .route("/api/canvas/{id}", get(get_board).post(save))
        .route("/api/canvas/{id}/sources", get(sources))
        .merge(checkpoints::routes())
        .route_layer(middleware::from_fn(api_rejections))
        .layer(DefaultBodyLimit::max(6 * 1024 * 1024));
    Router::new()
        .route("/canvas", get(directory).post(create))
        .route("/canvas/{id}", get(editor))
        .route("/canvas/{id}/delete", post(delete))
        .route("/canvas/{id}/export.svg", get(export::download))
        .merge(api)
}

struct ApiError(AppError);
type ApiResult<T> = Result<T, ApiError>;

impl From<AppError> for ApiError {
    fn from(error: AppError) -> Self {
        Self(error)
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self(error.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = match &self.0 {
            AppError::Internal(_) => "The board request could not be completed.".to_owned(),
            error => error.to_string(),
        };
        let status = self.0.into_response().status();
        (status, Json(json!({ "error": message }))).into_response()
    }
}

// Authentication normally redirects HTML forms. Preserve the JSON protocol when a device
// expires or when Axum rejects a malformed or oversized request before the handler runs.
async fn api_rejections(req: Request, next: Next) -> Response {
    let res = next.run(req).await;
    let status = res.status();
    if status.is_redirection() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Sign in to access this board."})),
        )
            .into_response();
    }
    if status.is_client_error()
        && !res
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v == "application/json")
    {
        let error = match status {
            StatusCode::PAYLOAD_TOO_LARGE => "This board exceeds the maximum request size.",
            StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST => "Invalid board request.",
            _ => status.canonical_reason().unwrap_or("Request failed"),
        };
        return (status, Json(json!({"error":error}))).into_response();
    }
    res
}

#[derive(FromRow)]
struct BoardSummary {
    id: Uuid,
    title: String,
    project_name: String,
    updated_at: DateTime<Utc>,
    revision: i64,
    read_only: bool,
    node_count: i32,
    edge_count: i32,
    stroke_count: i32,
}

#[derive(FromRow, Serialize)]
struct Board {
    id: Uuid,
    project: Uuid,
    title: String,
    revision: i64,
    #[sqlx(json)]
    document: Document,
    read_only: bool,
    #[serde(skip)]
    project_name: String,
}

const BOARD_SELECT: &str = "SELECT b.id,b.project_id AS project,b.title,b.revision,b.document,
    p.archived AS read_only,p.name AS project_name
    FROM canvas_boards b JOIN projects p ON (p.owner_id,p.id)=(b.owner_id,b.project_id)
    WHERE b.owner_id=$1 AND b.id=$2";
const STALE: &str = "This board changed on another device. Reload it before saving.";

#[derive(Deserialize)]
struct DirectoryFilter {
    project: Option<String>,
    page: Option<u32>,
    #[serde(default)]
    new: bool,
}
async fn directory(
    State(s): State<AppState>,
    auth: Auth,
    Query(filter): Query<DirectoryFilter>,
) -> AppResult<Html<String>> {
    let project = filter
        .project
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| AppError::BadRequest("Choose a valid project.".into()))?;
    let projects = store::projects(&s.db, auth.user_id).await?;
    if project.is_some_and(|id| !projects.iter().any(|p| p.id == id)) {
        return Err(AppError::NotFound);
    }
    if filter.new {
        return Ok(views::creation(&auth, &projects, project));
    }
    let page = i64::from(filter.page.unwrap_or(1).max(1));
    let mut boards = sqlx::query_as::<_,BoardSummary>("SELECT b.id,b.title,b.revision,b.updated_at,
         p.name AS project_name,p.archived AS read_only,
         jsonb_array_length(b.document->'nodes') AS node_count,jsonb_array_length(b.document->'edges') AS edge_count,
         jsonb_array_length(b.document->'strokes') AS stroke_count
         FROM canvas_boards b JOIN projects p ON (p.owner_id,p.id)=(b.owner_id,b.project_id)
         WHERE b.owner_id=$1 AND ($2::uuid IS NULL OR b.project_id=$2)
         ORDER BY p.archived,b.updated_at DESC,b.id LIMIT 41 OFFSET $3")
        .bind(auth.user_id).bind(project).bind((page-1)*40).fetch_all(&s.db).await?;
    let more = boards.len() > 40;
    boards.truncate(40);
    Ok(views::directory(
        &auth, &projects, project, &boards, page, more,
    ))
}

fn board_title(raw: &str) -> AppResult<&str> {
    let title = raw.trim();
    if title.is_empty() || title.chars().count() > 120 || title.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "Use a board title with 1 to 120 characters on one line.".into(),
        ));
    }
    Ok(title)
}

#[derive(Deserialize)]
struct NewBoard {
    csrf: String,
    project: Uuid,
    title: String,
    template: String,
}

async fn create(
    State(s): State<AppState>,
    auth: Auth,
    Form(f): Form<NewBoard>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let title = board_title(&f.title)?;
    let doc = Document::from_template(&f.template)?;
    let mut tx = s.db.begin().await?;
    store::lock_project(&mut tx, auth.user_id, f.project).await?;
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO canvas_boards(owner_id,project_id,title,document)
        VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(auth.user_id)
    .bind(f.project)
    .bind(title)
    .bind(sqlx::types::Json(&doc))
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/canvas/{id}")))
}

async fn board_project(db: &PgPool, owner: Uuid, id: Uuid) -> AppResult<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT project_id FROM canvas_boards WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .fetch_one(db)
            .await?,
    )
}
async fn load_board(db: &PgPool, owner: Uuid, id: Uuid) -> AppResult<Board> {
    let mut conn = db.acquire().await?;
    let mut board = sqlx::query_as::<_, Board>(BOARD_SELECT)
        .bind(owner)
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    strip_missing_sources(&mut conn, owner, board.project, &mut board.document).await?;
    Ok(board)
}
async fn lock_board(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    id: Uuid,
    revision: i64,
) -> AppResult<Board> {
    let project =
        sqlx::query_scalar("SELECT project_id FROM canvas_boards WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    store::lock_project(tx, owner, project).await?;
    let board: Board = sqlx::query_as(&format!("{BOARD_SELECT} FOR UPDATE OF b"))
        .bind(owner)
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    if board.revision != revision {
        return Err(AppError::Conflict(STALE.into()));
    }
    Ok(board)
}
async fn write_board(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    board: &mut Board,
) -> AppResult<()> {
    board.revision=sqlx::query_scalar("UPDATE canvas_boards SET title=$3,document=$4,revision=revision+1,
        updated_at=clock_timestamp() WHERE owner_id=$1 AND id=$2 AND revision=$5 RETURNING revision")
        .bind(owner).bind(board.id).bind(&board.title).bind(sqlx::types::Json(&board.document))
        .bind(board.revision).fetch_optional(&mut **tx).await?.ok_or_else(||AppError::Conflict(STALE.into()))?;
    Ok(())
}

async fn editor(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> AppResult<Html<String>> {
    let board = load_board(&s.db, auth.user_id, id).await?;
    Ok(views::editor(&auth, &board))
}

#[derive(Deserialize)]
struct Since {
    since: Option<i64>,
    read_only: Option<bool>,
}
async fn get_board(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Query(q): Query<Since>,
) -> ApiResult<Response> {
    if let (Some(since), Some(read_only)) = (q.since, q.read_only) {
        let current: (i64, bool) = sqlx::query_as(
            "SELECT b.revision,p.archived FROM canvas_boards b JOIN projects p
            ON (p.owner_id,p.id)=(b.owner_id,b.project_id) WHERE b.owner_id=$1 AND b.id=$2",
        )
        .bind(auth.user_id)
        .bind(id)
        .fetch_one(&s.db)
        .await?;
        if current == (since, read_only) {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    }
    Ok(Json(load_board(&s.db, auth.user_id, id).await?).into_response())
}

async fn existing_sources(
    conn: &mut PgConnection,
    owner: Uuid,
    project: Uuid,
    doc: &Document,
) -> AppResult<HashSet<Source>> {
    let mut issues = Vec::new();
    let mut notes = Vec::new();
    for source in doc.nodes.iter().filter_map(|n| n.source) {
        match source.kind {
            SourceKind::Issue => issues.push(source.id),
            SourceKind::Note => notes.push(source.id),
        }
    }
    if issues.is_empty() && notes.is_empty() {
        return Ok(HashSet::new());
    }
    let rows: Vec<(bool, Uuid)> = sqlx::query_as(
        "SELECT true,id FROM issues WHERE owner_id=$1 AND project_id=$2 AND id=ANY($3)
        UNION ALL SELECT false,id FROM notes WHERE owner_id=$1 AND project_id=$2 AND id=ANY($4)",
    )
    .bind(owner)
    .bind(project)
    .bind(issues)
    .bind(notes)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(issue, id)| Source {
            kind: if issue {
                SourceKind::Issue
            } else {
                SourceKind::Note
            },
            id,
        })
        .collect())
}
async fn strip_missing_sources(
    conn: &mut PgConnection,
    owner: Uuid,
    project: Uuid,
    doc: &mut Document,
) -> AppResult<()> {
    let existing = existing_sources(conn, owner, project, doc).await?;
    for node in &mut doc.nodes {
        if node.source.is_some_and(|s| !existing.contains(&s)) {
            node.source = None;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Save {
    csrf: String,
    revision: i64,
    title: String,
    document: Document,
}
async fn save(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(mut f): Json<Save>,
) -> ApiResult<Json<serde_json::Value>> {
    auth.check_csrf(&f.csrf)?;
    let title = board_title(&f.title)?.to_owned();
    f.document.validate()?;
    let mut tx = s.db.begin().await?;
    let mut board = lock_board(&mut tx, auth.user_id, id, f.revision).await?;
    let existing = existing_sources(&mut tx, auth.user_id, board.project, &f.document).await?;
    let mut detached = Vec::new();
    for node in &mut f.document.nodes {
        if let Some(pin) = node.source
            && !existing.contains(&pin)
        {
            // A deleted record becomes an ordinary diagram node. Reject additions from other
            // projects without stranding edits made while an existing pin was being removed.
            if board
                .document
                .nodes
                .iter()
                .any(|old| old.id == node.id && old.source == Some(pin))
            {
                detached.push(json!({"node":node.id,"kind":pin.kind,"id":pin.id}));
                node.source = None;
            } else {
                return Err(AppError::BadRequest(
                    "Choose an issue or note from this board’s project.".into(),
                )
                .into());
            }
        }
    }
    if board.title != title || board.document != f.document {
        board.title = title;
        board.document = f.document;
        write_board(&mut tx, auth.user_id, &mut board).await?;
    }
    tx.commit().await?;
    let mut response = json!({"revision":board.revision});
    if !detached.is_empty() {
        response["detached"] = json!(detached);
    }
    Ok(Json(response))
}

#[derive(Deserialize)]
struct DeleteBoard {
    csrf: String,
    revision: i64,
    confirmation: String,
}
async fn delete(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<DeleteBoard>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let mut tx = s.db.begin().await?;
    let board = lock_board(&mut tx, auth.user_id, id, f.revision).await?;
    if f.confirmation != board.title {
        return Err(AppError::BadRequest(
            "Enter the current board title to confirm deletion.".into(),
        ));
    }
    sqlx::query("DELETE FROM canvas_boards WHERE owner_id=$1 AND id=$2")
        .bind(auth.user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/canvas?project={}", board.project)))
}

#[derive(Deserialize)]
struct Search {
    #[serde(default)]
    q: String,
}
#[derive(FromRow, Serialize)]
struct Match {
    kind: String,
    id: Uuid,
    label: String,
}
async fn sources(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Query(q): Query<Search>,
) -> ApiResult<Json<Vec<Match>>> {
    if q.q.chars().count() > 160 || q.q.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "Use a search of up to 160 characters on one line.".into(),
        )
        .into());
    }
    let project = board_project(&s.db, auth.user_id, id).await?;
    let rows=sqlx::query_as("SELECT kind,id,label FROM (
        SELECT 'issue' AS kind,i.id,p.key||'-'||i.number||' · '||i.title AS label,i.updated_at
          FROM issues i JOIN projects p ON (p.owner_id,p.id)=(i.owner_id,i.project_id)
         WHERE i.owner_id=$1 AND i.project_id=$2 AND NOT i.archived
        UNION ALL
        SELECT 'note',n.id,n.title||' · '||n.path,n.updated_at FROM notes n WHERE n.owner_id=$1 AND n.project_id=$2
        ) records WHERE strpos(lower(label),lower($3))>0 ORDER BY updated_at DESC,id LIMIT 30")
        .bind(auth.user_id).bind(project).bind(q.q.trim()).fetch_all(&s.db).await?;
    Ok(Json(rows))
}
