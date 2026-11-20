use axum::{
    Form,
    extract::{Path, Query, State},
    response::{Html, Redirect},
};
use chrono::{DateTime, Utc};
use maud::html;
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{
    AppError, AppResult, AppState,
    auth::Auth,
    store,
    ui::{csrf, page},
};

#[derive(Deserialize)]
pub(super) struct Filter {
    project: Option<Uuid>,
    page: Option<u32>,
}

#[derive(FromRow)]
struct TrashedImage {
    id: Uuid,
    name: String,
    path: String,
    archived: bool,
    bytes: i32,
    purge_at: DateTime<Utc>,
    recoverable: bool,
}

pub(super) fn list_url(project: Option<Uuid>, page: u32) -> String {
    match project {
        Some(id) => format!("/transfer/trash?project={id}&page={page}"),
        None => format!("/transfer/trash?page={page}"),
    }
}

pub(super) async fn index(
    auth: Auth,
    State(state): State<AppState>,
    Query(filter): Query<Filter>,
) -> AppResult<Html<String>> {
    let project = match filter.project {
        Some(id) => Some(store::project(&state.db, auth.user_id, id).await?),
        None => None,
    };
    let number = filter.page.unwrap_or(1).max(1);
    let images = sqlx::query_as::<_, TrashedImage>(
        "SELECT b.id,p.name,b.path,p.archived,octet_length(b.bytes) AS bytes,
             b.trashed_at + interval '168 hours' AS purge_at,
             b.trashed_at > statement_timestamp() - interval '168 hours' AS recoverable
         FROM transfer_blobs b JOIN projects p ON p.owner_id=b.owner_id AND p.id=b.project_id
         WHERE b.owner_id=$1 AND b.trashed_at IS NOT NULL AND ($2::uuid IS NULL OR b.project_id=$2)
         ORDER BY b.trashed_at DESC,b.id LIMIT 51 OFFSET $3",
    )
    .bind(auth.user_id)
    .bind(filter.project)
    .bind((i64::from(number) - 1) * 50)
    .fetch_all(&state.db)
    .await?;
    let more = images.len() > 50;
    let back = project.as_ref().map_or_else(
        || "/transfer#images".into(),
        |p| format!("/transfer?project={}#images", p.id),
    );
    Ok(page(
        &auth,
        "Image trash",
        "transfer",
        html! {
            header {
                a href=(back) {"Import and export"}
                h1 {"Image trash"}
                @if let Some(project) = project {p {(project.name)}}
                p class="muted" {"Restore an image within seven days to reconnect its links. Trashed images count toward storage until purged."}
            }
            section class="panel stack" {
                @if images.is_empty() {p {"No images on this page."}}
                @else {ul class="issue-list" {
                    @for image in images.into_iter().take(50) {
                        li class="stack" {
                            strong {(image.path)}
                            span class="muted" {(image.name) " · " (image.bytes) " bytes · "
                                @if image.recoverable {"Recoverable until " (image.purge_at.format("%Y-%m-%d %H:%M UTC"))}
                                @else {"Awaiting permanent removal"}
                            }
                            @if image.archived {span class="muted" {"Restore the archived project to manage this image."}}
                            @else {
                                div class="toolbar" {
                                    @if image.recoverable {
                                        form method="post" action=(format!("/transfer/assets/{}/restore",image.id)) {
                                            (csrf(&auth)) button class="secondary" type="submit" aria-label=(format!("Restore {}",image.path)) {"Restore"}
                                        }
                                    }
                                    a href=(format!("/transfer/assets/{}/delete",image.id)) aria-label=(format!("Permanently delete {}",image.path)) {"Delete permanently"}
                                }
                            }
                        }
                    }
                }}
                nav class="toolbar" aria-label="Trash pages" {
                    @if number>1 {a href=(list_url(filter.project,number-1)) {"Previous images"}}
                    @if more {a href=(list_url(filter.project,number.saturating_add(1))) {"More images"}}
                }
            }
        },
    ))
}

#[derive(Deserialize)]
pub(super) struct ActionForm {
    csrf: String,
    confirm: Option<String>,
}

enum Change {
    Trash,
    Restore,
    Delete,
}

