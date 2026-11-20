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
}

fn checkpoint_text(action: &str) -> Markup {
    let text = action.trim();
    let first_line = text.lines().next().unwrap_or_default();
    let excerpt = match first_line.char_indices().nth(160) {
        Some((end, _)) => {
            let prefix = &first_line[..end];
            prefix
                .rsplit_once(char::is_whitespace)
                .filter(|(words, _)| !words.is_empty())
                .map_or(prefix, |(words, _)| words)
        }
        None => first_line,
    };
    html! {
        @if !text.is_empty() {
            @if excerpt == text {
                p.focus-action { (text) }
            } @else {
                details.focus-next-step {
                    summary {
                        span.focus-action { (excerpt) "…" }
                        span.step-toggle { "Full next action" }
                    }
                    p.focus-action { (text) }
                }
            }
        }
    }
}

const HOME_SELECT: &str = "
    SELECT i.id, i.project_id,p.archived AS project_archived,i.revision,
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
struct HomeTotals {
    backlog: i64,
    ready: i64,
    active: i64,
    done: i64,
    critical: i64,
}

impl HomeTotals {
    fn open(&self) -> i64 {
        self.active + self.ready + self.backlog
    }

    fn overview(&self, project: Option<Uuid>, milestone: Markup) -> Markup {
        let open = self.open();
        html! {
            aside.focus-overview aria-labelledby="overview-title" {
                h2 id="overview-title" { "Open issues" }
                strong.queue-total { (open) }
                @if open > 0 {
                    ul.queue-stages {
                        @for (status, count) in [("in_progress", self.active), ("ready", self.ready), ("backlog", self.backlog)] {
                            li {
                                a href=(issues_url(project, &format!("status={status}"))) {
                                    span.queue-label { (pages::status_mark(status)) (label(status)) }
                                    meter min="0" max=(open) value=(count) aria-hidden="true" {}
                                    span.queue-value { (count) }
                                }
                            }
                        }
                    }
                }
                @if self.done > 0 {
                    a.queue-done href=(issues_url(project, "status=done")) {
                        (self.done) " completed" (ui::icon("arrow"))
                    }
                }
                (milestone)
            }
        }
    }
}

async fn home_totals(
    db: &sqlx::PgPool,
    owner: Uuid,
    project: Option<Uuid>,
) -> Result<HomeTotals, sqlx::Error> {
    sqlx::query_as(
        "SELECT count(*) FILTER (WHERE NOT p.archived AND i.status='backlog') AS backlog,
                count(*) FILTER (WHERE NOT p.archived AND i.status='ready') AS ready,
                count(*) FILTER (WHERE NOT p.archived AND i.status='in_progress') AS active,
                count(*) FILTER (WHERE NOT p.archived AND i.status='done') AS done,
                count(*) FILTER (WHERE i.security_critical AND NOT i.finding_closed) AS critical
           FROM issues i JOIN projects p ON (i.owner_id,i.project_id)=(p.owner_id,p.id)
          WHERE i.owner_id=$1 AND ($2::uuid IS NULL OR i.project_id=$2) AND NOT i.archived",
    )
    .bind(owner)
    .bind(project)
    .fetch_one(db)
    .await
}

#[derive(Default, FromRow)]
struct HomeContext {
    source_note: Option<(Uuid, String, String)>,
    milestone: Option<(Uuid, String, Option<chrono::NaiveDate>)>,
}

