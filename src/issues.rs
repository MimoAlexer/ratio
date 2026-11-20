use axum::{
    Router,
    extract::{DefaultBodyLimit, Query, State},
    response::Html,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::FromRow;
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, store, ui};

#[cfg(test)]
mod acceptance;
mod pages;
mod write;

pub(crate) use write::{NewIssue, create_in};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(home))
        .route("/issues", get(pages::list).post(write::create))
        .route("/issues/{id}", get(pages::detail))
        .route("/issues/{id}/{action}", post(write::update))
        // Percent-encoding can triple the 2 MiB description on an ordinary HTML form.
        .layer(DefaultBodyLimit::max(6 * 1024 * 1024 + 64 * 1024))
}

const STATUSES: &[&str] = &["backlog", "ready", "in_progress", "done", "canceled"];
const KINDS: &[&str] = &["feature", "bug", "maintenance", "security"];
const PRIORITIES: &[&str] = &["low", "normal", "high", "urgent"];
const SEVERITIES: &[&str] = &["none", "low", "medium", "high", "critical"];

fn label(key: &str) -> &str {
    match key {
        "backlog" => "Backlog",
        "ready" => "Ready",
        "in_progress" => "In progress",
        "done" => "Done",
        "canceled" => "Canceled",
        "feature" => "Feature",
        "bug" => "Bug",
        "maintenance" => "Maintenance",
        "security" => "Security",
        "none" => "None",
        "low" => "Low",
        "medium" => "Medium",
        "normal" => "Normal",
        "high" => "High",
        "urgent" => "Urgent",
        "critical" => "Critical",
        _ => key,
    }
}

#[derive(FromRow)]
struct Issue {
    id: Uuid,
    project_id: Uuid,
    number: i64,
    title: String,
    body: String,
    status: String,
    priority: String,
    kind: String,
    labels: Vec<String>,
    manual_severity: String,
    finding_closed: bool,
    next_action: String,
    checkpoint_at: Option<DateTime<Utc>>,
    revision: i64,
    archived: bool,
}

#[derive(Deserialize, Default)]
struct HomeFilter {
    project: Option<String>,
}

#[derive(FromRow)]
struct HomeIssue {
    id: Uuid,
    project_name: String,
    project_key: String,
    number: i64,
    title: String,
    next_action: String,
    status: String,
    checkpoint_at: Option<DateTime<Utc>>,
}

impl HomeIssue {
    fn url(&self) -> String {
        format!("/issues/{}", self.id)
    }

    fn identity(&self) -> Markup {
        html! {
            span.mono { (self.project_key) "-" (self.number) }
            " · " (self.project_name)
        }
    }
}

const HOME_SELECT: &str = "
    SELECT i.id, p.name AS project_name, p.key AS project_key,
           i.number, i.title, i.status, i.next_action, i.checkpoint_at
      FROM issues i JOIN projects p ON (i.owner_id,i.project_id)=(p.owner_id,p.id)
     WHERE i.owner_id=$1 AND ($2::uuid IS NULL OR i.project_id=$2) AND NOT i.archived";

fn issues_url(project: Option<Uuid>, filter: &str) -> String {
    let mut destination = String::from("/issues");
    if let Some(id) = project {
        destination.push_str(&format!("?project={id}"));
    }
    if !filter.is_empty() {
        destination.push(if project.is_some() { '&' } else { '?' });
        destination.push_str(filter);
    }
    destination
}

