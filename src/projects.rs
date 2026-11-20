use crate::{AppError, AppResult, AppState, auth::Auth, billing, store, ui};
use axum::{
    Form, Router,
    extract::{Path, State},
    response::{Html, Redirect},
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

fn project_fields(name: &str, key: &str) -> Result<(String, String), &'static str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
        return Err("Use a project name with 1 to 120 characters on one line.");
    }
    let key = key.trim();
    if !key.is_empty() {
        if !key.starts_with(|c: char| c.is_ascii_alphabetic())
            || key.len() > 12
            || !key.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(
                "Use 1 to 12 letters or digits for the project key, starting with a letter.",
            );
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
        AppError::Conflict("This project key is already in use. Choose another key.".into())
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
        return Err(AppError::BadRequest("Enter the project key.".into()));
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
}

async fn counts(db: &PgPool, owner: Uuid, id: Uuid) -> AppResult<Counts> {
    sqlx::query_as("SELECT i.open_issues, i.completed_issues,
          (SELECT count(*) FROM notes WHERE owner_id=p.owner_id AND project_id=p.id) AS notes,
          (SELECT count(*) FROM milestones WHERE owner_id=p.owner_id AND project_id=p.id) AS milestones,
          (SELECT count(*) FROM releases WHERE owner_id=p.owner_id AND project_id=p.id) AS releases
        FROM projects p CROSS JOIN LATERAL (
          SELECT count(*) FILTER (WHERE status IN ('backlog','ready','in_progress')) AS open_issues,
                 count(*) FILTER (WHERE status='done') AS completed_issues
          FROM issues WHERE owner_id=p.owner_id AND project_id=p.id AND NOT archived
        ) AS i WHERE p.owner_id=$1 AND p.id=$2")
        .bind(owner).bind(id).fetch_optional(db).await?.ok_or(AppError::NotFound)
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Form(form): Form<Fields>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let id = create_project(&state.db, auth.user_id, &form.name, &form.key).await?;
    let target = if form.next.as_deref() == Some("first-issue") {
        format!("/issues?project={id}#new-issue")
    } else {
        format!("/projects/{id}")
    };
    Ok(Redirect::to(&target))
}

async fn edit(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(form): Form<Fields>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    edit_project(&state.db, auth.user_id, id, &form.name, &form.key).await?;
    Ok(Redirect::to(&format!("/projects/{id}")))
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

fn project_form(auth: &Auth, project: Option<&store::Project>) -> Markup {
    let action = project.map_or_else(
        || "/projects".to_owned(),
        |p| format!("/projects/{}/edit", p.id),
    );
    html! {
        form.stack method="post" action=(action) {
            (ui::csrf(auth))
            label { "Project name" input name="name" type="text" maxlength="120" required value=(project.map_or("", |p| &p.name)); }
            label { "Project key" @if project.is_none() { " (optional)" }
                input name="key" type="text" maxlength="12" pattern="[A-Za-z][A-Za-z0-9]{0,11}" title="1 to 12 letters or digits, starting with a letter"
                    required[project.is_some()] value=(project.map_or("", |p| &p.key)) aria-describedby="project-key-hint";
            }
            @if project.is_some() {
                small.muted id="project-key-hint" { "The key appears before every issue number. Changing it changes those references." }
            } @else {
                small.muted id="project-key-hint" { "Leave blank to generate a key from the name. The key appears before every issue number." }
            }
            button type="submit" { @if project.is_some() { "Save changes" } @else { "Create project" } }
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
}

async fn directory(db: &PgPool, owner: Uuid) -> AppResult<Vec<DirectoryEntry>> {
    Ok(sqlx::query_as(
        "SELECT p.id, p.name, p.key, p.archived,
                coalesce(i.open_issues, 0) AS open_issues,
                coalesce(i.completed_issues, 0) AS completed_issues,
                m.id AS milestone_id, m.title AS milestone_title, m.target_on AS milestone_target
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
    let description =
        format!("{completed} of {total} issues completed. Excludes canceled and archived issues.");
    html! {
        div.project-progress title=(&description) {
            progress value=(completed) max=(total) aria-label=(&description) {}
            span { (completed) span.muted { " / " (total) } span.project-mobile-unit { " done" } }
        }
    }
}

fn project_list(projects: &[DirectoryEntry], archived: bool) -> Markup {
    html! {
        div.project-directory {
            div.project-columns aria-hidden="true" { span { "Project" } span { "Issues completed" } span { "Next milestone" } }
            ul.issue-list.project-rows {
                @for p in projects.iter().filter(|p| p.archived == archived) {
                    li.project-row {
                        a.project-name href=(format!("/projects/{}", p.id)) {
                            (ui::icon("projects")) span { strong { (&p.name) } code { (&p.key) } }
                        }
                        (issue_progress(p.open_issues, p.completed_issues))
                        @if let (Some(id), Some(title), Some(date)) = (p.milestone_id, &p.milestone_title, p.milestone_target) {
                            a.project-milestone href=(format!("/roadmap?project={}&milestone={id}#milestone-detail", p.id)) {
                                span.project-mobile-label { "Next milestone" } span { (title) }
                                time datetime=(date) { (date.format("%b %-d, %Y")) }
                            }
                        } @else {
                            span.project-unscheduled { "No scheduled milestone" }
                        }
                    }
                }
            }
        }
    }
}

async fn index(State(state): State<AppState>, auth: Auth) -> AppResult<Html<String>> {
    let projects = directory(&state.db, auth.user_id).await?;
    let plan: String = sqlx::query_scalar("SELECT plan FROM users WHERE id=$1")
        .bind(auth.user_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let limit = store::limits(&plan).projects;
    let active = projects.iter().filter(|p| !p.archived).count();
    Ok(ui::page(
        &auth,
        "Projects",
        "projects",
        html! {
            header.page-head.project-index-head {
                div { h1 { "Projects" } p.muted { (active) " of " (limit) " active project slots in use" } }
                @if (active as i64) < limit && !projects.is_empty() {
                    a.button href="#new-project" { (ui::icon("plus")) "New project" }
                }
            }
            @if active > 0 {
                section.project-group { h2 { "Active projects" span.project-count { (active) } } (project_list(&projects, false)) }
            }
            @if projects.len() > active {
                section.project-group { h2 { "Archived projects" span.project-count { (projects.len() - active) } } (project_list(&projects, true)) }
            }
            @if (active as i64) < limit {
                details.project-form-disclosure open[projects.is_empty()] {
                    summary { @if projects.is_empty() { "Create your first project" } @else { "Create a project" } }
                    div id="new-project" { (project_form(&auth, None)) }
                }
            } @else {
                p.notice { "Archive an active project to free a slot, or " a href="/billing" { "review your plan" } "." }
            }
        },
    ))
}

#[derive(FromRow)]
struct QueueIssue {
    id: Uuid,
    number: i64,
    title: String,
    status: String,
}

async fn project_queue(db: &PgPool, owner: Uuid, project: Uuid) -> AppResult<Vec<QueueIssue>> {
    Ok(sqlx::query_as(
        "SELECT id, number, title, status FROM issues
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
    Ok(ui::page(
        &auth,
        &project.name,
        "projects",
        html! {
            header.page-head.project-detail-head {
                div {
                    a.project-back href="/projects" { "Projects" }
                    h1 { (&project.name) }
                    div.project-id { code { (&project.key) } @if project.archived { " · Archived · Read-only" } }
                }
                div.actions {
                    @if !project.archived {
                        a.button href=(format!("/issues?project={id}#new-issue")) { (ui::icon("plus")) "New issue" }
                    }
                    a.button.secondary href=(format!("/transfer/export/{id}")) { "Export project" }
                }
            }
            div.project-overview {
                section.project-work aria-labelledby="project-issues-title" {
                    header.project-section-head {
                        h2 id="project-issues-title" { "Issues" }
                        div.project-view-links { a href=(&issues) { "Issue list" } a href=(format!("{issues}&view=board")) { "Board" } }
                    }
                    div.project-work-progress {
                        div { strong { (totals.completed_issues) " done" } span { " · " (totals.open_issues) " open" } }
                        (issue_progress(totals.open_issues, totals.completed_issues))
                    }
                    @if queue.is_empty() {
                        p.project-empty { "No open issues." }
                    } @else {
                        h3.project-queue-heading {
                            "Open work"
                            @if totals.open_issues > queue.len() as i64 {
                                span.project-queue-count { (queue.len()) " of " (totals.open_issues) }
                            }
                        }
                        ul.project-queue {
                            @for item in &queue {
                                li {
                                    a href=(format!("/issues/{}", item.id)) {
                                        code { (&project.key) "-" (item.number) } span { (&item.title) }
                                    }
                                    span.project-issue-state { @match item.status.as_str() {
                                        "in_progress" => { "In progress" }
                                        "ready" => { "Ready" }
                                        _ => { "Backlog" }
                                    } }
                                }
                            }
                        }
                    }
                }
                nav.project-resources aria-label="Project views" {
                    h2 { "Project views" }
                    @for (path, label, number, singular, plural) in [
                        ("notes","Notes",totals.notes,"note","notes"),
                        ("roadmap","Roadmap",totals.milestones,"milestone","milestones"),
                        ("releases","Releases",totals.releases,"release","releases")
                    ] {
                        a href=(format!("/{path}?project={id}")) {
                            (ui::icon(path)) span { (label) }
                            small { (number) " " @if number == 1 { (singular) } @else { (plural) } }
                        }
                    }
                    a href=(format!("/insights?project={id}")) { (ui::icon("insights")) span { "Insights" } (ui::icon("arrow")) }
                    a href=(format!("/health?project={id}")) { (ui::icon("health")) span { "Project health" } (ui::icon("arrow")) }
                }
            }
            section.project-settings {
                h2 { "Project settings" }
                @if project.archived {
                    p { "Reactivating this project uses an active slot. Resume paused repositories from " a href="/integrations" { "Integrations" } "." }
                    form method="post" action=(format!("/projects/{id}/unarchive")) {
                        (ui::csrf(&auth)) button type="submit" { "Reactivate project" }
                    }
                } @else {
                    details { summary { "Edit project" } (project_form(&auth, Some(&project))) }
                    details {
                        summary { "Archive project" }
                        p { "Archiving frees a project slot, pauses repository sync and keeps the project available to read and export." }
                        form method="post" action=(format!("/projects/{id}/archive")) {
                            (ui::csrf(&auth)) button.secondary type="submit" { "Archive project" }
                        }
                    }
                }
                details {
                    summary { "Delete project" }
                    p { "Permanently delete this project and its issues, notes, releases, files and history. This also disconnects its linked repositories." }
                    form.stack method="post" action=(format!("/projects/{id}/delete")) {
                        (ui::csrf(&auth))
                        label { "Type " code { (&project.key) } " to confirm"
                            input type="text" name="confirmation" maxlength="12" autocomplete="off" required;
                        }
                        button.danger type="submit" { "Permanently delete project" }
                    }
                }
            }
        },
    ))
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

        let rows = directory(&db, owner).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, project);
        assert_eq!((row.open_issues, row.completed_issues), (2, 1));
        assert_eq!(row.milestone_title.as_deref(), Some("Prototype"));
        let queue = project_queue(&db, owner, project).await.unwrap();
        assert_eq!(queue.iter().map(|i| i.number).collect::<Vec<_>>(), [2, 1]);
        assert!(
            project_queue(&db, outsider, project)
                .await
                .unwrap()
                .is_empty()
        );
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