async fn home_context(
    db: &sqlx::PgPool,
    owner: Uuid,
    project: Uuid,
    issue: Uuid,
) -> AppResult<HomeContext> {
    Ok(sqlx::query_as(
        "SELECT (SELECT ROW(n.id,n.title,src.source_anchor) FROM note_issue_sources src
                   JOIN notes n ON (n.owner_id,n.project_id,n.id)=(src.owner_id,src.project_id,src.note_id)
                  WHERE src.owner_id=$1 AND src.project_id=$2 AND src.issue_id=$3) AS source_note,
                (SELECT ROW(m.id,m.title,m.target_on) FROM milestone_issues mi JOIN milestones m
                     ON (m.owner_id,m.project_id,m.id)=(mi.owner_id,mi.project_id,mi.milestone_id)
                  WHERE mi.owner_id=$1 AND mi.project_id=$2 AND mi.issue_id=$3 AND m.status <> 'reached'
                  ORDER BY m.target_on NULLS LAST,m.id LIMIT 1) AS milestone",
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
    let critical_sql = format!(
        "{HOME_SELECT}
        AND i.security_critical AND NOT i.finding_closed ORDER BY i.updated_at DESC, i.id LIMIT 3"
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
                  i.updated_at DESC,i.id LIMIT 1"
    );
    let (resume, ready, critical, totals) = if projects.is_empty() {
        (None, None, Vec::new(), HomeTotals::default())
    } else {
        tokio::try_join!(
            sqlx::query_as::<_, HomeIssue>(&resume_sql)
                .bind(owner)
                .bind(project)
                .fetch_optional(&state.db),
            sqlx::query_as::<_, HomeIssue>(&ready_sql)
                .bind(owner)
                .bind(project)
                .fetch_optional(&state.db),
            sqlx::query_as::<_, HomeIssue>(&critical_sql)
                .bind(owner)
                .bind(project)
                .fetch_all(&state.db),
            home_totals(&state.db, owner, project)
        )?
    };
    let focus = resume.as_ref().or(ready.as_ref());
    let create_url = format!("{}#new-issue", issues_url(project, ""));
    let focus_is_critical = focus.is_some_and(|issue| issue.open_finding);
    let other_finding_count = totals.critical - i64::from(focus_is_critical);
    let other_findings: Vec<_> = critical
        .iter()
        .filter(|issue| focus.is_none_or(|item| item.id != issue.id))
        .take(2)
        .collect();
    let (work, (coverage, remote_security)) = tokio::join!(
        async {
            match focus {
                Some(item) => home_context(&state.db, owner, item.project_id, item.id).await,
                None => Ok(HomeContext::default()),
            }
        },
        async {
            if projects.is_empty() {
                (String::new(), html! {})
            } else {
                crate::github::project_home_parts(&state, &auth, project).await
            }
        }
    );
    let HomeContext {
        source_note,
        milestone,
    } = work?;
    let pull_request = if let Some(item) = focus {
        crate::github::resume_pr(&state, &auth, item.id).await
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
                    form.form-stack method="post" action="/projects" {
                        (ui::csrf(&auth))
                        input type="hidden" name="next" value="first-issue";
                        label { "Project name" input name="name" maxlength="120" required pattern=r".*\S.*"; }
                        button.action-primary type="submit" { "Continue to first issue" (ui::icon("arrow")) }
                    }
                }
            } @else {
                div.focus-layout {
                    section.focus-record id="resume" aria-labelledby="resume-title" {
                        @if let Some(item) = focus {
                            p.focus-eyebrow {
                                (ui::icon("resume"))
                                @if item.checkpoint_at.is_some() { "Last checkpoint" }
                                @else if resume.is_some() { "In progress" }
                                @else { "Next ready issue" }
                                span.mono { (item.project_key) "-" (item.number) }
                            }
                            h2 id="resume-title" { (&item.title) }
                            (checkpoint_text(&item.next_action))
                            @if focus_is_critical {
                                p.focus-risk { (ui::icon("flag")) "Open critical finding" }
                            }
                            div.focus-actions {
                                a.action-primary href=(item.url()) {
                                    @if resume.is_some() { "Resume issue" } @else { "Open issue" } (ui::icon("arrow"))
                                }
                            }
                            details.focus-links {
                                summary { "Issue context" }
                                div {
                                    span { (pages::status_mark(&item.status)) (label(&item.status)) }
                                    a href=(format!("/projects/{}",item.project_id)) { (ui::icon("projects")) (item.project_name) }
                                    @if let Some(t) = item.checkpoint_at {
                                        time datetime=(t.to_rfc3339()) { "Saved " (t.format("%d %b %Y")) }
                                    }
                                    @if let Some((id, title, anchor)) = &source_note {
                                        a href=(format!("/notes/{id}#{anchor}")) { (ui::icon("notes")) span.sr-only { "Source note " } (title) }
                                    }
                                    (pull_request)
                                    (item.close_finding(&auth))
                                }
                            }
                        } @else {
                            h2 id="resume-title" {
                                @if writable { "No work in progress" } @else { "Archived work" }
                            }
                            div.focus-actions {
                                @if writable {
                                    a.action-primary href=(&create_url) { (ui::icon("plus")) "New issue" }
                                    @if totals.backlog > 0 {
                                        a href=(issues_url(project,"status=backlog")) { "View backlog" }
                                    }
                                } @else {
                                    a.action-secondary href=(issues_url(project,"scope=archived")) { "Browse archived issues" }
                                }
                            }
                        }
                    }
                    section.focus-security id="critical-findings" aria-labelledby="findings-title" {
                        h2 id="findings-title" {
                            (ui::icon("flag"))
                            @if focus_is_critical && other_finding_count > 0 { "Other critical issues" } @else { "Critical issues" }
                            span.focus-count { @if focus_is_critical && other_finding_count > 0 { (other_finding_count) } @else { (totals.critical) } }
                        }
                        @if !other_findings.is_empty() {
                            ul.focus-findings {
                                @for issue in &other_findings {
                                    li {
                                        a.finding-link href=(issue.url()) { span.mono { (issue.project_key) "-" (issue.number) } span { (&issue.title) } }
                                        @if !issue.next_action.is_empty() || !issue.project_archived {
                                            details.finding-options {
                                                summary aria-label=(format!("Details for {}-{}",issue.project_key,issue.number)) { "Details" }
                                                div {
                                                    @if !issue.next_action.is_empty() { p { (&issue.next_action) } }
                                                    (issue.close_finding(&auth))
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            @if other_finding_count > other_findings.len() as i64 {
                                a.focus-all href=(issues_url(project,"critical=open&scope=all")) { "View all critical issues" (ui::icon("arrow")) }
                            }
                        } @else if focus_is_critical {
                            a.focus-all href="#resume" { "Current issue" (ui::icon("arrow")) }
                        }
                        details.focus-coverage {
                            summary { "Dependency alerts" span { (coverage) } }
                            (remote_security)
                        }
                    }
                    @if writable && (totals.open() > 0 || totals.done > 0) {
                        (totals.overview(project, html! {
                            @if let Some((id, title, target)) = &milestone && let Some(item) = focus {
                                a.focus-milestone href=(format!("/roadmap?project={}&milestone={id}#milestone-detail", item.project_id)) {
                                    span { (ui::icon("roadmap")) "Linked milestone" }
                                    strong { (title) }
                                    @if let Some(date) = target {
                                        time datetime=(date) { (date.format("%d %b")) }
                                    }
                                }
                            }
                        }))
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
    async fn home_context_preserves_links_and_scopes_work_counts(
        db: sqlx::PgPool,
    ) -> AppResult<()> {
        use crate::sample::{DEMO_OWNER, DEMO_PROJECT, seed};
        seed(&db).await?;
        let issue = sqlx::query_scalar("SELECT id FROM issues WHERE project_id=$1 AND number=32")
            .bind(DEMO_PROJECT)
            .fetch_one(&db)
            .await?;
        let context = home_context(&db, DEMO_OWNER, DEMO_PROJECT, issue).await?;
        let (_, title, anchor) = context.source_note.expect("source decision");
        assert_eq!(
            (title.as_str(), anchor.as_str()),
            ("Link resolution", "block-rename-check")
        );
        let (_, title, date) = context.milestone.expect("linked milestone");
        assert_eq!(title, "Public beta");
        assert!(date.is_some());

        let unseen = home_context(&db, Uuid::new_v4(), DEMO_PROJECT, issue).await?;
        assert!(unseen.source_note.is_none() && unseen.milestone.is_none());
        let totals = home_totals(&db, DEMO_OWNER, Some(DEMO_PROJECT)).await?;
        assert_eq!(
            (
                totals.active,
                totals.ready,
                totals.backlog,
                totals.done,
                totals.critical
            ),
            (2, 3, 2, 12, 1)
        );
        let outsider = home_totals(&db, Uuid::new_v4(), Some(DEMO_PROJECT)).await?;
        assert_eq!(
            (outsider.open(), outsider.done, outsider.critical),
            (0, 0, 0)
        );
        sqlx::query("UPDATE projects SET archived=true WHERE owner_id=$1 AND id=$2")
            .bind(DEMO_OWNER)
            .bind(DEMO_PROJECT)
            .execute(&db)
            .await?;
        let archived = home_totals(&db, DEMO_OWNER, None).await?;
        assert_eq!(
            (archived.open(), archived.done, archived.critical),
            (0, 0, 1)
        );
        Ok(())
    }

    #[test]
    fn long_and_multiline_checkpoints_keep_the_full_text_available() {
        assert_eq!(checkpoint_text(" ").into_string(), "");
        assert!(
            !checkpoint_text("Run the release checks")
                .into_string()
                .contains("<details")
        );
        for action in [
            "Read the change\nVerify the release".to_owned(),
            "β".repeat(180),
        ] {
            let rendered = checkpoint_text(&action).into_string();
            assert!(rendered.contains("<details") && rendered.contains(&action));
            assert!(rendered.contains("Full next action"));
        }
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
