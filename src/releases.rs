mod commands;
mod detail;
mod forecast;
mod history;
mod insights;

use axum::{
    Router,
    extract::{Form, Query, State},
    response::{Html, Redirect},
    routing::get,
};
use chrono::NaiveDate;
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, store, ui};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/releases", get(index).post(create))
        .route("/releases/{id}", get(detail::show))
        .merge(commands::routes())
        .merge(forecast::routes())
        .merge(insights::routes())
}

#[derive(Deserialize, Default)]
struct IndexQuery {
    project: Option<Uuid>,
    #[serde(default)]
    portfolio: bool,
    #[serde(default)]
    new: bool,
    #[serde(default)]
    page: u32,
}

#[derive(FromRow)]
struct ReleaseRow {
    id: Uuid,
    project_name: String,
    name: String,
    version: String,
    target_on: Option<NaiveDate>,
    published_on: Option<NaiveDate>,
    entries: i64,
    members: i64,
    first_shipments: i64,
}

async fn plan_today(db: &PgPool, owner: Uuid) -> AppResult<(String, NaiveDate)> {
    sqlx::query_as("SELECT plan,(now() AT TIME ZONE timezone)::date FROM users WHERE id=$1")
        .bind(owner)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

pub(super) fn history_start(today: NaiveDate, plan: &str) -> Option<NaiveDate> {
    match plan {
        "max" => None,
        "plus" => today
            .checked_sub_months(chrono::Months::new(12))
            .and_then(|d| d.succ_opt()),
        _ => Some(today - chrono::Duration::days(29)),
    }
}

async fn index(
    auth: Auth,
    State(state): State<AppState>,
    Query(query): Query<IndexQuery>,
) -> AppResult<Html<String>> {
    let projects = store::projects(&state.db, auth.user_id).await?;
    let (plan, today) = plan_today(&state.db, auth.user_id).await?;
    if query.portfolio && plan != "max" {
        return Err(AppError::Forbidden);
    }
    let selected = match query.project {
        Some(id) => Some(
            projects
                .iter()
                .find(|p| p.id == id)
                .ok_or(AppError::NotFound)?,
        ),
        None if query.portfolio => None,
        None => projects.iter().find(|p| !p.archived).or(projects.first()),
    };
    let project = selected.map(|p| p.id);
    let floor = history_start(today, &plan);
    let page_size = 30_i64;
    let rows: Vec<ReleaseRow> = sqlx::query_as(
        "WITH shipments AS (
            SELECT e.release_id,e.issue_id,
                row_number() OVER (PARTITION BY e.issue_id ORDER BY r.published_on,r.published_at,r.id) AS ordinal
            FROM release_entries e JOIN releases r ON r.id=e.release_id
            WHERE e.owner_id=$1 AND ($2::uuid IS NULL OR e.project_id=$2) AND r.published_at IS NOT NULL
         ), shipped AS (
            SELECT release_id,count(*) AS entries,count(*) FILTER (WHERE ordinal=1) AS first_shipments
            FROM shipments GROUP BY release_id
         )
         SELECT r.id,p.name AS project_name,r.name,r.version,r.target_on,r.published_on,
            coalesce(s.entries,0) AS entries,coalesce(s.first_shipments,0) AS first_shipments,
            (SELECT count(*) FROM release_members m WHERE m.release_id=r.id) AS members
         FROM releases r JOIN projects p ON p.id=r.project_id LEFT JOIN shipped s ON s.release_id=r.id
         WHERE r.owner_id=$1 AND ($2::uuid IS NULL OR r.project_id=$2)
         AND (r.published_on IS NULL OR $3::date IS NULL OR r.published_on >= $3)
         ORDER BY (r.published_on IS NULL) DESC,r.published_on DESC,r.created_at DESC,r.id
         LIMIT $4 OFFSET $5"
    ).bind(auth.user_id).bind(project).bind(floor).bind(page_size+1).bind(i64::from(query.page)*page_size)
        .fetch_all(&state.db).await?;
    let totals: Option<(i64, i64, i64)> = if query.portfolio && project.is_none() {
        Some(sqlx::query_as(
            "SELECT count(DISTINCT e.issue_id),count(e.issue_id),count(DISTINCT r.id) FROM releases r
             LEFT JOIN release_entries e ON e.release_id=r.id WHERE r.owner_id=$1 AND r.published_at IS NOT NULL"
        ).bind(auth.user_id).fetch_one(&state.db).await?)
    } else {
        None
    };
    let base = project
        .map(|id| format!("/releases?project={id}"))
        .unwrap_or_else(|| "/releases?portfolio=true".to_owned());
    let writable: Vec<_> = projects.iter().filter(|p| !p.archived).collect();
    Ok(ui::section_page(
        &auth,
        "Releases",
        "releases",
        html! {
            header.view-head.release-index-head {
                div {
                    h1 { "Releases" }
                    @if projects.len() == 1 && plan != "max" {
                        p.muted { (&projects[0].name) }
                    }
                }
                @if !writable.is_empty() && !query.new {
                    div.view-actions { a.button.action-primary href=(format!("{base}&new=true#new-release")) {
                        (ui::icon("plus")) "New release"
                    } }
                }
            }
            @if projects.len() > 1 || (plan == "max" && !projects.is_empty()) {
                details.release-projects {
                    summary { "Project · " (selected.map_or("All projects", |p| p.name.as_str())) }
                    nav aria-label="Release projects" {
                        @for p in &projects {
                            a href=(format!("/releases?project={}",p.id)) aria-current=[(project == Some(p.id)).then_some("page")] { (p.name) @if p.archived { " (archived)" } }
                        }
                        @if plan == "max" { a href="/releases?portfolio=true" aria-current=[project.is_none().then_some("page")] { "All projects" } }
                    }
                }
            }
            @if let Some((unique,entries,releases)) = totals {
                section.content-section { h2 { "Portfolio shipments" } p { strong { (unique) } " distinct issues · " (entries) " manifest entries across " (releases) " published releases" } }
            }
            @if !writable.is_empty() && query.new {
                section.content-section.release-new id="new-release" {
                    h2 { "Create a release" }
                    form method="post" action="/releases" class="form-stack stack" {
                        (ui::csrf(&auth))
                        label { "Release name" input name="name" maxlength="150" placeholder="e.g. Public beta" required; }
                        @if writable.len() == 1 {
                            input type="hidden" name="project" value=(writable[0].id);
                            p.muted { "Project · " (writable[0].name) }
                        } @else {
                            label { "Project" select name="project" { @for p in &writable {
                                option value=(p.id) selected[Some(p.id)==project] { (p.name) }
                            } } }
                        }
                        details.release-additional { summary { "Version, target date and GitHub link" }
                            div.grid {
                                label { "Version (optional)" input name="version" maxlength="100" placeholder="v1.0.0"; }
                                label { "Target date (optional)" input type="date" name="target_on"; }
                            }
                            label { "GitHub Release URL (optional)" input type="url" name="source_url" maxlength="1024" placeholder="https://github.com/owner/repo/releases/tag/v1.0.0"; }
                        }
                        div.view-actions { button.action-primary { "Create draft" } a.action-quiet href=(&base) { "Cancel" } }
                    }
                }
            }
            @if rows.is_empty() && (!query.new || writable.is_empty()) {
                section.empty-state {
                    h2 { @if query.page > 0 { "No more releases" } @else if floor.is_some() && !projects.is_empty() { "No releases in this view" } @else { "No releases yet" } }
                    @if projects.is_empty() {
                        p { "Create a project to group issues into a release." }
                        a.button.action-primary href="/projects" { "Create a project" }
                    } @else if query.page == 0 {
                        p { @if let Some(since)=floor { "This view includes current drafts and releases published since " (since) "." } @else { "Create a draft to group issues and record what ships." } }
                    }
                }
            } @else if !rows.is_empty() {
                @let visible = &rows[..rows.len().min(page_size as usize)];
                @for (draft,heading) in [(true, "Drafts"), (false, "Published")] {
                    @if visible.iter().any(|r| r.published_on.is_none() == draft) {
                        section.release-group.content-section {
                            div.section-head { h2 { (heading) } @if !draft && let Some(since) = floor { span.release-period { "Since " (since.format("%d %b %Y")) } } }
                            ul.clean-list {
                                @for r in visible.iter().filter(|r| r.published_on.is_none() == draft) {
                                    li { a.release-record href=(format!("/releases/{}",r.id)) {
                                        span.release-record-icon aria-hidden="true" { (ui::icon(if draft { "edit" } else { "releases" })) }
                                        span.release-identity {
                                            span.release-name { (r.name) }
                                            @if !r.version.is_empty() { span.release-version { (r.version) } }
                                            @if project.is_none() { small { (r.project_name) } }
                                        }
                                        span.release-record-count {
                                            @if draft { (r.members) " in scope" }
                                            @else { (r.entries) " shipped · " (r.first_shipments) " first " (if r.first_shipments == 1 { "shipment" } else { "shipments" }) }
                                        }
                                        span.release-record-date {
                                            @if let Some(date) = r.published_on {
                                                time datetime=(date) { (date.format("%d %b %Y")) }
                                            } @else if let Some(date) = r.target_on {
                                                "Target " time datetime=(date) { (date.format("%d %b %Y")) }
                                            } @else { "No target date" }
                                        }
                                        span.release-forward aria-hidden="true" { (ui::icon("arrow")) }
                                    } }
                                }
                            }
                        }
                    }
                }
            }
            @if query.page > 0 || rows.len() > page_size as usize { nav class="view-tools" aria-label="Release pages" {
                @if query.page > 0 { a href=(format!("{base}&page={}", query.page-1)) { "Previous page" } }
                @if rows.len() > page_size as usize { a href=(format!("{base}&page={}",u64::from(query.page)+1)) { "Next page" } }
            } }
        },
        project,
    ))
}

