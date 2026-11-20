use axum::extract::{Query, State};
use axum::response::Html;
use chrono::NaiveDate;
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use std::collections::HashSet;
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, github, store, ui};

#[derive(Deserialize)]
pub(super) struct HealthQuery {
    project: Option<Uuid>,
    view: Option<String>,
}

#[derive(FromRow)]
struct Release {
    id: Uuid,
    project_id: Uuid,
    name: String,
    target_on: Option<NaiveDate>,
    published_on: Option<NaiveDate>,
}

#[derive(FromRow)]
struct MilestoneState {
    id: Uuid,
    project_id: Uuid,
    release_id: Option<Uuid>,
    title: String,
    target_on: Option<NaiveDate>,
    status: String,
}

#[derive(FromRow)]
struct Issue {
    id: Uuid,
    number: i64,
    title: String,
    status: String,
}

#[derive(FromRow)]
struct Member {
    milestone_id: Uuid,
    #[sqlx(flatten)]
    issue: Issue,
}

#[derive(FromRow)]
struct Blocker {
    project_id: Uuid,
    issue_id: Uuid,
    waiting_number: i64,
    waiting_title: String,
    #[sqlx(flatten)]
    blocker: Issue,
}

#[derive(FromRow)]
struct Predecessor {
    milestone_id: Uuid,
    prerequisite_id: Uuid,
    title: String,
}

#[derive(FromRow)]
struct ManualCritical {
    project_id: Uuid,
    next_action: String,
    #[sqlx(flatten)]
    issue: Issue,
}

#[derive(FromRow)]
struct Checkpoint {
    project_id: Uuid,
    checkpoint_on: NaiveDate,
    #[sqlx(flatten)]
    issue: Issue,
}

struct HealthData {
    releases: Vec<Release>,
    milestones: Vec<MilestoneState>,
    members: Vec<Member>,
    blockers: Vec<Blocker>,
    predecessors: Vec<Predecessor>,
    flags: Vec<ManualCritical>,
    checkpoints: Vec<Checkpoint>,
    blocked_milestones: HashSet<Uuid>,
}

impl HealthData {
    async fn load(db: &PgPool, owner: Uuid, ids: &[Uuid]) -> AppResult<Self> {
        let mut tx = db.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let releases = sqlx::query_as(
            "SELECT id, project_id, name, target_on, published_on FROM releases
             WHERE owner_id=$1 AND project_id=ANY($2)
             ORDER BY published_on NULLS FIRST, target_on NULLS LAST, name, id",
        )
        .bind(owner)
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        let milestones = sqlx::query_as(
            "SELECT m.id, m.project_id, mr.release_id, m.title, m.target_on, m.status
               FROM milestones m LEFT JOIN milestone_releases mr
                 ON (mr.owner_id,mr.project_id,mr.milestone_id)=(m.owner_id,m.project_id,m.id)
              WHERE m.owner_id=$1 AND m.project_id=ANY($2)
              ORDER BY m.target_on NULLS LAST, m.title, m.id",
        )
        .bind(owner)
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        let members: Vec<Member> = sqlx::query_as(
            "SELECT mi.milestone_id, i.id, i.number, i.title, i.status
               FROM milestone_issues mi JOIN issues i
                 ON (i.owner_id,i.project_id,i.id)=(mi.owner_id,mi.project_id,mi.issue_id)
              WHERE mi.owner_id=$1 AND mi.project_id=ANY($2) AND NOT i.archived
              ORDER BY i.number, i.id",
        )
        .bind(owner)
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        let blockers: Vec<Blocker> = sqlx::query_as(
            "SELECT b.project_id, i.id AS issue_id, i.number AS waiting_number,
                    i.title AS waiting_title, p.id, p.number, p.title, p.status
               FROM issue_blockers b JOIN issues i
                 ON (i.owner_id,i.project_id,i.id)=(b.owner_id,b.project_id,b.issue_id)
               JOIN issues p
                 ON (p.owner_id,p.project_id,p.id)=(b.owner_id,b.project_id,b.blocker_id)
              WHERE b.owner_id=$1 AND b.project_id=ANY($2)
                AND NOT i.archived AND i.status IN ('backlog','ready','in_progress')
                AND NOT p.archived AND p.status IN ('backlog','ready','in_progress')
              ORDER BY i.number, p.number",
        )
        .bind(owner)
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        let predecessors: Vec<Predecessor> = sqlx::query_as(
            "SELECT d.dependent_id AS milestone_id, d.prerequisite_id, p.title
               FROM milestone_dependencies d JOIN milestones p
                 ON (p.owner_id,p.project_id,p.id)=(d.owner_id,d.project_id,d.prerequisite_id)
              WHERE d.owner_id=$1 AND d.project_id=ANY($2) AND p.status<>'reached'
              ORDER BY p.target_on NULLS LAST, p.title, p.id",
        )
        .bind(owner)
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        let flags = sqlx::query_as(
            "SELECT project_id, id, number, title, status, next_action
               FROM issues WHERE owner_id=$1 AND project_id=ANY($2)
                AND security_critical AND NOT archived ORDER BY number",
        )
        .bind(owner)
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        let checkpoints = sqlx::query_as(
            "SELECT DISTINCT ON (project_id) project_id, id, number, title, status,
                    (checkpoint_at AT TIME ZONE (SELECT timezone FROM users WHERE id=$1))::date AS checkpoint_on
               FROM issues WHERE owner_id=$1 AND project_id=ANY($2)
                AND NOT archived AND status IN ('backlog','ready','in_progress')
                AND checkpoint_at IS NOT NULL
              ORDER BY project_id, checkpoint_at DESC, id",
        )
        .bind(owner)
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let blocked_issues: HashSet<_> = blockers.iter().map(|b| b.issue_id).collect();
        let blocked_milestones = predecessors
            .iter()
            .map(|p| p.milestone_id)
            .chain(
                members
                    .iter()
                    .filter(|m| blocked_issues.contains(&m.issue.id))
                    .map(|m| m.milestone_id),
            )
            .collect();
        Ok(Self {
            blocked_milestones,
            releases,
            milestones,
            members,
            blockers,
            predecessors,
            flags,
            checkpoints,
        })
    }

