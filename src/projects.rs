use crate::{AppError, AppResult, AppState, auth::Auth, billing, store, ui};
use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use std::collections::HashSet;
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/projects", get(index).post(create))
        .route("/projects/{id}", get(overview))
        .route("/projects/{id}/edit", post(edit))
        .route("/projects/{id}/archive", post(archive))
        .route("/projects/{id}/unarchive", post(unarchive))
        .route("/projects/{id}/delete", post(delete))
}

#[derive(Deserialize)]
struct Fields {
    csrf: String,
    name: String,
    #[serde(default)]
    key: String,
    next: Option<String>,
}

#[derive(Deserialize)]
struct Action {
    csrf: String,
}

#[derive(Deserialize)]
struct Confirmation {
    csrf: String,
    confirmation: String,
}

const PROJECT_NAME_RULE: &str = "Use a project name with 1 to 120 characters on one line.";
const PROJECT_KEY_RULE: &str =
    "Use 1 to 12 letters or digits for the project key, starting with a letter.";
const PROJECT_KEY_REQUIRED: &str = "Enter the project key.";
const PROJECT_KEY_TAKEN: &str = "This project key is already in use. Choose another key.";

fn project_fields(name: &str, key: &str) -> Result<(String, String), &'static str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
        return Err(PROJECT_NAME_RULE);
    }
    let key = key.trim();
    if !key.is_empty() {
        if !key.starts_with(|c: char| c.is_ascii_alphabetic())
            || key.len() > 12
            || !key.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(PROJECT_KEY_RULE);
        }
        return Ok((name.to_owned(), key.to_ascii_uppercase()));
    }

    let mut words = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(|w| w.trim_start_matches(|c: char| c.is_ascii_digit()))
        .filter(|w| !w.is_empty());
    let generated = match (words.next(), words.next()) {
        (Some(first), Some(second)) => [first, second]
            .into_iter()
            .chain(words)
            .take(4)
            .filter_map(|w| w.chars().next())
            .collect::<String>()
            .to_ascii_uppercase(),
        (Some(word), None) => word
            .chars()
            .take(4)
            .collect::<String>()
            .to_ascii_uppercase(),
        _ => "PRJ".to_owned(),
    };
    Ok((name.to_owned(), generated))
}

async fn lock_owner(tx: &mut Transaction<'_, Postgres>, owner: Uuid) -> AppResult<String> {
    sqlx::query_scalar("SELECT plan FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)
}

async fn require_project_slot(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    plan: &str,
) -> AppResult<()> {
    let active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM projects WHERE owner_id=$1 AND NOT archived")
            .bind(owner)
            .fetch_one(&mut **tx)
            .await?;
    if active >= store::limits(plan).projects {
        return Err(AppError::Conflict(
            "All active project slots are in use. Archive a project to free a slot.".into(),
        ));
    }
    Ok(())
}

fn project_write_error(error: sqlx::Error) -> AppError {
    if error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation())
    {
        AppError::Conflict(PROJECT_KEY_TAKEN.into())
    } else {
        error.into()
    }
}