#[derive(Deserialize)]
struct CreateForm {
    csrf: String,
    project: Uuid,
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    target_on: String,
    #[serde(default)]
    source_url: String,
}

fn valid_release_url(raw: &str) -> bool {
    if raw.is_empty() {
        return true;
    }
    if raw.len() > 1024 || raw.chars().any(char::is_control) {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    let path: Vec<_> = url.path_segments().into_iter().flatten().collect();
    url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && path.len() >= 3
        && !path[0].is_empty()
        && !path[1].is_empty()
        && path[2] == "releases"
}

async fn create(
    auth: Auth,
    State(state): State<AppState>,
    Form(input): Form<CreateForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&input.csrf)?;
    let name = input.name.trim();
    let version = input.version.trim();
    let url = input.source_url.trim();
    if name.is_empty()
        || name.contains('\0')
        || version.contains('\0')
        || name.chars().count() > 150
        || version.chars().count() > 100
    {
        return Err(AppError::BadRequest(
            "Use a release name of 1–150 characters and a version of at most 100.".into(),
        ));
    }
    if !valid_release_url(url) {
        return Err(AppError::BadRequest(
            "Use the HTTPS release page for a repository on github.com.".into(),
        ));
    }
    let target = (!input.target_on.is_empty())
        .then(|| input.target_on.parse::<NaiveDate>())
        .transpose()
        .map_err(|_| AppError::BadRequest("Use a valid target date.".into()))?;
    let mut tx = state.db.begin().await?;
    store::lock_project(&mut tx, auth.user_id, input.project).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO releases(owner_id,project_id,name,version,target_on,source_url) VALUES ($1,$2,$3,$4,$5,$6) RETURNING id"
    ).bind(auth.user_id).bind(input.project).bind(name).bind(version).bind(target).bind(url)
        .fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/releases/{id}")))
}

fn issue_link(id: Uuid, key: &str, number: i64, title: &str) -> Markup {
    html! { a href=(format!("/issues/{id}")) { (key) "-" (number) " · " (title) } }
}

fn status_label(status: &str) -> &str {
    match status {
        "in_progress" => "In progress",
        "backlog" => "Backlog",
        "ready" => "Ready",
        "canceled" => "Canceled",
        "done" => "Done",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::valid_release_url;
    #[test]
    fn release_links_must_identify_a_github_repository_release() {
        assert!(valid_release_url(""));
        assert!(valid_release_url(
            "https://github.com/example/app/releases/tag/v1.0"
        ));
        assert!(!valid_release_url(
            "https://github.com.example.com/app/r/releases"
        ));
        assert!(!valid_release_url(
            "https://github.com:999/example/app/releases"
        ));
        assert!(!valid_release_url("https://github.com/example/app/issues"));
        assert!(!valid_release_url("javascript:alert(1)"));
    }
}