async fn home(
    State(state): State<AppState>,
    auth: Auth,
    Query(filter): Query<HomeFilter>,
) -> AppResult<Html<String>> {
    let owner = auth.user_id;
    let project = filter
        .project
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<Uuid>()
                .map_err(|_| AppError::BadRequest("Choose a valid project.".into()))
        })
        .transpose()?;
    let projects = store::projects(&state.db, owner).await?;
    if let Some(id) = project
        && !projects.iter().any(|p| p.id == id)
    {
        return Err(AppError::NotFound);
    }
    let writable = projects
        .iter()
        .any(|p| !p.archived && project.is_none_or(|id| p.id == id));
    let resume_sql = format!(
        "{HOME_SELECT}
        AND NOT p.archived AND i.status IN ('backlog','ready','in_progress')
        AND (i.checkpoint_at IS NOT NULL OR i.status='in_progress')
        ORDER BY i.checkpoint_at DESC NULLS LAST, i.updated_at DESC, i.id LIMIT 1"
    );
    let active_sql = format!(
        "{HOME_SELECT}
        AND NOT p.archived AND i.status='in_progress'
        ORDER BY i.updated_at DESC, i.id LIMIT 7"
    );
    let critical_sql = format!(
        "{HOME_SELECT}
        AND i.security_critical AND NOT i.finding_closed ORDER BY i.updated_at DESC, i.id LIMIT 6"
    );
    let (resume, active, critical, critical_count) = tokio::try_join!(
        sqlx::query_as::<_, HomeIssue>(&resume_sql).bind(owner).bind(project).fetch_optional(&state.db),
        sqlx::query_as::<_, HomeIssue>(&active_sql).bind(owner).bind(project).fetch_all(&state.db),
        sqlx::query_as::<_, HomeIssue>(&critical_sql).bind(owner).bind(project).fetch_all(&state.db),
        sqlx::query_scalar::<_,i64>("SELECT count(*) FROM issues WHERE owner_id=$1 AND ($2::uuid IS NULL OR project_id=$2) AND NOT archived AND security_critical AND NOT finding_closed")
            .bind(owner).bind(project).fetch_one(&state.db)
    )?;
    let (source_note, pull_request) = if let Some(issue) = &resume {
        let (note, pr) = tokio::join!(
            sqlx::query_as::<_, (Uuid, String, String)>(
                "SELECT n.id, n.title, src.source_anchor FROM note_issue_sources src
             JOIN notes n ON (n.owner_id,n.id)=(src.owner_id,src.note_id)
             WHERE src.owner_id=$1 AND src.issue_id=$2",
            )
            .bind(owner)
            .bind(issue.id)
            .fetch_optional(&state.db),
            crate::github::resume_pr(&state, &auth, issue.id)
        );
        (note?, pr)
    } else {
        (None, html! {})
    };
    let (_, remote_security) = crate::github::project_home_parts(&state, &auth, project).await;
    let directory = issues_url(project, "");
    let create_url = if writable {
        format!("{directory}#new-issue")
    } else {
        "/projects".to_owned()
    };
    let active_others: Vec<_> = active
        .iter()
        .filter(|item| resume.as_ref().is_none_or(|top| item.id != top.id))
        .take(6)
        .collect();
    let body = html! {
        header.page-head.home-head {
            h1 { "Home" }
            div.actions {
                @if !projects.is_empty() {
                    form.row method="get" action="/" {
                        label { span.sr-only { "Project" } select name="project" {
                            option value="" selected[project.is_none()] { "All projects" }
                            @for p in &projects {
                                option value=(p.id) selected[project == Some(p.id)] {
                                    (p.name) @if p.archived { " (archived)" }
                                }
                            }
                        } }
                        button.secondary type="submit" { "Apply" }
                    }
                }
                a.btn.primary href=(&create_url) {
                    (ui::icon("plus")) @if writable { "New issue" } @else { "Create a project" }
                }
            }
        }
        div.home-columns {
            div.home-main {
                section.home-resume id="resume" aria-labelledby="resume-title" {
                    header.home-section-heading { h2 id="resume-title" { "Resume work" } }
                    @if let Some(item) = &resume {
                        div.resume-content {
                            p.issue-context { (item.identity()) span.work-state { (label(&item.status)) } }
                            h3 { a href=(item.url()) { (item.title) } }
                            @if !item.next_action.is_empty() {
                                div.checkpoint-copy {
                                    p.home-label { "Next action" }
                                    p { (item.next_action) }
                                }
                            }
                            @if let Some((id, title, anchor)) = &source_note {
                                p.source-note { "Source note · " a href=(format!("/notes/{id}#{anchor}")) { (title) } }
                            }
                            div.actions.resume-actions {
                                a.btn.primary href=(item.url()) { "Resume issue" (ui::icon("arrow")) }
                                (pull_request)
                            }
                            @if let Some(stamp) = item.checkpoint_at {
                                p.checkpoint-stamp { "Checkpoint saved "
                                    time datetime=(stamp.to_rfc3339()) { (stamp.format("%b %d, %Y at %H:%M UTC")) }
                                }
                            } @else { p.checkpoint-stamp { "Your most recently edited issue in progress." } }
                        }
                    } @else {
                        div.resume-content {
                            @if projects.is_empty() {
                                h3 { "Start your first issue" }
                                p { "Choose a project name to start tracking the issue and its next action." }
                                form.stack method="post" action="/projects" {
                                    (ui::csrf(&auth))
                                    input type="hidden" name="next" value="first-issue";
                                    label { "Project name" input name="name" maxlength="120" required; }
                                    div.actions { button.primary type="submit" { "Create my first issue" } }
                                }
                            } @else if writable {
                                h3 { "No work to resume" }
                                p { "Choose an issue to start, or save a next action on an open issue." }
                                div.actions {
                                    a.btn.primary href=(&create_url) { "Create an issue" }
                                    a href=(&directory) { "Browse issues" }
                                }
                            } @else {
                                p { "Work in archived projects is available in the issue list." }
                                a href=(&directory) { "Browse issues" }
                            }
                        }
                    }
                }
                section.home-active id="active-work" aria-labelledby="active-title" {
                    header.home-section-heading { h2 id="active-title" { "Other work in progress" } }
                    @if active_others.is_empty() {
                        p.muted { "No other issues in progress." }
                    } @else {
                        ul.clean-list {
                            @for issue in active_others {
                                li.home-issue-row {
                                    span.issue-context { (issue.identity()) }
                                    a href=(issue.url()) { (issue.title) }
                                }
                            }
                        }
                    }
                    a.text-action href=(issues_url(project,"status=in_progress")) { "View in-progress issues" (ui::icon("arrow")) }
                }
            }
            section.home-findings id="critical-findings" aria-labelledby="findings-title" {
                header.home-section-heading { h2 id="findings-title" { "Critical findings" } }
                div.manual-findings {
                    h3 { (ui::icon("flag")) "Manual reports" }
                    @if critical_count == 0 {
                        p.muted { "No open manual critical findings. Set an issue’s manual severity to Critical to track it here." }
                        a.text-action href=(if writable {&directory} else {&create_url}) { "Flag an issue" (ui::icon("arrow")) }
                    } @else {
                        p.finding-count {
                            (critical_count) @if critical_count == 1 { " open critical finding" } @else { " open critical findings" }
                        }
                        ul.clean-list {
                            @for issue in &critical {
                                li.home-issue-row {
                                    span.issue-context { (issue.identity()) }
                                    a href=(issue.url()) { (issue.title) }
                                    @if !issue.next_action.is_empty() { p { (issue.next_action) } }
                                }
                            }
                        }
                        @if critical_count > critical.len() as i64 {
                            a.text-action href=(issues_url(project,"critical=open&scope=all")) { "View all " (critical_count) " open findings" (ui::icon("arrow")) }
                        }
                    }
                }
                (remote_security)
            }
        }
    };
    Ok(ui::page(&auth, "Home", "home", body))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn home_links_preserve_scope_and_have_valid_query_separators() {
        let project = Uuid::nil();
        assert_eq!(
            issues_url(None, "status=in_progress"),
            "/issues?status=in_progress"
        );
        assert_eq!(issues_url(None, ""), "/issues");
        assert_eq!(
            issues_url(Some(project), "critical=open"),
            format!("/issues?project={project}&critical=open")
        );
    }
}