impl Change {
    async fn apply(
        self,
        state: &AppState,
        auth: &Auth,
        id: Uuid,
        form: &ActionForm,
    ) -> AppResult<Redirect> {
        auth.check_csrf(&form.csrf)?;
        if matches!(self, Self::Delete) && form.confirm.as_deref() != Some("delete") {
            return Err(AppError::BadRequest(
                "Confirm permanent image deletion.".into(),
            ));
        }
        let mut tx = state.db.begin().await?;
        let project: Uuid =
            sqlx::query_scalar("SELECT project_id FROM transfer_blobs WHERE owner_id=$1 AND id=$2")
                .bind(auth.user_id)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?;
        // The shared lock takes the owner before the project and rechecks archive status.
        store::lock_project(&mut tx, auth.user_id, project).await?;
        let trashed: Option<bool> = sqlx::query_scalar(
            "SELECT CASE WHEN trashed_at IS NULL THEN NULL ELSE trashed_at > statement_timestamp() - interval '168 hours' END
             FROM transfer_blobs WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE",
        ).bind(auth.user_id).bind(project).bind(id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        let active_url = format!("/transfer?project={project}#images");
        let trash_url = list_url(Some(project), 1);
        let target = match self {
            Self::Trash => {
                if trashed.is_none() {
                    sqlx::query("UPDATE transfer_blobs SET trashed_at=statement_timestamp() WHERE owner_id=$1 AND project_id=$2 AND id=$3")
                        .bind(auth.user_id).bind(project).bind(id).execute(&mut *tx).await?;
                }
                trash_url
            }
            Self::Restore => {
                if trashed == Some(false) {
                    return Err(AppError::Conflict(
                        "The seven-day recovery period has ended.".into(),
                    ));
                }
                if trashed == Some(true) {
                    sqlx::query("UPDATE transfer_blobs SET trashed_at=NULL WHERE owner_id=$1 AND project_id=$2 AND id=$3")
                        .bind(auth.user_id).bind(project).bind(id).execute(&mut *tx).await?;
                }
                active_url
            }
            Self::Delete => {
                sqlx::query(
                    "DELETE FROM transfer_blobs WHERE owner_id=$1 AND project_id=$2 AND id=$3",
                )
                .bind(auth.user_id)
                .bind(project)
                .bind(id)
                .execute(&mut *tx)
                .await?;
                if trashed.is_some() {
                    trash_url
                } else {
                    active_url
                }
            }
        };
        tx.commit().await?;
        Ok(Redirect::to(&target))
    }
}

pub(super) async fn move_to_trash(
    auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Form(form): Form<ActionForm>,
) -> AppResult<Redirect> {
    Change::Trash.apply(&state, &auth, id, &form).await
}
pub(super) async fn restore(
    auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Form(form): Form<ActionForm>,
) -> AppResult<Redirect> {
    Change::Restore.apply(&state, &auth, id, &form).await
}
pub(super) async fn delete(
    auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Form(form): Form<ActionForm>,
) -> AppResult<Redirect> {
    Change::Delete.apply(&state, &auth, id, &form).await
}

pub(super) async fn delete_confirmation(
    auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Html<String>> {
    let (project,path,bytes,in_trash):(Uuid,String,i32,bool)=sqlx::query_as(
        "SELECT project_id,path,octet_length(bytes),trashed_at IS NOT NULL FROM transfer_blobs WHERE owner_id=$1 AND id=$2")
        .bind(auth.user_id).bind(id).fetch_optional(&state.db).await?.ok_or(AppError::NotFound)?;
    store::writable(&state.db, auth.user_id, project).await?;
    let back = if in_trash {
        list_url(Some(project), 1)
    } else {
        format!("/transfer?project={project}#images")
    };
    Ok(page(
        &auth,
        "Delete image permanently",
        "transfer",
        html! {
            header {h1 {"Delete image permanently"}}
            section class="panel stack" {
                p {"Permanently delete " strong {(path)} " and free " (bytes) " bytes? Notes that reference this image will show a missing image."}
                form class="toolbar" method="post" action=(format!("/transfer/assets/{id}/delete")) {
                    (csrf(&auth)) input type="hidden" name="confirm" value="delete";
                    button class="danger" type="submit" {"Delete image permanently"} a href=(back) {"Keep image"}
                }
            }
        },
    ))
}

/// Remove a bounded batch of expired bytes. Owner locks serialize the storage change
/// with imports, while disconnected and archived projects still honor their retention.
pub async fn purge_expired(db: &PgPool) -> AppResult<u64> {
    let candidates:Vec<(Uuid,Uuid)>=sqlx::query_as(
        "SELECT owner_id,project_id FROM (SELECT owner_id,project_id,trashed_at FROM transfer_blobs
           WHERE trashed_at<=statement_timestamp()-interval '168 hours' ORDER BY trashed_at,id LIMIT 500) expired
         GROUP BY owner_id,project_id ORDER BY min(trashed_at)")
        .fetch_all(db).await?;
    let mut count = 0;
    for (owner, project) in candidates {
        let mut tx = db.begin().await?;
        let has_owner = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM users WHERE id=$1 FOR UPDATE SKIP LOCKED",
        )
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !has_owner {
            tx.rollback().await?;
            continue;
        }
        let has_project = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM projects WHERE owner_id=$1 AND id=$2 FOR UPDATE",
        )
        .bind(owner)
        .bind(project)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if has_project {
            count+=sqlx::query("DELETE FROM transfer_blobs WHERE owner_id=$1 AND project_id=$2 AND id IN
                (SELECT id FROM transfer_blobs WHERE owner_id=$1 AND project_id=$2 AND trashed_at<=statement_timestamp()-interval '168 hours'
                 ORDER BY trashed_at,id LIMIT $3)")
                .bind(owner).bind(project).bind(500_i64-count as i64).execute(&mut *tx).await?.rows_affected();
        }
        tx.commit().await?;
        if count >= 500 {
            break;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests;