    fn blocked(&self, milestone: Uuid) -> bool {
        self.blocked_milestones.contains(&milestone)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Timing {
    Reached,
    Overdue(i64),
    BlockedSoon(i64),
    Blocked,
    Unscheduled,
    Scheduled,
}

impl Timing {
    fn of(milestone: &MilestoneState, blocked: bool, today: NaiveDate) -> Self {
        if milestone.status == "reached" {
            return Self::Reached;
        }
        let days = milestone.target_on.map(|date| (date - today).num_days());
        match days {
            Some(n) if n < 0 => Self::Overdue(-n),
            Some(n) if blocked && n <= 7 => Self::BlockedSoon(n),
            _ if blocked => Self::Blocked,
            None => Self::Unscheduled,
            Some(_) => Self::Scheduled,
        }
    }

    fn attention(self) -> bool {
        matches!(self, Self::Overdue(_) | Self::BlockedSoon(_))
    }

    fn label(self) -> String {
        match self {
            Self::Reached => "Reached".into(),
            Self::Overdue(1) => "1 day overdue".into(),
            Self::Overdue(days) => format!("{days} days overdue"),
            Self::BlockedSoon(0) => "Blocked · due today".into(),
            Self::BlockedSoon(1) => "Blocked · due tomorrow".into(),
            Self::BlockedSoon(days) => format!("Blocked · due in {days} days"),
            Self::Blocked => "Blocked".into(),
            Self::Unscheduled => "Unscheduled".into(),
            Self::Scheduled => "Scheduled".into(),
        }
    }
}

fn milestone_url(project: Uuid, milestone: Uuid) -> String {
    format!("/roadmap?project={project}&milestone={milestone}#milestone-detail")
}

fn issue_link(key: &str, issue: &Issue) -> Markup {
    html! {
        a href=(format!("/issues/{}", issue.id)) {
            code { (key) "-" (issue.number) } " " (&issue.title)
        }
    }
}

fn status_name(value: &str) -> &str {
    match value {
        "in_progress" => "In progress",
        "ready" => "Ready",
        "backlog" => "Backlog",
        "done" => "Done",
        "canceled" => "Canceled",
        "planned" => "Planned",
        "reached" => "Reached",
        v => v,
    }
}

fn day(date: NaiveDate) -> Markup {
    html! { time datetime=(date) { (date.format("%e %b %Y").to_string().trim()) } }
}

fn milestone_row(
    project: &store::Project,
    m: &MilestoneState,
    data: &HealthData,
    today: NaiveDate,
) -> Markup {
    let timing = Timing::of(m, data.blocked(m.id), today);
    let members: Vec<_> = data
        .members
        .iter()
        .filter(|i| i.milestone_id == m.id)
        .collect();
    let count_done = members.iter().filter(|i| i.issue.status == "done").count();
    let count_in_scope = members
        .iter()
        .filter(|i| i.issue.status != "canceled")
        .count();
    html! {
        details open[timing.attention()] {
            summary {
                strong { (&m.title) } " "
                span class=(if timing.attention() { "status danger" } else { "status" }) { (timing.label()) }
                @if let Some(date) = m.target_on {
                    " · Target " (day(date))
                } @else if timing != Timing::Unscheduled {
                    " · No target"
                }
                @if count_in_scope > 0 {
                    " · " (count_done) "/" (count_in_scope) " issues done"
                }
            }
            div.stack {
                p { a href=(milestone_url(project.id, m.id)) { "Open milestone" } " · " (status_name(&m.status)) }
                @if timing != Timing::Reached {
                    @for p in data.predecessors.iter().filter(|p| p.milestone_id == m.id) {
                        p { "Waiting for milestone " a href=(milestone_url(project.id, p.prerequisite_id)) { (&p.title) } }
                    }
                }
                @if !members.is_empty() {
                    ul.issue-list {
                        @for member in members {
                            li { (issue_link(&project.key, &member.issue)) " · " (status_name(&member.issue.status))
                                @for blocker in data.blockers.iter().filter(|b| b.issue_id == member.issue.id) {
                                    div.muted { "Blocked by " (issue_link(&project.key, &blocker.blocker)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn release_row(
    project: &store::Project,
    release: &Release,
    data: &HealthData,
    today: NaiveDate,
) -> Markup {
    let children: Vec<_> = data
        .milestones
        .iter()
        .filter(|m| m.release_id == Some(release.id))
        .collect();
    let reached = children.iter().filter(|m| m.status == "reached").count();
    let attention = children
        .iter()
        .any(|m| Timing::of(m, data.blocked(m.id), today).attention());
    html! {
        details open[attention] {
            summary {
                strong { (&release.name) } " · "
                @if let Some(date) = release.published_on {
                    "Published " (day(date))
                } @else if let Some(date) = release.target_on {
                    "Target " (day(date))
                    @if date < today { " · Overdue" }
                } @else { "Draft release" }
                @if !children.is_empty() {
                    " · " (reached) "/" (children.len()) " milestones reached"
                }
            }
            div.stack {
                a href=(format!("/releases/{}", release.id)) { "Open release review" }
                @for m in children { (milestone_row(project, m, data, today)) }
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Coverage {
    checked: i64,
    unavailable: i64,
    critical: i64,
}

impl From<(i64, i64, i64)> for Coverage {
    fn from((checked, unavailable, critical): (i64, i64, i64)) -> Self {
        Self {
            checked,
            unavailable,
            critical,
        }
    }
}

fn github_coverage(project: Uuid, coverage: Coverage) -> Markup {
    html! {
        p {
            strong { "Dependabot" } " · "
            @if coverage.checked == 0 && coverage.unavailable == 0 {
                "No checked repositories."
            } @else {
                @if coverage.checked > 0 {
                    (coverage.critical) " open critical in " (coverage.checked)
                    @if coverage.checked == 1 { " checked repository" } @else { " checked repositories" }
                }
                @if coverage.unavailable > 0 {
                    @if coverage.checked > 0 { " · " }
                    (coverage.unavailable) " unavailable · Coverage incomplete"
                }
                "."
            }
            " " a href=(if coverage.critical > 0 { format!("/?project={project}#github-security") } else { "/integrations".into() }) {
                @if coverage.critical > 0 { "Review GitHub alerts" } @else { "Repository coverage" }
            }
        }
    }
}

fn project_health(
    project: &store::Project,
    data: &HealthData,
    coverage: Coverage,
    today: NaiveDate,
    portfolio: bool,
) -> (Markup, bool) {
    let milestones: Vec<_> = data
        .milestones
        .iter()
        .filter(|m| m.project_id == project.id)
        .collect();
    let releases: Vec<_> = data
        .releases
        .iter()
        .filter(|r| r.project_id == project.id)
        .collect();
    let flags: Vec<_> = data
        .flags
        .iter()
        .filter(|f| f.project_id == project.id)
        .collect();
    let blockers: Vec<_> = data
        .blockers
        .iter()
        .filter(|b| b.project_id == project.id)
        .collect();
    let checkpoint = data.checkpoints.iter().find(|c| c.project_id == project.id);
    let first_at_risk = milestones.iter().find_map(|m| {
        let timing = Timing::of(m, data.blocked(m.id), today);
        timing.attention().then_some((*m, timing))
    });
    let overdue_release = releases
        .iter()
        .find(|r| r.published_on.is_none() && r.target_on.is_some_and(|date| date < today));
    let has_target = milestones
        .iter()
        .any(|m| m.status != "reached" && m.target_on.is_some())
        || releases
            .iter()
            .any(|r| r.published_on.is_none() && r.target_on.is_some());
    let health_label = if project.archived {
        "Archived"
    } else if !flags.is_empty()
        || coverage.critical > 0
        || first_at_risk.is_some()
        || overdue_release.is_some()
    {
        "Needs attention"
    } else if has_target {
        "On track"
    } else {
        "Unscheduled"
    };
    let content = html! {
        section.panel aria-label=(format!("{} health", project.name)) {
            div.row {
                h2 { a href=(format!("/health?project={}", project.id)) { (&project.name) } }
                span class=(if health_label == "Needs attention" { "status danger" } else { "status" }) { (health_label) }
            }
            @if let Some(flag) = flags.first() {
                p { "Open manual critical flag on " (issue_link(&project.key, &flag.issue)) "." }
            } @else if coverage.critical > 0 {
                p { a href=(format!("/?project={}#github-security", project.id)) {
                    (coverage.critical) " open critical Dependabot " @if coverage.critical == 1 { "alert" } @else { "alerts" }
                } "." }
            } @else if let Some((m, timing)) = first_at_risk {
                p { a href=(milestone_url(project.id, m.id)) { (&m.title) } " · " (timing.label()) }
            } @else if let Some(r) = overdue_release {
                p { a href=(format!("/releases/{}", r.id)) { (&r.name) } " · Draft release past its target" }
            } @else if !has_target && !project.archived {
                p.muted { "No upcoming milestone or release target." }
            }
            @if let Some(cp) = checkpoint {
                @let age = (today - cp.checkpoint_on).num_days();
                p.muted {
                    "Latest checkpoint " (day(cp.checkpoint_on))
                    @if age == 0 { " (today)" } @else if age == 1 { " (yesterday)" }
                    @else if age > 1 { " (" (age) " days ago)" }
                    " · " (issue_link(&project.key, &cp.issue))
                }
            }
            (github_coverage(project.id, coverage))
            @if !portfolio {
                h3 { "Releases and milestones" }
                @if releases.is_empty() && milestones.is_empty() {
                    p.empty { "There are no releases or milestones yet. " a href=(format!("/roadmap?project={}", project.id)) { "Open the roadmap" } }
                }
                @for r in releases { (release_row(project, r, data, today)) }
                @for m in milestones.iter().filter(|m| m.release_id.is_none()) { (milestone_row(project, m, data, today)) }
                h3 { "Open issue blockers" }
                @if blockers.is_empty() {
                    p.muted { "No open blocking relationships." }
                } @else {
                    ul.issue-list {
                        @for b in blockers {
                            li {
                                a href=(format!("/issues/{}", b.issue_id)) { code { (&project.key) "-" (b.waiting_number) } " " (&b.waiting_title) }
                                " is blocked by " (issue_link(&project.key, &b.blocker)) " · " (status_name(&b.blocker.status))
                            }
                        }
                    }
                }
                h3 { "Manual critical flags" }
                @if flags.is_empty() { p.muted { "No open manual critical flags." } }
                @else { ul.issue-list {
                    @for f in flags { li {
                        (issue_link(&project.key, &f.issue)) " · " (status_name(&f.issue.status))
                        @if !f.next_action.is_empty() { div { "Next action · " (&f.next_action) } }
                    } }
                } }
            } @else { a href=(format!("/health?project={}", project.id)) { "View project health" } }
        }
    };
    (content, health_label == "Needs attention")
}

pub(super) async fn health(
    auth: Auth,
    State(state): State<AppState>,
    Query(query): Query<HealthQuery>,
) -> AppResult<Html<String>> {
    let projects = store::projects(&state.db, auth.user_id).await?;
    let plan: String = sqlx::query_scalar("SELECT plan FROM users WHERE id=$1")
        .bind(auth.user_id)
        .fetch_one(&state.db)
        .await?;
    let portfolio = query.view.as_deref() == Some("portfolio");
    if portfolio && plan != "max" {
        return Err(AppError::Forbidden);
    }
    let selected = match query.project {
        Some(id) => Some(
            projects
                .iter()
                .find(|p| p.id == id)
                .ok_or(AppError::NotFound)?,
        ),
        None => projects
            .iter()
            .find(|p| !p.archived)
            .or_else(|| projects.first()),
    };
    let scope: Vec<_> = if portfolio {
        projects.iter().collect()
    } else {
        selected.into_iter().collect()
    };
    let ids: Vec<_> = scope.iter().map(|p| p.id).collect();
    let data = HealthData::load(&state.db, auth.user_id, &ids).await?;
    let (today, timezone) = super::workspace_clock(&state.db, auth.user_id).await?;
    let mut sections = Vec::with_capacity(scope.len());
    let mut needing_attention = 0;
    let active = scope.iter().filter(|p| !p.archived).count();
    for project in scope {
        let coverage = github::coverage(&state, &auth, project.id).await?.into();
        let (summary, attention) = project_health(project, &data, coverage, today, portfolio);
        needing_attention += usize::from(attention);
        sections.push(summary);
    }
    let body = html! {
        header {
            h1 { @if portfolio { "Portfolio health" } @else { "Project health" } }
            p.muted { "As of " (day(today)) " · " (timezone) }
        }
        @if projects.is_empty() {
            p.empty { "Create a project to track milestone targets, blockers, and critical flags. " a.button href="/projects" { "Create a project" } }
        } @else {
            nav.toolbar aria-label="Health views" {
                form method="get" action="/health" {
                    label { "Project " select name="project" {
                        @for p in &projects {
                            option value=(p.id) selected[!portfolio && selected.is_some_and(|s| s.id == p.id)] {
                                (&p.name) @if p.archived { " (archived)" }
                            }
                        }
                    } }
                    button type="submit" { "View project" }
                }
                @if plan == "max" && !portfolio {
                    a.secondary href="/health?view=portfolio" { "Portfolio health" }
                }
                @if !portfolio && let Some(p) = selected {
                    a.secondary href=(format!("/roadmap?project={}", p.id)) { "Roadmap" }
                }
            }
            @if portfolio {
                p { strong { (needing_attention) " of " (active) } " active "
                    @if active == 1 { "project needs attention" } @else { "projects need attention" }
                    @if projects.len() > active { " · " (projects.len() - active) " archived" }
                }
            }
            div.stack { @for section in sections { (section) } }
        }
    };
    Ok(ui::page(
        &auth,
        if portfolio {
            "Portfolio health"
        } else {
            "Project health"
        },
        "health",
        body,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_means_missed_target_or_blocked_within_seven_days() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let mut m = MilestoneState {
            id: Uuid::nil(),
            project_id: Uuid::nil(),
            release_id: None,
            title: "Beta".into(),
            target_on: None,
            status: "planned".into(),
        };
        assert_eq!(Timing::of(&m, false, today), Timing::Unscheduled);
        assert_eq!(Timing::of(&m, true, today), Timing::Blocked);
        for (offset, blocked, expected) in [
            (-1, false, Timing::Overdue(1)),
            (0, true, Timing::BlockedSoon(0)),
            (7, true, Timing::BlockedSoon(7)),
            (8, true, Timing::Blocked),
            (2, false, Timing::Scheduled),
        ] {
            m.target_on = Some(today + chrono::Duration::days(offset));
            assert_eq!(Timing::of(&m, blocked, today), expected);
        }
        m.status = "reached".into();
        m.target_on = Some(today - chrono::Duration::days(2));
        assert_eq!(Timing::of(&m, true, today), Timing::Reached);
    }

    #[sqlx::test]
    async fn project_health_shows_owned_sources_and_gates_the_portfolio(db: PgPool) {
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id,email,timezone) VALUES ($1,'health@example.invalid','America/Los_Angeles'),($2,'other-health@example.invalid','UTC')")
            .bind(owner).bind(other).execute(&db).await.unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects (owner_id,key,name) VALUES ($1,'TEST','Tracker') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
        let foreign: Uuid = sqlx::query_scalar("INSERT INTO projects (owner_id,key,name) VALUES ($1,'EXT','Other workspace project') RETURNING id")
            .bind(other).fetch_one(&db).await.unwrap();
        let release: Uuid = sqlx::query_scalar("INSERT INTO releases (owner_id,project_id,name) VALUES ($1,$2,'Milestone release') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        let target: Uuid = sqlx::query_scalar("INSERT INTO milestones (owner_id,project_id,title,target_on) VALUES ($1,$2,'Public beta',(now() AT TIME ZONE 'America/Los_Angeles')::date+2) RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        sqlx::query("INSERT INTO milestone_releases (owner_id,project_id,milestone_id,release_id) VALUES ($1,$2,$3,$4)")
            .bind(owner).bind(project).bind(target).bind(release).execute(&db).await.unwrap();
        let member: Uuid = sqlx::query_scalar("INSERT INTO issues (owner_id,project_id,number,title,checkpoint_at) VALUES ($1,$2,1,'Install build','2026-06-01T00:05:00Z'::timestamptz) RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        let blocker: Uuid = sqlx::query_scalar("INSERT INTO issues (owner_id,project_id,number,title) VALUES ($1,$2,2,'Obtain certificate') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        sqlx::query("INSERT INTO issue_blockers (owner_id,project_id,issue_id,blocker_id) VALUES ($1,$2,$3,$4)")
            .bind(owner).bind(project).bind(member).bind(blocker).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO milestone_issues (owner_id,project_id,milestone_id,issue_id) VALUES ($1,$2,$3,$4)")
            .bind(owner).bind(project).bind(target).bind(member).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO issues (owner_id,project_id,number,title,status,manual_severity,security_critical) VALUES ($1,$2,3,'Still flagged','done','critical',true)")
            .bind(owner).bind(project).execute(&db).await.unwrap();
        let data = HealthData::load(&db, owner, &[project, foreign])
            .await
            .unwrap();
        assert!(data.blocked(target));
        assert_eq!(data.flags.len(), 1);
        assert_eq!(data.checkpoints.len(), 1);
        assert_eq!(
            data.checkpoints[0].checkpoint_on,
            NaiveDate::from_ymd_opt(2026, 5, 31).unwrap()
        );
        assert_eq!(data.milestones.first().unwrap().release_id, Some(release));
        let state = AppState::for_test(db);
        let actor = || Auth {
            user_id: owner,
            email: "health@example.invalid".into(),
            csrf: String::new(),
        };
        let q = |project, view: Option<&str>| {
            Query(HealthQuery {
                project,
                view: view.map(str::to_owned),
            })
        };
        let body = health(actor(), State(state.clone()), q(Some(project), None))
            .await
            .unwrap()
            .0;
        for label in [
            "Needs attention",
            "Blocked · due in 2 days",
            "Obtain certificate",
            "Latest checkpoint",
            "America/Los_Angeles",
            "31 May 2026",
            "Still flagged",
            "Milestone release",
        ] {
            assert!(body.contains(label), "{label}");
        }
        assert!(!body.contains("Other workspace project"));
        assert!(matches!(
            health(actor(), State(state.clone()), q(Some(foreign), None)).await,
            Err(AppError::NotFound)
        ));
        assert!(matches!(
            health(actor(), State(state.clone()), q(None, Some("portfolio"))).await,
            Err(AppError::Forbidden)
        ));
        sqlx::query("UPDATE users SET plan='max' WHERE id=$1")
            .bind(owner)
            .execute(&state.db)
            .await
            .unwrap();
        let body = health(actor(), State(state), q(None, Some("portfolio")))
            .await
            .unwrap()
            .0;
        assert!(body.contains("Portfolio health"));
        assert!(!body.contains("Other workspace project"));
    }

    #[test]
    fn unavailable_repositories_never_claim_zero_criticals() {
        let view = github_coverage(Uuid::nil(), (0, 2, 0).into()).into_string();
        assert!(view.contains("2 unavailable"));
        assert!(!view.contains("0 open critical"));
        let partial = github_coverage(Uuid::nil(), (3, 1, 2).into()).into_string();
        assert!(partial.contains("2 open critical in 3 checked repositories"));
        assert!(partial.contains("Coverage incomplete"));
    }
}
