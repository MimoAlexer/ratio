mod graph;
mod index;
mod markdown;
#[cfg(test)]
mod tests;
mod views;

pub use graph::issue_panel;
pub use index::{refresh_issue, reindex, unresolved_warnings};
pub use markdown::validate_path;

use crate::{AppError, AppResult, AppState, auth::Auth, store};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, Path, State},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use markdown::Document;
use serde::Deserialize;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

const MAX_SOURCE: usize = 2_000_000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/notes", get(views::list).post(create))
        .route("/notes/new", get(views::new))
        .route("/notes/suggestions", get(views::suggestions))
        .route("/notes/preview", post(views::preview))
        .route("/notes/{id}", get(views::detail).post(update))
        .route("/notes/{id}/edit", get(views::edit))
        .route(
            "/notes/{id}/delete",
            get(views::confirm_delete).post(delete),
        )
        .route("/notes/{id}/issues", post(create_issue))
        .route("/notes/{id}/links/{ordinal}", post(choose_link))
        .route("/assets/note-editor.js", get(views::editor_script))
        // A valid URL-encoded non-ASCII source may consume three wire bytes per byte.
        .layer(DefaultBodyLimit::max(MAX_SOURCE * 3 + 32_768))
        .merge(graph::routes())
}

#[derive(FromRow)]
struct Note {
    id: Uuid,
    project_id: Uuid,
    title: String,
    path: String,
    body: String,
    revision: i64,
}

#[derive(Deserialize)]
struct NoteForm {
    csrf: String,
    #[serde(default)]
    draft_token: Option<Uuid>,
    project: Uuid,
    #[serde(default)]
    note_id: Option<Uuid>,
    #[serde(default)]
    revision: Option<i64>,
    title: String,
    path: String,
    body: String,
}

#[derive(Deserialize)]
struct IssueForm {
    csrf: String,
    revision: i64,
    source_anchor: String,
    #[serde(default)]
    title: String,
    command_id: Uuid,
}

#[derive(Deserialize)]
struct Choice {
    csrf: String,
    revision: i64,
    target: Uuid,
}

async fn load_note(db: &PgPool, owner: Uuid, id: Uuid) -> AppResult<Note> {
    sqlx::query_as(
        "SELECT id,project_id,title,path,body,revision FROM notes WHERE owner_id=$1 AND id=$2",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

async fn lock_owner(tx: &mut Transaction<'_, Postgres>, owner: Uuid) -> AppResult<()> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(())
}

fn validated_title(input: &str) -> AppResult<&str> {
    let title = input.trim();
    if title.is_empty() || title.chars().count() > 200 || title.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "Use a title between 1 and 200 characters on a single line.".into(),
        ));
    }
    Ok(title)
}

fn validated_source(source: &str) -> AppResult<()> {
    if source.len() > MAX_SOURCE || source.contains('\0') {
        return Err(AppError::BadRequest(
            "Markdown must be valid text up to 2 MB with no null characters.".into(),
        ));
    }
    Ok(())
}

fn expect_revision(current: i64, submitted: Option<i64>) -> AppResult<()> {
    if submitted != Some(current) {
        return Err(AppError::Conflict(
            "The note changed after you opened it. Compare both copies before saving.".into(),
        ));
    }
    Ok(())
}

fn path_collision(error: sqlx::Error) -> AppError {
    if error
        .as_database_error()
        .is_some_and(|db| db.is_unique_violation())
    {
        AppError::Conflict("Another note already uses this path. Choose a different path.".into())
    } else {
        error.into()
    }
}

