use super::{
    Board, board_project, lock_board, model::Document, strip_missing_sources, write_board,
};
use crate::{AppError, AppResult, AppState, auth::Auth};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, PgPool, types::Json as SqlJson};
use uuid::Uuid;

pub(crate) const NAMED_LIMIT: i64 = 30;
pub(crate) const RECOVERY_LIMIT: i64 = 5;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/canvas/{id}/checkpoints", get(list).post(create))
        .route("/api/canvas/{id}/checkpoints/{snap}/restore", post(restore))
        .route("/api/canvas/{id}/checkpoints/{snap}/delete", post(remove))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    csrf: String,
    revision: i64,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    csrf: String,
    revision: i64,
}

#[derive(Serialize)]
struct Created {
    id: Uuid,
}

#[derive(FromRow, Serialize)]
struct Checkpoint {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    automatic: bool,
}

async fn list(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> super::ApiResult<Json<Vec<Checkpoint>>> {
    Ok(Json(list_checkpoints(&state.db, auth.user_id, id).await?))
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(form): Json<Create>,
) -> super::ApiResult<Json<Created>> {
    auth.check_csrf(&form.csrf)?;
    let id = save_checkpoint(&state.db, auth.user_id, id, form.revision, &form.name).await?;
    Ok(Json(Created { id }))
}

async fn restore(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, snap)): Path<(Uuid, Uuid)>,
    Json(form): Json<Action>,
) -> super::ApiResult<Json<Board>> {
    auth.check_csrf(&form.csrf)?;
    Ok(Json(
        restore_checkpoint(&state.db, auth.user_id, id, snap, form.revision).await?,
    ))
}

async fn remove(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, snap)): Path<(Uuid, Uuid)>,
    Json(form): Json<Action>,
) -> super::ApiResult<StatusCode> {
    auth.check_csrf(&form.csrf)?;
    remove_checkpoint(&state.db, auth.user_id, id, snap, form.revision).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_checkpoints(db: &PgPool, owner: Uuid, id: Uuid) -> AppResult<Vec<Checkpoint>> {
    let project = board_project(db, owner, id).await?;
    Ok(sqlx::query_as(
        "SELECT id, name, created_at, automatic
           FROM canvas_checkpoints
          WHERE owner_id=$1 AND project_id=$2 AND board_id=$3
          ORDER BY created_at DESC, id DESC",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .fetch_all(db)
    .await?)
}

async fn save_checkpoint(
    db: &PgPool,
    owner: Uuid,
    id: Uuid,
    revision: i64,
    name: &str,
) -> AppResult<Uuid> {
    let clean_name = name.trim();
    if clean_name.is_empty()
        || clean_name.chars().count() > 80
        || name.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "Enter a checkpoint name of 1 to 80 characters on one line.".into(),
        ));
    }
    let mut tx = db.begin().await?;
    let board = lock_board(&mut tx, owner, id, revision).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM canvas_checkpoints
         WHERE owner_id=$1 AND project_id=$2 AND board_id=$3 AND NOT automatic",
    )
    .bind(owner)
    .bind(board.project)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if count >= NAMED_LIMIT {
        return Err(AppError::Conflict(format!(
            "This board has {NAMED_LIMIT} named checkpoints. Remove one before saving another."
        )));
    }
    let snap = insert_snapshot(&mut tx, owner, &board, clean_name, false).await?;
    tx.commit().await?;
    Ok(snap)
}

// Copy the locked, persisted board so the checkpoint exactly matches its saved revision.
async fn insert_snapshot(
    db: &mut PgConnection,
    owner: Uuid,
    board: &Board,
    name: &str,
    automatic: bool,
) -> AppResult<Uuid> {
    Ok(sqlx::query_scalar(
        "INSERT INTO canvas_checkpoints
            (owner_id, project_id, board_id, name, title, document, automatic, created_at)
         SELECT owner_id, project_id, id, $4, title, document, $5, clock_timestamp()
           FROM canvas_boards WHERE owner_id=$1 AND project_id=$2 AND id=$3
         RETURNING id",
    )
    .bind(owner)
    .bind(board.project)
    .bind(board.id)
    .bind(name)
    .bind(automatic)
    .fetch_one(db)
    .await?)
}

async fn restore_checkpoint(
    db: &PgPool,
    owner: Uuid,
    id: Uuid,
    snap: Uuid,
    revision: i64,
) -> AppResult<Board> {
    let mut tx = db.begin().await?;
    let mut board = lock_board(&mut tx, owner, id, revision).await?;
    let (title, SqlJson(mut document)): (String, SqlJson<Document>) = sqlx::query_as(
        "SELECT title, document FROM canvas_checkpoints
         WHERE owner_id=$1 AND project_id=$2 AND board_id=$3 AND id=$4",
    )
    .bind(owner)
    .bind(board.project)
    .bind(id)
    .bind(snap)
    .fetch_one(&mut *tx)
    .await?;
    document.validate()?;
    // A note or issue can be removed after the checkpoint. Keep its map node restorable.
    strip_missing_sources(&mut tx, owner, board.project, &mut document).await?;
    // Make room first so imported timestamps or a clock adjustment cannot evict the new backup.
    sqlx::query(
        "DELETE FROM canvas_checkpoints
          WHERE owner_id=$1 AND project_id=$2 AND board_id=$3 AND automatic
            AND id IN (SELECT id FROM canvas_checkpoints
                        WHERE owner_id=$1 AND project_id=$2 AND board_id=$3 AND automatic
                        ORDER BY created_at DESC, id DESC OFFSET $4)",
    )
    .bind(owner)
    .bind(board.project)
    .bind(id)
    .bind(RECOVERY_LIMIT - 1)
    .execute(&mut *tx)
    .await?;
    let recovery_name = format!("Before restore · revision {revision}");
    insert_snapshot(&mut tx, owner, &board, &recovery_name, true).await?;
    board.title = title;
    board.document = document;
    write_board(&mut tx, owner, &mut board).await?;
    tx.commit().await?;
    Ok(board)
}

async fn remove_checkpoint(
    db: &PgPool,
    owner: Uuid,
    id: Uuid,
    snap: Uuid,
    revision: i64,
) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let board = lock_board(&mut tx, owner, id, revision).await?;
    let automatic: bool = sqlx::query_scalar(
        "SELECT automatic FROM canvas_checkpoints
         WHERE owner_id=$1 AND project_id=$2 AND board_id=$3 AND id=$4",
    )
    .bind(owner)
    .bind(board.project)
    .bind(id)
    .bind(snap)
    .fetch_one(&mut *tx)
    .await?;
    if automatic {
        return Err(AppError::Conflict(
            "Recovery checkpoints are rotated after restores.".into(),
        ));
    }
    sqlx::query(
        "DELETE FROM canvas_checkpoints
          WHERE owner_id=$1 AND project_id=$2 AND board_id=$3 AND id=$4 AND NOT automatic",
    )
    .bind(owner)
    .bind(board.project)
    .bind(id)
    .bind(snap)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
