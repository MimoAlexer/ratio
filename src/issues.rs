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
    project_id: Uuid,
    project_archived: bool,
    open_finding: bool,
    revision: i64,
    priority: String,
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

    fn queue_row(&self) -> Markup {
        html! { li {
            a.focus-row href=(self.url()) {
                span.focus-key { (pages::status_mark(&self.status)) (self.project_key) "-" (self.number) }
                span.focus-row-title { (&self.title) }
                span.focus-priority { (pages::priority_mark(&self.priority)) (label(&self.priority)) }
            }
        } }
    }

    fn close_finding(&self, auth: &Auth) -> Markup {
        html! {
            @if self.open_finding && !self.project_archived {
                form method="post" action=(format!("/issues/{}/edit",self.id)) {
                    (ui::csrf(auth))
                    input type="hidden" name="command_id" value=(Uuid::new_v4());
                    input type="hidden" name="revision" value=(self.revision);
                    input type="hidden" name="finding_closed" value="true";
                    button.action-secondary type="submit" aria-label=(format!("Close finding {}-{}",self.project_key,self.number)) { "Close finding" }
                }
            }
        }
    }

    fn identity(&self) -> Markup {
        html! {
            span.mono { (self.project_key) "-" (self.number) }
            span { (self.project_name) }
        }
    }
}

const HOME_SELECT: &str = "
    SELECT i.id, i.project_id,p.archived AS project_archived,i.revision,i.priority,
           (i.security_critical AND NOT i.finding_closed) AS open_finding,p.name AS project_name, p.key AS project_key,
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

#[derive(Default, FromRow)]
struct HomeContext {
    completed: i64,
    tracked: i64,
    source_note: Option<(Uuid, String, String)>,
    milestone: Option<(Uuid, String, Option<chrono::NaiveDate>)>,
}