async fn available_key(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    base: &str,
) -> AppResult<String> {
    // A generated base has at most four ASCII characters. The owner lock guards allocation.
    let used: HashSet<String> =
        sqlx::query_scalar("SELECT key FROM projects WHERE owner_id=$1 AND starts_with(key,$2)")
            .bind(owner)
            .bind(base)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .collect();
    if !used.contains(base) {
        return Ok(base.to_owned());
    }
    for suffix in 2..=used.len() + 1 {
        let candidate = format!("{base}{suffix}");
        if candidate.len() <= 12 && !used.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(AppError::Conflict("Enter a distinct project key.".into()))
}

async fn create_project(db: &PgPool, owner: Uuid, name: &str, key: &str) -> AppResult<Uuid> {
    let (name, base) = project_fields(name, key).map_err(|err| AppError::BadRequest(err.into()))?;
    let mut tx = db.begin().await?;
    let plan = lock_owner(&mut tx, owner).await?;
    require_project_slot(&mut tx, owner, &plan).await?;
    let key = if key.trim().is_empty() {
        available_key(&mut tx, owner, &base).await?
    } else {
        base
    };
    let id = sqlx::query_scalar(
        "INSERT INTO projects (owner_id,name,key) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind(owner)
    .bind(name)
    .bind(key)
    .fetch_one(&mut *tx)
    .await
    .map_err(project_write_error)?;
    tx.commit().await?;
    Ok(id)
}

async fn edit_project(db: &PgPool, owner: Uuid, id: Uuid, name: &str, key: &str) -> AppResult<()> {
    if key.trim().is_empty() {
        return Err(AppError::BadRequest(PROJECT_KEY_REQUIRED.into()));
    }
    let (name, key) = project_fields(name, key).map_err(|err| AppError::BadRequest(err.into()))?;
    let mut tx = db.begin().await?;
    store::lock_project(&mut tx, owner, id).await?;
    let retry_links: bool = sqlx::query_scalar(
        "SELECT key <> $3 AND EXISTS (
             SELECT FROM note_links WHERE owner_id=$1 AND project_id=$2
               AND kind='wiki' AND dest_note IS NULL AND dest_issue IS NULL
               AND starts_with(upper(target), $3 || '-')
         ) FROM projects WHERE owner_id=$1 AND id=$2",
    )
    .bind(owner)
    .bind(id)
    .bind(&key)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE projects SET name=$3,key=$4 WHERE owner_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .bind(name)
        .bind(key)
        .execute(&mut *tx)
        .await
        .map_err(project_write_error)?;
    if retry_links {
        crate::notes::reindex(&mut tx, owner, id).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn set_archived(db: &PgPool, owner: Uuid, id: Uuid, archived: bool) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let plan = lock_owner(&mut tx, owner).await?;
    let current: bool =
        sqlx::query_scalar("SELECT archived FROM projects WHERE owner_id=$1 AND id=$2 FOR UPDATE")
            .bind(owner)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
    if current != archived {
        if !archived {
            require_project_slot(&mut tx, owner, &plan).await?;
        }
        sqlx::query("UPDATE projects SET archived=$3 WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .bind(archived)
            .execute(&mut *tx)
            .await?;
        billing::enforce_repo_cap(&mut tx, owner, &plan).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn delete_project(db: &PgPool, owner: Uuid, id: Uuid, confirmation: &str) -> AppResult<()> {
    let mut tx = db.begin().await?;
    lock_owner(&mut tx, owner).await?;
    let current_key: String =
        sqlx::query_scalar("SELECT key FROM projects WHERE owner_id=$1 AND id=$2 FOR UPDATE")
            .bind(owner)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
    if confirmation != current_key {
        return Err(AppError::BadRequest(
            "Type the current project key exactly to confirm deletion.".into(),
        ));
    }
    sqlx::query("DELETE FROM projects WHERE owner_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(FromRow)]
struct Counts {
    open_issues: i64,
    completed_issues: i64,
    notes: i64,
    milestones: i64,
    releases: i64,
    flagged: i64,
    blocked: i64,
    milestone_id: Option<Uuid>,
    milestone_title: Option<String>,
    milestone_target: Option<chrono::NaiveDate>,
}

async fn counts(db: &PgPool, owner: Uuid, id: Uuid) -> AppResult<Counts> {
    sqlx::query_as("SELECT i.open_issues, i.completed_issues, i.flagged,
          m.id AS milestone_id, m.title AS milestone_title, m.target_on AS milestone_target,
          (SELECT count(DISTINCT b.issue_id) FROM issue_blockers b
             JOIN issues waiting ON (waiting.owner_id,waiting.project_id,waiting.id)=(b.owner_id,b.project_id,b.issue_id)
             JOIN issues blocker ON (blocker.owner_id,blocker.project_id,blocker.id)=(b.owner_id,b.project_id,b.blocker_id)
            WHERE b.owner_id=p.owner_id AND b.project_id=p.id
              AND NOT waiting.archived AND waiting.status IN ('backlog','ready','in_progress')
              AND NOT blocker.archived AND blocker.status IN ('backlog','ready','in_progress')) AS blocked,
          (SELECT count(*) FROM notes WHERE owner_id=p.owner_id AND project_id=p.id) AS notes,
          (SELECT count(*) FROM milestones WHERE owner_id=p.owner_id AND project_id=p.id) AS milestones,
          (SELECT count(*) FROM releases WHERE owner_id=p.owner_id AND project_id=p.id) AS releases
        FROM projects p CROSS JOIN LATERAL (
          SELECT count(*) FILTER (WHERE status IN ('backlog','ready','in_progress')) AS open_issues,
                 count(*) FILTER (WHERE status='done') AS completed_issues,
                 count(*) FILTER (WHERE security_critical AND NOT finding_closed) AS flagged
          FROM issues WHERE owner_id=p.owner_id AND project_id=p.id AND NOT archived
        ) AS i LEFT JOIN LATERAL (
          SELECT id,title,target_on FROM milestones
           WHERE owner_id=p.owner_id AND project_id=p.id AND status<>'reached' AND target_on IS NOT NULL
           ORDER BY target_on, id LIMIT 1
        ) AS m ON true WHERE p.owner_id=$1 AND p.id=$2")
        .bind(owner).bind(id).fetch_optional(db).await?.ok_or(AppError::NotFound)
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Form(form): Form<Fields>,
) -> AppResult<Response> {
    auth.check_csrf(&form.csrf)?;
    let id = match create_project(&state.db, auth.user_id, &form.name, &form.key).await {
        Ok(id) => id,
        Err(error) => return project_form_error(&auth, None, &form, error),
    };
    let target = if form.next.as_deref() == Some("first-issue") {
        format!("/issues?project={id}#new-issue")
    } else {
        format!("/projects/{id}")
    };
    Ok(Redirect::to(&target).into_response())
}

async fn edit(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(form): Form<Fields>,
) -> AppResult<Response> {
    auth.check_csrf(&form.csrf)?;
    match edit_project(&state.db, auth.user_id, id, &form.name, &form.key).await {
        Ok(()) => Ok(Redirect::to(&format!("/projects/{id}")).into_response()),
        Err(error) => project_form_error(&auth, Some(id), &form, error),
    }
}

async fn archive(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(form): Form<Action>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    set_archived(&state.db, auth.user_id, id, true).await?;
    Ok(Redirect::to(&format!("/projects/{id}")))
}

async fn unarchive(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(form): Form<Action>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    set_archived(&state.db, auth.user_id, id, false).await?;
    Ok(Redirect::to(&format!("/projects/{id}")))
}

async fn delete(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(form): Form<Confirmation>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    delete_project(&state.db, auth.user_id, id, &form.confirmation).await?;
    Ok(Redirect::to("/projects"))
}

fn project_form_error(
    auth: &Auth,
    id: Option<Uuid>,
    fields: &Fields,
    error: AppError,
) -> AppResult<Response> {
    let (status, field, message) = match &error {
        AppError::BadRequest(message) if message == PROJECT_NAME_RULE => {
            (StatusCode::BAD_REQUEST, "name", message)
        }
        AppError::BadRequest(message)
            if message == PROJECT_KEY_RULE || message == PROJECT_KEY_REQUIRED =>
        {
            (StatusCode::BAD_REQUEST, "key", message)
        }
        AppError::Conflict(message) if message == PROJECT_KEY_TAKEN => {
            (StatusCode::CONFLICT, "key", message)
        }
        _ => return Err(error),
    };
    let title = if id.is_some() {
        "Edit project"
    } else {
        "New project"
    };
    let first_issue = fields.next.as_deref() == Some("first-issue");
    let body = html! {
        header.view-head { h1 { (title) } }
        (project_form(auth, id, &fields.name, &fields.key, first_issue, Some((field,message))))
    };
    Ok((
        status,
        ui::page_in_project(auth, title, "projects", body, id),
    )
        .into_response())
}

fn project_form(
    auth: &Auth,
    id: Option<Uuid>,
    name: &str,
    key: &str,
    first_issue: bool,
    error: Option<(&str, &str)>,
) -> Markup {
    let action = id.map_or_else(
        || "/projects".to_owned(),
        |id| format!("/projects/{id}/edit"),
    );
    let name_error = error.filter(|(field, _)| *field == "name");
    let key_error = error.filter(|(field, _)| *field == "key");
    html! {
        form.form-stack.project-form method="post" action=(action) {
            (ui::csrf(auth))
            @if first_issue { input type="hidden" name="next" value="first-issue"; }
            label { "Project name"
                input name="name" type="text" maxlength="120" required value=(name)
                    aria-invalid=[name_error.map(|_|"true")] aria-describedby=[name_error.map(|_|"project-name-error")] autofocus[name_error.is_some()];
            }
            @if let Some((_, message)) = name_error { p.project-field-error id="project-name-error" { (message) } }
            label { "Project key" @if id.is_none() { " (optional)" }
                input name="key" type="text" maxlength="12" pattern="[A-Za-z][A-Za-z0-9]{0,11}" title="1 to 12 letters or digits, starting with a letter"
                    required[id.is_some()] value=(key) aria-describedby=(if key_error.is_some() { "project-key-error project-key-hint" } else { "project-key-hint" })
                    aria-invalid=[key_error.map(|_|"true")] autofocus[key_error.is_some()];
            }
            @if let Some((_, message)) = key_error { p.project-field-error id="project-key-error" { (message) } }
            small.muted id="project-key-hint" {
                @if id.is_some() { "Changing the key updates every issue reference in this project." }
                @else { "Leave blank to generate a key from the name. The key appears before each issue number." }
            }
            div.view-actions.project-form-actions {
                button.action-primary type="submit" { @if id.is_some() { "Save changes" } @else { "Create project" } }
                @if error.is_some() {
                    a.project-text-link href=(id.map_or_else(|| "/projects".to_owned(), |id| format!("/projects/{id}"))) { "Cancel" }
                }
            }
        }
    }
}

#[derive(FromRow)]
struct DirectoryEntry {
    id: Uuid,
    name: String,
    key: String,
    archived: bool,
    open_issues: i64,
    completed_issues: i64,
    milestone_id: Option<Uuid>,
    milestone_title: Option<String>,
    milestone_target: Option<chrono::NaiveDate>,
    current_issue: Option<Uuid>,
    current_number: Option<i64>,
    current_title: Option<String>,
}

async fn directory(db: &PgPool, owner: Uuid) -> AppResult<Vec<DirectoryEntry>> {
    Ok(sqlx::query_as(
        "SELECT p.id, p.name, p.key, p.archived,
                coalesce(i.open_issues, 0) AS open_issues,
                coalesce(i.completed_issues, 0) AS completed_issues,
                m.id AS milestone_id, m.title AS milestone_title, m.target_on AS milestone_target,
                current_issue.id AS current_issue, current_issue.number AS current_number, current_issue.title AS current_title
           FROM projects p
           LEFT JOIN (
               SELECT project_id,
                      count(*) FILTER (WHERE status IN ('backlog','ready','in_progress')) AS open_issues,
                      count(*) FILTER (WHERE status='done') AS completed_issues
                 FROM issues WHERE owner_id=$1 AND NOT archived GROUP BY project_id
           ) i ON i.project_id=p.id
           LEFT JOIN LATERAL (
               SELECT id, title, target_on FROM milestones
                WHERE owner_id=p.owner_id AND project_id=p.id
                  AND status<>'reached' AND target_on IS NOT NULL
                ORDER BY target_on, id LIMIT 1
           ) m ON true
           LEFT JOIN LATERAL (
               SELECT id, number, title FROM issues
                WHERE owner_id=p.owner_id AND project_id=p.id AND NOT archived AND status='in_progress'
                ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END,
                         updated_at DESC, id LIMIT 1
           ) current_issue ON true
          WHERE p.owner_id=$1 ORDER BY p.archived, p.name, p.id",
    )
    .bind(owner)
    .fetch_all(db)
    .await?)
}

fn issue_progress(open: i64, completed: i64) -> Markup {
    let total = open + completed;
    if total == 0 {
        return html! { span.project-no-work { "No tracked issues" } };
    }
    html! {
        div.project-progress {
            progress value=(completed) max=(total) aria-label=(format!("{completed} of {total} issues completed, excluding canceled and archived issues")) {}
            span { (completed) " of " (total) " done" }
        }
    }
}

fn milestone_link(
    project: Uuid,
    id: Option<Uuid>,
    title: Option<&str>,
    target: Option<chrono::NaiveDate>,
) -> Markup {
    match (id, title, target) {
        (Some(id), Some(title), Some(date)) => html! {
            a.project-milestone href=(format!("/roadmap?project={project}&milestone={id}#milestone-detail")) {
                span { (title) }
                time datetime=(date) { (date.format("%b %-d, %Y")) }
            }
        },
        _ => html! { span.project-no-work { "Unscheduled" } },
    }
}

fn project_list(projects: &[&DirectoryEntry], archived: bool) -> Markup {
    let count = projects.iter().filter(|p| p.archived == archived).count();
    if count == 0 {
        return html! {};
    }
    let title = if archived {
        "Archived projects"
    } else {
        "Active projects"
    };
    html! {
        section.project-group {
            h2 { (title) span.project-count { (count) } }
            table.project-directory role="table" aria-label=(title) {
                thead role="rowgroup" { tr role="row" {
                    @for label in ["Project", "In progress", "Issue progress", "Next milestone"] {
                        th role="columnheader" scope="col" { (label) }
                    }
                } }
                tbody role="rowgroup" {
                    @for p in projects.iter().filter(|p| p.archived == archived) {
                        tr.project-row role="row" {
                            th role="rowheader" scope="row" {
                                a.project-name href=(format!("/projects/{}", p.id)) {
                                    (ui::icon("projects"))
                                    span { strong { (&p.name) } code { (&p.key) } }
                                }
                            }
                            td.project-directory-current role="cell" {
                                span.project-mobile-label aria-hidden="true" { "In progress" }
                                @if let (Some(id), Some(number), Some(title)) = (p.current_issue, p.current_number, &p.current_title) {
                                    a.project-current-link href=(format!("/issues/{id}")) {
                                        span { (title) }
                                        code { (&p.key) "-" (number) }
                                    }
                                } @else {
                                    span.project-no-work { "None" }
                                }
                            }
                            td.project-directory-progress role="cell" { (issue_progress(p.open_issues, p.completed_issues)) }
                            td.project-directory-milestone role="cell" {
                                span.project-mobile-label aria-hidden="true" { "Next milestone" }
                                (milestone_link(p.id, p.milestone_id, p.milestone_title.as_deref(), p.milestone_target))
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct DirectoryQuery {
    #[serde(default)]
    q: String,
}

async fn index(
    State(state): State<AppState>,
    auth: Auth,
    Query(query): Query<DirectoryQuery>,
) -> AppResult<Html<String>> {
    let projects = directory(&state.db, auth.user_id).await?;
    let plan: String = sqlx::query_scalar("SELECT plan FROM users WHERE id=$1")
        .bind(auth.user_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let limit = store::limits(&plan).projects;
    let active = projects.iter().filter(|p| !p.archived).count();
    let at_capacity = active as i64 >= limit;
    let search = query.q.trim();
    let needle = search.to_lowercase();
    let filtered: Vec<_> = projects
        .iter()
        .filter(|p| {
            needle.is_empty()
                || p.name.to_lowercase().contains(&needle)
                || p.key.to_lowercase().contains(&needle)
        })
        .collect();
    Ok(ui::section_page(
        &auth,
        "Projects",
        "projects",
        html! {
            div.project-index-bar {
                header.view-head.project-index-head { h1 { "Projects" } }
                @if !at_capacity && !projects.is_empty() {
                    details.project-create {
                        summary.action-primary { (ui::icon("plus")) "New project" }
                        div id="new-project" { (project_form(&auth, None, "", "", false, None)) }
                    }
                }
            }
            @if projects.is_empty() {
                section.project-first {
                    h2 { "Create your first project" }
                    p { "Group your issues, notes and milestones under one project key." }
                    div id="new-project" { (project_form(&auth, None, "", "", false, None)) }
                }
            } @else {
                @if projects.len() > 1 || !search.is_empty() {
                    form.project-search method="get" action="/projects" role="search" aria-label="Project directory" {
                        label { (ui::icon("search"))
                            input type="search" name="q" aria-label="Search projects by name or key" placeholder="Find by name or key" value=(search);
                        }
                        button.action-secondary type="submit" { "Search" }
                        @if !search.is_empty() { a.action-quiet href="/projects" { "Clear search" } }
                    }
                }
                @if filtered.is_empty() {
                    section.empty-state.project-empty { h2 { "No matching projects" } p { "Try another name or project key." } }
                } @else {
                    (project_list(&filtered, false))
                    (project_list(&filtered, true))
                }
                @if at_capacity {
                    details.project-capacity {
                        summary { (active) " of " (limit) " active project slots used" }
                        p { "Archive a project to free a slot, or " a href="/billing" { "review your plan" } "." }
                    }
                } @else {
                    p.project-capacity { (active) " of " (limit) " active project slots used" }
                }
            }
        },
        None,
    ))
}

#[derive(FromRow)]
struct QueueIssue {
    id: Uuid,
    number: i64,
    title: String,
    status: String,
    next_action: String,
}

async fn project_queue(db: &PgPool, owner: Uuid, project: Uuid) -> AppResult<Vec<QueueIssue>> {
    Ok(sqlx::query_as(
        "SELECT id, number, title, status, next_action FROM issues
          WHERE owner_id=$1 AND project_id=$2 AND NOT archived
            AND status IN ('in_progress', 'ready', 'backlog')
          ORDER BY CASE status WHEN 'in_progress' THEN 0 WHEN 'ready' THEN 1 ELSE 2 END,
                   CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END,
                   updated_at DESC, id LIMIT 6",
    )
    .bind(owner)
    .bind(project)
    .fetch_all(db)
    .await?)
}

async fn overview(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> AppResult<Html<String>> {
    let project = store::project(&state.db, auth.user_id, id).await?;
    let (totals, queue) = tokio::try_join!(
        counts(&state.db, auth.user_id, id),
        project_queue(&state.db, auth.user_id, id)
    )?;
    let issues = if project.archived {
        format!("/issues?project={id}&scope=archived")
    } else {
        format!("/issues?project={id}")
    };
    let can_restore =
        if project.archived {
            let (plan, active): (String, i64) = sqlx::query_as(
            "SELECT u.plan, (SELECT count(*) FROM projects WHERE owner_id=u.id AND NOT archived)
             FROM users u WHERE u.id=$1"
        ).bind(auth.user_id).fetch_one(&state.db).await?;
            active < store::limits(&plan).projects
        } else {
            false
        };
    let overview_path = format!("/projects/{id}");
    Ok(ui::page_in_project(
        &auth,
        &project.name,
        "projects",
        html! {
            header.view-head.project-detail-head {
                div {
                    a.project-back href="/projects" { "Projects" }
                    div.project-title-line { h1 { (&project.name) } span.project-id { code { (&project.key) } } }
                    @if project.archived { p.project-archive-state { "Archived · Read-only" } }
                }
                @if !project.archived {
                    a.button.action-primary href=(format!("/issues?project={id}#new-issue")) { (ui::icon("plus")) "New issue" }
                }
            }
            nav.project-tabs aria-label="Project views" {
                a href=(&overview_path) aria-current="page" { "Overview" }
                a href=(&issues) { "Issues" }
                a href=(format!("/notes?project={id}")) { "Notes" span { (totals.notes) } }
                a href=(format!("/roadmap?project={id}")) { "Roadmap" span { (totals.milestones) } }
                a href=(format!("/releases?project={id}")) { "Releases" span { (totals.releases) } }
            }
            div.project-overview {
                section.project-work aria-labelledby="project-issues-title" {
                    header.section-head.project-section-head {
                        h2 id="project-issues-title" { "Issue queue" span.project-count { (totals.open_issues) } }
                        a.project-text-link href=(format!("{issues}&view=board")) { "View board" (ui::icon("arrow")) }
                    }
                    @if let Some(first) = queue.first() {
                        article.project-current {
                            a.project-current-issue href=(format!("/issues/{}", first.id)) {
                                p.project-issue-line {
                                    code { (&project.key) "-" (first.number) }
                                    span { (queue_state(&first.status)) }
                                }
                                h3 { (&first.title) }
                            }
                            @if !first.next_action.trim().is_empty() {
                                div.project-next-action { h4 { "Next action" } p { (&first.next_action) } }
                            }
                        }
                    } @else {
                        div.empty-state.project-empty {
                            h3 { "No open issues" }
                            @if totals.completed_issues > 0 {
                                a.project-text-link href=(format!("{issues}&status=done")) { "View completed work" (ui::icon("arrow")) }
                            } @else if !project.archived {
                                p { "Add an issue to start this project’s queue." }
                            }
                        }
                    }
                }
                aside.project-brief aria-label="Project summary" {
                    header.project-brief-head {
                        h2 { "Project summary" }
                        a.project-text-link href=(format!("/insights?project={id}")) { "Insights" (ui::icon("arrow")) }
                    }
                    dl {
                        div { dt { "Issue progress" } dd { (issue_progress(totals.open_issues, totals.completed_issues)) } }
                        div { dt { "Next milestone" } dd {
                            (milestone_link(id, totals.milestone_id, totals.milestone_title.as_deref(), totals.milestone_target))
                        } }
                        div { dt { a href=(format!("/health?project={id}")) { "Project health" (ui::icon("arrow")) } }
                            dd.project-signal-list {
                                @if totals.flagged > 0 {
                                    a.project-flag href=(format!("/issues?project={id}&critical=open&scope=all")) {
                                        (totals.flagged) @if totals.flagged == 1 { " open critical flag" } @else { " open critical flags" } (ui::icon("arrow"))
                                    }
                                } @else { span { "No open critical flags" } }
                                span { @if totals.blocked == 0 { "No blocked issues" } @else { (totals.blocked) @if totals.blocked == 1 { " blocked issue" } @else { " blocked issues" } } }
                            }
                        }
                    }
                }
                @if queue.len() > 1 {
                    section.project-other-work aria-labelledby="project-other-title" {
                        h2 id="project-other-title" { "Other open issues" }
                        ul.project-queue {
                            @for issue in &queue[1..] {
                                li { a href=(format!("/issues/{}", issue.id)) {
                                    code { (&project.key) "-" (issue.number) }
                                    span.project-queue-title { (&issue.title) }
                                    span.project-issue-state { (queue_state(&issue.status)) }
                                } }
                            }
                        }
                        @if totals.open_issues > queue.len() as i64 {
                            a.project-text-link.project-all href=(&issues) { "View all issues" (ui::icon("arrow")) }
                        }
                    }
                }
            }
            details.project-settings {
                summary { "Project settings" }
                div.project-settings-content {
                    a.project-text-link href=(format!("/transfer/export/{id}")) { "Export project" (ui::icon("arrow")) }
                    @if project.archived {
                        @if can_restore {
                            p { "Reactivating uses an active project slot. Resume paused repositories in " a href="/integrations" { "Integrations" } "." }
                            form method="post" action=(format!("/projects/{id}/unarchive")) {
                                (ui::csrf(&auth)) button.action-secondary type="submit" { "Reactivate project" }
                            }
                        } @else {
                            p { "All active project slots are in use. " a href="/projects" { "Archive another project" } " or " a href="/billing" { "review your plan" } " to reactivate this one." }
                        }
                    } @else {
                        details { summary { "Edit project" } (project_form(&auth, Some(id), &project.name, &project.key, false, None)) }
                        details {
                            summary { "Archive project" }
                            p { "Archiving frees a project slot and pauses repository sync. Its work stays available to read and export." }
                            form method="post" action=(format!("/projects/{id}/archive")) {
                                (ui::csrf(&auth)) button.action-secondary type="submit" { "Archive project" }
                            }
                        }
                    }
                    details {
                        summary { "Delete project" }
                        p { "Permanently delete this project and its issues, notes, releases, files and history. This also disconnects its linked repositories." }
                        form.form-stack.project-form method="post" action=(format!("/projects/{id}/delete")) {
                            (ui::csrf(&auth))
                            label { "Type " code { (&project.key) } " to confirm"
                                input type="text" name="confirmation" maxlength="12" autocomplete="off" required;
                            }
                            button.danger type="submit" { "Permanently delete project" }
                        }
                    }
                }
            }
        },
        Some(id),
    ))
}

fn queue_state(status: &str) -> &'static str {
    match status {
        "in_progress" => "In progress",
        "ready" => "Ready",
        _ => "Backlog",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn directory_and_queue_use_the_current_work_in_the_owners_projects(db: PgPool) {
        let owner = Uuid::new_v4();
        let outsider = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users(id,email) VALUES($1,'one@example.test'),($2,'two@example.test')",
        )
        .bind(owner)
        .bind(outsider)
        .execute(&db)
        .await
        .unwrap();
        let project = create_project(&db, owner, "Main", "MAIN").await.unwrap();
        create_project(&db, outsider, "Private", "MAIN")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO issues(owner_id,project_id,number,title,status,priority,archived)
             VALUES($1,$2,1,'Later','backlog','urgent',false),($1,$2,2,'Now','in_progress','low',false),
                   ($1,$2,3,'Shipped','done','normal',false),($1,$2,4,'Old','in_progress','urgent',true),
                   ($1,$2,5,'Abandoned','canceled','high',false)",
        )
        .bind(owner).bind(project).execute(&db).await.unwrap();
        sqlx::query(
            "INSERT INTO milestones(owner_id,project_id,title,target_on,status)
             VALUES($1,$2,'Released','2026-03-01','reached'),($1,$2,'Beta','2026-05-01','planned'),
                   ($1,$2,'Prototype','2026-04-01','in_progress'),($1,$2,'Unscheduled',NULL,'planned')",
        )
        .bind(owner).bind(project).execute(&db).await.unwrap();

        sqlx::query(
            "INSERT INTO issue_blockers(owner_id,project_id,issue_id,blocker_id)
             SELECT i.owner_id,i.project_id,i.id,b.id FROM issues i JOIN issues b USING(owner_id,project_id)
              WHERE i.owner_id=$1 AND i.project_id=$2 AND i.number=1 AND b.number=2"
        ).bind(owner).bind(project).execute(&db).await.unwrap();
        sqlx::query("UPDATE issues SET security_critical=true WHERE owner_id=$1 AND project_id=$2 AND number=2")
            .bind(owner).bind(project).execute(&db).await.unwrap();

        let summary = counts(&db, owner, project).await.unwrap();
        assert_eq!((summary.blocked, summary.flagged), (1, 1));
        assert_eq!(summary.milestone_title.as_deref(), Some("Prototype"));
        let rows = directory(&db, owner).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, project);
        assert_eq!((row.open_issues, row.completed_issues), (2, 1));
        assert_eq!(row.milestone_title.as_deref(), Some("Prototype"));
        assert_eq!(row.current_number, Some(2));
        assert_eq!(row.current_title.as_deref(), Some("Now"));
        let queue = project_queue(&db, owner, project).await.unwrap();
        assert_eq!(queue.iter().map(|i| i.number).collect::<Vec<_>>(), [2, 1]);
        assert!(
            project_queue(&db, outsider, project)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn correctable_form_errors_keep_values_focus_and_the_first_issue_destination() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            email: "local@example.test".into(),
            csrf: "test".into(),
        };
        let fields = Fields {
            csrf: auth.csrf.clone(),
            name: "Recovery & Docs".into(),
            key: "DOC".into(),
            next: Some("first-issue".into()),
        };
        let response = project_form_error(
            &auth,
            None,
            &fields,
            AppError::Conflict(PROJECT_KEY_TAKEN.into()),
        )
        .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), 256_000)
            .await
            .unwrap();
        let html = std::str::from_utf8(&bytes).unwrap();
        assert!(html.contains("Recovery &amp; Docs"));
        assert!(html.contains(r#"name="next" value="first-issue""#));
        let key_input = html
            .split(r#"name="key""#)
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert!(key_input.contains(r#"value="DOC""#));
        assert!(key_input.contains(r#"aria-invalid="true""#));
        assert!(key_input.contains("autofocus"));
        assert!(html.contains(PROJECT_KEY_TAKEN));
    }

    #[test]
    fn explicit_keys_are_trimmed_uppercased_and_validated() {
        assert_eq!(
            project_fields("  Café  ", " x9 ").unwrap(),
            ("Café".into(), "X9".into())
        );
        for bad in ["9ABC", "a-b", "é", "ABCDEFGHIJKLM", "a b", "a\0b"] {
            assert!(project_fields("Project", bad).is_err(), "{bad:?}");
        }
        assert!(project_fields("Project", "x").is_ok());
        assert!(project_fields("Project", "A23456789012").is_ok());
    }

    #[test]
    fn blank_key_is_generated_and_unicode_names_remain_usable() {
        assert_eq!(project_fields("Weather Station", "").unwrap().1, "WS");
        assert_eq!(project_fields("Weather", " ").unwrap().1, "WEAT");
        assert_eq!(project_fields("雪の町", "").unwrap().1, "PRJ");
    }

    #[test]
    fn names_are_validated_by_characters() {
        assert!(project_fields(" ", "A").is_err());
        assert!(project_fields("line\nbreak", "A").is_err());
        assert!(project_fields(&"雪".repeat(121), "A").is_err());
        assert_eq!(
            project_fields(&"雪".repeat(120), "A")
                .unwrap()
                .0
                .chars()
                .count(),
            120
        );
    }
}

#[cfg(test)]
mod db_tests;