async fn save(db: &PgPool, owner: Uuid, id: Option<Uuid>, f: &NoteForm) -> AppResult<Uuid> {
    let title = validated_title(&f.title)?;
    let path = validate_path(&f.path).map_err(AppError::BadRequest)?;
    validated_source(&f.body)?;
    let mut tx = db.begin().await?;
    store::lock_project(&mut tx, owner, f.project).await?;
    let existing = if let Some(id) = id {
        let note:Note=sqlx::query_as("SELECT id,project_id,title,path,body,revision FROM notes WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE").bind(owner).bind(f.project).bind(id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        expect_revision(note.revision, f.revision)?;
        Some(note)
    } else {
        None
    };
    // Browsers normalize form newlines. Keep imported line endings on a title-only edit.
    let body = existing
        .as_ref()
        .filter(|n| {
            f.body != n.body && f.body.replace("\r\n", "\n") == n.body.replace("\r\n", "\n")
        })
        .map_or(f.body.as_str(), |n| &n.body);
    let bytes = title.len() + path.len() + body.len();
    let old = existing
        .as_ref()
        .map_or(0, |n| n.title.len() + n.path.len() + n.body.len());
    store::check_storage(&mut tx, owner, bytes as i64 - old as i64).await?;
    let id = if let Some(n) = existing.as_ref() {
        if n.title == title && n.path == path && n.body == body {
            tx.commit().await?;
            return Ok(n.id);
        }
        sqlx::query("UPDATE notes SET title=$4,path=$5,body=$6,revision=revision+1,updated_at=now() WHERE owner_id=$1 AND project_id=$2 AND id=$3")
            .bind(owner).bind(f.project).bind(n.id).bind(title).bind(&path).bind(body)
            .execute(&mut *tx).await.map_err(path_collision)?;
        n.id
    } else {
        sqlx::query_scalar("INSERT INTO notes(owner_id,project_id,title,path,body) VALUES($1,$2,$3,$4,$5) RETURNING id")
            .bind(owner).bind(f.project).bind(title).bind(&path).bind(body).fetch_one(&mut *tx).await.map_err(path_collision)?
    };
    index::refresh_note(&mut tx, owner, f.project, id).await?;
    tx.commit().await?;
    Ok(id)
}

async fn create(
    State(s): State<AppState>,
    auth: Auth,
    Form(f): Form<NoteForm>,
) -> AppResult<Response> {
    write_form(&s, &auth, None, f).await
}

async fn update(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<NoteForm>,
) -> AppResult<Response> {
    write_form(&s, &auth, Some(id), f).await
}

async fn write_form(
    s: &AppState,
    auth: &Auth,
    id: Option<Uuid>,
    f: NoteForm,
) -> AppResult<Response> {
    auth.check_csrf(&f.csrf)?;
    if let Some(id) = id {
        let note = load_note(&s.db, auth.user_id, id).await?;
        if note.project_id != f.project {
            return Err(AppError::NotFound);
        }
    }
    match save(&s.db, auth.user_id, id, &f).await {
        Ok(note) => {
            let origin = if id.is_some() { "updated" } else { "created" };
            let token = f
                .draft_token
                .map_or_else(String::new, |id| format!("&draft_token={id}"));
            Ok(Redirect::to(&format!("/notes/{note}?saved={origin}{token}")).into_response())
        }
        Err(AppError::Conflict(message)) => {
            views::recover(&s.db, auth, id, f, &message, true).await
        }
        Err(AppError::BadRequest(message)) => {
            views::recover(&s.db, auth, id, f, &message, false).await
        }
        Err(e) => Err(e),
    }
}

async fn create_issue(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<IssueForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let issue = promote(&s.db, auth.user_id, id, &f).await?;
    Ok(Redirect::to(&format!("/issues/{issue}")))
}

#[derive(FromRow)]
struct PromotionReceipt {
    note_id: Option<Uuid>,
    source_anchor: Option<String>,
    note_revision: Option<i64>,
    excerpt: Option<String>,
}

async fn promote(db: &PgPool, owner: Uuid, id: Uuid, f: &IssueForm) -> AppResult<Uuid> {
    let mut tx = db.begin().await?;
    lock_owner(&mut tx, owner).await?;
    let project: Uuid =
        sqlx::query_scalar("SELECT project_id FROM notes WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
    let replay:Option<PromotionReceipt>=sqlx::query_as(
        "SELECT s.note_id,s.source_anchor,s.note_revision,s.excerpt FROM issue_events e LEFT JOIN note_issue_sources s ON s.issue_id=e.issue_id AND s.owner_id=e.owner_id WHERE e.owner_id=$1 AND e.command_id=$2")
        .bind(owner).bind(f.command_id).fetch_optional(&mut *tx).await?;
    let already_created = replay.is_some();
    let excerpt = if let Some(prior) = replay {
        if prior.note_id != Some(id)
            || prior.source_anchor.as_deref() != Some(f.source_anchor.as_str())
            || prior.note_revision != Some(f.revision)
        {
            return Err(AppError::Conflict("This issue creation command was already used. Reopen the note to start another issue.".into()));
        }
        prior.excerpt.ok_or(AppError::NotFound)?
    } else {
        store::lock_project(&mut tx, owner, project).await?;
        let (body,revision):(String,i64)=sqlx::query_as("SELECT body,revision FROM notes WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE").bind(owner).bind(project).bind(id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        expect_revision(revision, Some(f.revision))?;
        Document::parse(&body)
            .paragraphs
            .into_iter()
            .find(|p| p.anchor == f.source_anchor)
            .ok_or_else(|| AppError::BadRequest("Select a paragraph in the current note.".into()))?
            .excerpt
    };
    let fallback = excerpt
        .split_whitespace()
        .flat_map(|word| word.chars().chain(std::iter::once(' ')))
        .take(100)
        .collect::<String>();
    let title = validated_title(if f.title.trim().is_empty() {
        &fallback
    } else {
        &f.title
    })?
    .to_owned();
    let draft = crate::issues::NewIssue {
        project,
        title,
        body: excerpt.clone(),
        status: "ready".into(),
        priority: "normal".into(),
        kind: "feature".into(),
        labels: String::new(),
        next_action: String::new(),
        severity: "none".into(),
    };
    let issue = crate::issues::create_in(&mut tx, owner, f.command_id, draft).await?;
    if !already_created {
        sqlx::query("INSERT INTO note_issue_sources(owner_id,project_id,note_id,issue_id,source_anchor,excerpt,note_revision) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(owner).bind(project).bind(id).bind(issue).bind(&f.source_anchor).bind(&excerpt).bind(f.revision).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(issue)
}

async fn choose_link(
    State(s): State<AppState>,
    auth: Auth,
    Path((id, ordinal)): Path<(Uuid, i32)>,
    Form(f): Form<Choice>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let note = load_note(&s.db, auth.user_id, id).await?;
    let mut tx = s.db.begin().await?;
    store::lock_project(&mut tx, auth.user_id, note.project_id).await?;
    let current:Note=sqlx::query_as("SELECT id,project_id,title,path,body,revision FROM notes WHERE id=$1 AND owner_id=$2 FOR UPDATE").bind(id).bind(auth.user_id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
    expect_revision(current.revision, Some(f.revision))?;
    let doc = Document::parse(&current.body);
    let reference = usize::try_from(ordinal)
        .ok()
        .and_then(|ord| doc.links.get(ord))
        .ok_or(AppError::NotFound)?;
    let catalog: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id,path,title FROM notes WHERE owner_id=$1 AND project_id=$2 ORDER BY path",
    )
    .bind(auth.user_id)
    .bind(current.project_id)
    .fetch_all(&mut *tx)
    .await?;
    if index::native_issue(&reference.target).is_some()
        || !markdown::candidates(&current.path, reference, &catalog).contains(&f.target)
    {
        return Err(AppError::BadRequest(
            "Choose one of the matching notes.".into(),
        ));
    }
    let changed=sqlx::query("UPDATE note_links SET dest_note=$4,manual=true WHERE source_id=$1 AND owner_id=$2 AND ordinal=$3 AND dest_note IS NULL AND dest_issue IS NULL AND raw=$5").bind(id).bind(auth.user_id).bind(ordinal).bind(f.target).bind(&reference.raw).execute(&mut *tx).await?;
    if changed.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "This link has already been resolved. Reopen the note to see its destination.".into(),
        ));
    }
    tx.commit().await?;
    Ok(Redirect::to(&format!("/notes/{id}#relationships")))
}

#[derive(Deserialize)]
struct DeleteForm {
    csrf: String,
    revision: i64,
    confirmation: String,
}

async fn delete(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<DeleteForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let project = remove_note(&s.db, auth.user_id, id, f.revision, &f.confirmation).await?;
    Ok(Redirect::to(&format!(
        "/notes?project={project}&deleted={id}"
    )))
}

async fn remove_note(
    db: &PgPool,
    owner: Uuid,
    id: Uuid,
    revision: i64,
    confirmation: &str,
) -> AppResult<Uuid> {
    let mut tx = db.begin().await?;
    lock_owner(&mut tx, owner).await?;
    // Reclaiming storage is available in archived or quota-limited projects.
    // Take the normal owner -> project -> item locks while allowing this specific removal.
    let project:Uuid=sqlx::query_scalar("SELECT p.id FROM projects p JOIN notes n ON (n.owner_id,n.project_id)=(p.owner_id,p.id) WHERE p.owner_id=$1 AND n.id=$2 FOR UPDATE OF p")
        .bind(owner).bind(id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
    let (path, current): (String, i64) = sqlx::query_as(
        "SELECT path,revision FROM notes WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    expect_revision(current, Some(revision))?;
    if confirmation != path {
        return Err(AppError::BadRequest(
            "Enter the current Markdown path exactly to confirm permanent deletion.".into(),
        ));
    }
    sqlx::query("DELETE FROM transfer_records WHERE owner_id=$1 AND project_id=$2 AND kind='note' AND local_id=$3")
        .bind(owner).bind(project).bind(id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM notes WHERE owner_id=$1 AND project_id=$2 AND id=$3")
        .bind(owner)
        .bind(project)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    index::refresh_after_delete(&mut tx, owner, project, id).await?;
    // Staged archives and their previews may still contain the removed text or path.
    sqlx::query("DELETE FROM transfer_previews WHERE owner_id=$1 AND project_id=$2")
        .bind(owner)
        .bind(project)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(project)
}