async fn home_context(
    db: &sqlx::PgPool,
    owner: Uuid,
    project: Option<Uuid>,
    issue: Option<Uuid>,
) -> AppResult<HomeContext> {
    let Some(project) = project else {
        return Ok(HomeContext::default());
    };
    Ok(sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status='done') AS completed, count(*) AS tracked,
                (SELECT ROW(n.id,n.title,src.source_anchor) FROM note_issue_sources src
                   JOIN notes n ON (n.owner_id,n.project_id,n.id)=(src.owner_id,src.project_id,src.note_id)
                  WHERE src.owner_id=$1 AND src.project_id=$2 AND src.issue_id=$3) AS source_note,
                (SELECT ROW(m.id,m.title,m.target_on) FROM milestone_issues mi JOIN milestones m
                     ON (m.owner_id,m.project_id,m.id)=(mi.owner_id,mi.project_id,mi.milestone_id)
                  WHERE mi.owner_id=$1 AND mi.project_id=$2 AND mi.issue_id=$3 AND m.status <> 'reached'
                  ORDER BY m.target_on NULLS LAST,m.id LIMIT 1) AS milestone
           FROM issues WHERE owner_id=$1 AND project_id=$2 AND NOT archived AND status <> 'canceled'",
    )
    .bind(owner)
    .bind(project)
    .bind(issue)
    .fetch_one(db)
    .await?)
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
        ORDER BY i.updated_at DESC, i.id LIMIT 13"
    );
    let critical_sql = format!(
        "{HOME_SELECT}
        AND i.security_critical AND NOT i.finding_closed ORDER BY i.updated_at DESC, i.id LIMIT 6"
    );
    let ready_sql = format!(
        "{HOME_SELECT}
         AND NOT p.archived AND i.status='ready'
         AND NOT EXISTS (
             SELECT 1 FROM issue_blockers b JOIN issues blocker
               ON (blocker.owner_id,blocker.project_id,blocker.id)=(b.owner_id,b.project_id,b.blocker_id)
              WHERE b.owner_id=i.owner_id AND b.issue_id=i.id AND NOT blocker.archived
                AND blocker.status NOT IN ('done','canceled')
         )
         ORDER BY CASE i.priority WHEN 'urgent' THEN 1 WHEN 'high' THEN 2 WHEN 'normal' THEN 3 ELSE 4 END,
                  i.updated_at DESC,i.id LIMIT 11"
    );
    let (resume, active, critical, critical_count, ready) = if projects.is_empty() {
        (None, Vec::new(), Vec::new(), 0, Vec::new())
    } else {
        tokio::try_join!(
            sqlx::query_as::<_, HomeIssue>(&resume_sql).bind(owner).bind(project).fetch_optional(&state.db),
            sqlx::query_as::<_, HomeIssue>(&active_sql).bind(owner).bind(project).fetch_all(&state.db),
            sqlx::query_as::<_, HomeIssue>(&critical_sql).bind(owner).bind(project).fetch_all(&state.db),
            sqlx::query_scalar::<_,i64>("SELECT count(*) FROM issues WHERE owner_id=$1 AND ($2::uuid IS NULL OR project_id=$2) AND NOT archived AND security_critical AND NOT finding_closed")
                .bind(owner).bind(project).fetch_one(&state.db),
            sqlx::query_as::<_,HomeIssue>(&ready_sql).bind(owner).bind(project).fetch_all(&state.db)
        )?
    };
    let focus = resume.as_ref().or_else(|| ready.first());
    let focus_project = focus.map(|i| i.project_id).or(project).or_else(|| {
        projects
            .first()
            .filter(|_| projects.len() == 1)
            .map(|p| p.id)
    });
    let context = projects.iter().find(|p| Some(p.id) == focus_project);
    let directory = issues_url(project, "");
    let create_url = format!("{directory}#new-issue");
    let focused_id = focus.map(|issue| issue.id);
    let other_finding_count =
        critical_count - i64::from(focus.is_some_and(|issue| issue.open_finding));
    let other_findings: Vec<_> = critical
        .iter()
        .filter(|issue| Some(issue.id) != focused_id)
        .collect();
    let queued = |issue: &&HomeIssue| {
        Some(issue.id) != focused_id && !other_findings.iter().any(|f| f.id == issue.id)
    };
    let active_others: Vec<_> = active.iter().filter(queued).take(6).collect();
    let ready_others: Vec<_> = ready.iter().filter(queued).take(4).collect();
    let (work, (coverage, remote_security)) = tokio::join!(
        home_context(&state.db, owner, context.map(|p| p.id), focused_id),
        async {
            if projects.is_empty() {
                (String::new(), html! {})
            } else {
                crate::github::project_home_parts(&state, &auth, project).await
            }
        }
    );
    let HomeContext {
        completed,
        tracked,
        source_note,
        milestone,
    } = work?;
    let progress = context.map(|_| (completed, tracked));
    let pull_request = if let Some(id) = focused_id {
        crate::github::resume_pr(&state, &auth, id).await
    } else {
        html! {}
    };
    let body = html! {
        div.focus-home {
            header.view-head {
                h1 { "Home" }
                @if !projects.is_empty() {
                    div.view-actions.focus-tools {
                        @if projects.len() > 1 {
                            form method="get" action="/" {
                                label { span.sr-only { "Project" } select name="project" {
                                    option value="" selected[project.is_none()] { "All projects" }
                                    @for p in &projects {
                                        option value=(p.id) selected[project == Some(p.id)] {
                                            (p.name) @if p.archived { " (archived)" }
                                        }
                                    }
                                } }
                                button.action-secondary type="submit" { "Apply" }
                            }
                        }
                        @if writable && focus.is_some() {
                            a.action-secondary href=(&create_url) { (ui::icon("plus")) "New issue" }
                        }
                    }
                }
            }
            @if projects.is_empty() {
                section.focus-first id="resume" aria-labelledby="resume-title" {
                    h2 id="resume-title" { "Start your first project" }
                    p { "Give your project a name. You’ll add its first issue next." }
                    form.form-stack method="post" action="/projects" {
                        (ui::csrf(&auth))
                        input type="hidden" name="next" value="first-issue";
                        label { "Project name" input name="name" maxlength="120" required pattern=r".*\S.*"; }
                        button.action-primary type="submit" { "Continue to first issue" (ui::icon("arrow")) }
                    }
                }
            } @else {
                div.focus-layout {
                    div.focus-queue {
                        section.focus-card id="resume" aria-labelledby="resume-title" {
                            @if let Some(item) = focus {
                                header.focus-card-head {
                                    h2 id="resume-title" {
                                        (ui::icon("resume"))
                                        @if item.checkpoint_at.is_some() { "Current checkpoint" }
                                        @else if resume.is_some() { "Current issue" }
                                        @else { "Next ready issue" }
                                    }
                                    @if let Some(t) = item.checkpoint_at {
                                        time datetime=(t.to_rfc3339()) title=(t.format("%d %b %Y, %H:%M UTC")) {
                                            "Saved " (t.format("%d %b"))
                                        }
                                    }
                                }
                                div.focus-record {
                                    div.focus-meta {
                                        span.mono { (item.project_key) "-" (item.number) }
                                        span { (item.project_name) }
                                        span { (pages::status_mark(&item.status)) (label(&item.status)) }
                                        @if item.open_finding { span.focus-risk { (ui::icon("flag")) "Open critical finding" } }
                                    }
                                    h3 { (&item.title) }
                                    @if !item.next_action.is_empty() {
                                        p.focus-action { (&item.next_action) }
                                    }
                                    div.focus-actions {
                                        a.action-primary href=(item.url()) {
                                            @if resume.is_some() { "Resume issue" } @else { "Open issue" } (ui::icon("arrow"))
                                        }
                                        @if let Some((id, title, anchor)) = &source_note {
                                            a.focus-source href=(format!("/notes/{id}#{anchor}")) title="Source note" {
                                                (ui::icon("notes")) span.sr-only { "Source note: " } (title)
                                            }
                                        }
                                        (pull_request)
                                        (item.close_finding(&auth))
                                    }
                                }
                            } @else {
                                div.focus-empty {
                                    h2 id="resume-title" {
                                        @if writable { "No work in progress" } @else { "Archived work" }
                                    }
                                    @if writable {
                                        p { "Create an issue or choose one from your backlog." }
                                        div.focus-actions {
                                            a.action-primary href=(&create_url) { (ui::icon("plus")) "New issue" }
                                            a href=(issues_url(project,"status=backlog")) { "View backlog" }
                                        }
                                    } @else {
                                        p { "Issues in archived projects are available to read." }
                                        a.action-secondary href=(issues_url(project,"scope=archived")) { "Browse archived issues" }
                                    }
                                }
                            }
                        }
                        @if other_finding_count > 0 {
                            section.focus-section id="critical-findings" aria-labelledby="findings-title" {
                                header.focus-section-head {
                                    h2 id="findings-title" { (ui::icon("flag")) "Critical findings" span.focus-count { (other_finding_count) } }
                                    @if other_finding_count > other_findings.len() as i64 {
                                        a href=(issues_url(project,"critical=open&scope=all")) { "View all" }
                                    }
                                }
                                ul.focus-findings {
                                    @for issue in other_findings {
                                        li {
                                            div {
                                                p.focus-meta { (issue.identity()) }
                                                a.focus-record-link href=(issue.url()) { (&issue.title) }
                                                @if !issue.next_action.is_empty() { p.focus-finding-action { (&issue.next_action) } }
                                            }
                                            (issue.close_finding(&auth))
                                        }
                                    }
                                }
                            }
                        }
                        @if !active_others.is_empty() {
                            section.focus-section id="active-work" aria-labelledby="active-title" {
                                header.focus-section-head {
                                    h2 id="active-title" { "Also in progress" }
                                    a href=(issues_url(project,"status=in_progress")) { "View all" span.sr-only { " in-progress issues" } (ui::icon("arrow")) }
                                }
                                ul.focus-list { @for item in active_others { (item.queue_row()) } }
                            }
                        }
                        @if !ready_others.is_empty() {
                            section.focus-section aria-labelledby="ready-title" {
                                header.focus-section-head {
                                    h2 id="ready-title" { "Ready next" }
                                    a href=(issues_url(project,"status=ready&sort=priority")) { "View all" span.sr-only { " ready issues" } (ui::icon("arrow")) }
                                }
                                ul.focus-list { @for item in ready_others { (item.queue_row()) } }
                            }
                        }
                    }
                    aside.focus-context aria-label="Work context" {
                        @if let Some(p) = context {
                            section {
                                div.focus-context-head {
                                    h2 { "Project" }
                                    @if projects.len() == 1 {
                                        @if project.is_some() {
                                            a href="/" { "All projects" }
                                        } @else {
                                            a href=(format!("/?project={}",p.id)) aria-label=(format!("Show Home for {}",p.name)) { "Project Home" }
                                        }
                                    }
                                }
                                a.focus-project-name href=(format!("/projects/{}",p.id)) { (ui::icon("projects")) (p.name) }
                                @if let Some((done, total)) = progress && total > 0 {
                                    p.focus-progress-label { (done) " of " (total) " issues completed" }
                                    progress.focus-progress value=(done) max=(total) aria-label=(format!("{done} of {total} issues completed")) {}
                                }
                                @if let Some((id, title, target)) = &milestone {
                                    div.focus-milestone {
                                        h3 { "Linked milestone" }
                                        a href=(format!("/roadmap?project={}&milestone={id}#milestone-detail", p.id)) { (title) (ui::icon("arrow")) }
                                        @if let Some(date) = target {
                                            p { "Target " time datetime=(date) { (date.format("%d %b %Y")) } }
                                        }
                                    }
                                }
                            }
                        }
                        details.focus-security {
                            summary { "Dependency alerts" span { (coverage) } }
                            (remote_security)
                        }
                    }
                }
            }
        }
    };
    Ok(ui::section_page(&auth, "Home", "home", body, project))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[sqlx::test]
    async fn home_context_preserves_optional_links_and_scopes_progress(
        db: sqlx::PgPool,
    ) -> AppResult<()> {
        use crate::sample::{DEMO_OWNER, DEMO_PROJECT, seed};
        seed(&db).await?;
        let issue = sqlx::query_scalar("SELECT id FROM issues WHERE project_id=$1 AND number=32")
            .bind(DEMO_PROJECT)
            .fetch_one(&db)
            .await?;
        let context = home_context(&db, DEMO_OWNER, Some(DEMO_PROJECT), Some(issue)).await?;
        let (_, title, anchor) = context.source_note.expect("source decision");
        assert_eq!(
            (title.as_str(), anchor.as_str()),
            ("Link resolution", "block-rename-check")
        );
        let (_, title, date) = context.milestone.expect("linked milestone");
        assert_eq!(title, "Public beta");
        assert!(date.is_some());

        let no_issue = home_context(&db, DEMO_OWNER, Some(DEMO_PROJECT), None).await?;
        assert!(no_issue.source_note.is_none() && no_issue.milestone.is_none());
        assert_eq!(
            (no_issue.completed, no_issue.tracked),
            (context.completed, context.tracked)
        );
        let unseen = home_context(&db, Uuid::new_v4(), Some(DEMO_PROJECT), Some(issue)).await?;
        assert!(unseen.source_note.is_none() && unseen.milestone.is_none());
        assert_eq!((unseen.completed, unseen.tracked), (0, 0));
        Ok(())
    }

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
