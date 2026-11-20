use super::{Issue, KINDS, PRIORITIES, SEVERITIES, STATUSES, label};
use crate::{
    AppError, AppResult, AppState,
    auth::Auth,
    store::{self, Project},
    ui,
};
use axum::{
    extract::{Path, Query, State},
    response::Html,
};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::FromRow;
use uuid::Uuid;

const PAGE_SIZE: usize = 50;

#[derive(Default, Deserialize)]
pub(super) struct Filters {
    project: Option<String>,
    #[serde(default)]
    q: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    critical: String,
    #[serde(default)]
    view: String,
    p: Option<u32>,
}

impl Filters {
    fn check(&self) -> AppResult<Option<Uuid>> {
        if self.q.chars().count() > 250 {
            return Err(AppError::BadRequest(
                "Use up to 250 characters for search.".into(),
            ));
        }
        for (value, choices) in [
            (self.status.as_str(), STATUSES),
            (self.kind.as_str(), KINDS),
            (self.priority.as_str(), PRIORITIES),
            (self.scope.as_str(), &["active", "archived", "all"][..]),
            (self.view.as_str(), &["list", "board"][..]),
            (self.critical.as_str(), &["open"][..]),
        ] {
            if !value.is_empty() && !choices.contains(&value) {
                return Err(AppError::BadRequest("Choose a listed issue filter.".into()));
            }
        }
        self.project
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| {
                Uuid::parse_str(s)
                    .map_err(|_| AppError::BadRequest("Choose a valid project.".into()))
            })
            .transpose()
    }

    fn save_view_url(&self) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("save", "1");
        for (name, value) in [
            ("project", self.project.as_deref().unwrap_or_default()),
            ("q", self.q.as_str()),
            ("status", self.status.as_str()),
            ("priority", self.priority.as_str()),
            ("kind", self.kind.as_str()),
            ("critical", self.critical.as_str()),
            ("scope", self.scope.as_str()),
            ("view", self.view.as_str()),
        ] {
            if !value.is_empty() {
                query.append_pair(name, value);
            }
        }
        format!("/workflows?{}#views", query.finish())
    }

    fn board(&self) -> bool {
        self.view == "board"
    }
}

fn key_number(term: &str) -> Option<i64> {
    let (key, number) = term.trim().split_once('-')?;
    if key.is_empty()
        || key.len() > 12
        || !key.as_bytes()[0].is_ascii_alphabetic()
        || !key.bytes().all(|b| b.is_ascii_alphanumeric())
        || number.starts_with('0')
        || !number.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    number.parse().ok()
}

#[derive(FromRow)]
struct ListItem {
    id: Uuid,
    project_key: String,
    project_name: String,
    number: i64,
    title: String,
    status: String,
    priority: String,
    kind: String,
    labels: Vec<String>,
    manual_severity: String,
    finding_closed: bool,
    archived: bool,
    project_archived: bool,
    revision: i64,
}

pub(super) async fn list(
    State(state): State<AppState>,
    auth: Auth,
    Query(mut f): Query<Filters>,
) -> AppResult<Html<String>> {
    let project = f.check()?;
    let projects = store::projects(&state.db, auth.user_id).await?;
    if project.is_some_and(|id| !projects.iter().any(|p| p.id == id)) {
        return Err(AppError::NotFound);
    }
    let selected = projects.iter().find(|p| Some(p.id) == project);
    if f.scope.is_empty() {
        f.scope = if f.critical == "open" || selected.is_some_and(|p| p.archived) {
            "all"
        } else {
            "active"
        }
        .into();
    }
    let page = f.p.unwrap_or(1).max(1);
    let offset = i64::from(page - 1) * PAGE_SIZE as i64;
    let mut items: Vec<ListItem> = sqlx::query_as(
        "SELECT i.id, p.key AS project_key, p.name AS project_name, i.number, i.title, i.status,
                i.priority, i.kind, i.labels, i.manual_severity, i.finding_closed, i.archived,
                p.archived AS project_archived, i.revision
         FROM issues i JOIN projects p ON (p.owner_id,p.id)=(i.owner_id,i.project_id)
         WHERE i.owner_id=$1 AND ($2::uuid IS NULL OR i.project_id=$2)
           AND ($3='' OR i.id IN (
               SELECT x.id FROM issues x
                 WHERE x.owner_id=$1 AND issue_search_vector(x.title,x.body,x.next_action) @@ websearch_to_tsquery('simple',$3)
               UNION
               SELECT x.id FROM issues x
                 WHERE x.owner_id=$1 AND issue_search_labels(x.labels) @> ARRAY[lower($3)]
               UNION
               SELECT x.id FROM issues x JOIN projects kp ON (x.owner_id,x.project_id)=(kp.owner_id,kp.id)
                 WHERE x.owner_id=$1 AND kp.key=upper(split_part($3,'-',1)) AND x.number=$11::bigint
               UNION
               SELECT x.id FROM issues x
                 WHERE x.owner_id=$1 AND octet_length(x.body)>65536 AND strpos(lower(x.body),lower($3))>0))
           AND ($4='' OR i.status=$4) AND ($5='' OR i.kind=$5) AND ($6='' OR i.priority=$6)
           AND ($7='all' OR (i.archived OR p.archived)=($7='archived'))
           AND ($10='' OR (i.security_critical AND NOT i.finding_closed AND NOT i.archived))
         ORDER BY i.updated_at DESC,i.id LIMIT $8 OFFSET $9"
    ).bind(auth.user_id).bind(project).bind(f.q.trim()).bind(&f.status).bind(&f.kind).bind(&f.priority)
     .bind(&f.scope).bind(PAGE_SIZE as i64 + 1).bind(offset).bind(&f.critical).bind(key_number(&f.q)).fetch_all(&state.db).await?;
    let more = items.len() > PAGE_SIZE;
    items.truncate(PAGE_SIZE);
    let can_create = selected.map_or_else(|| projects.iter().any(|p| !p.archived), |p| !p.archived);
    Ok(ui::page(
        &auth,
        "Issues",
        "issues",
        html! {
            header.page-head {
                div { @if let Some(p) = selected { a.muted href=(format!("/projects/{}",p.id)) { (&p.name) } } h1 { "Issues" } }
                div.actions { a.btn href="/workflows" { "Saved views and templates" } @if can_create { a.btn.primary href="#new-issue" { "New issue" } } }
            }
            (filter_bar(&f,&projects,project))
            @if items.is_empty() {
                section.panel.empty { h2 { "No matching issues" }
                    @if projects.is_empty() { p { "Create a project to start tracking work." } a.btn href="/projects" { "Create a project" } }
                    @else if !f.q.is_empty() || !f.status.is_empty() || !f.kind.is_empty() || !f.priority.is_empty() || page > 1 || f.scope == "archived" || f.critical == "open" { a href=(project.map_or_else(|| "/issues".into(), |id| format!("/issues?project={id}"))) { "Clear filters" } }
                    @else if can_create { a href="#new-issue" { "Create the first issue" } }
                }
            } @else if f.board() {
                div.board { @for &status in STATUSES { @if f.status.is_empty() || f.status == status {
                    section.panel.board-column aria-label=(label(status)) { h2 { (label(status)) }
                        @for item in items.iter().filter(|i| i.status == status) { article.issue-card {
                            p.muted { (&item.project_key) "-" (item.number) }
                            h3 { a href=(format!("/issues/{}",item.id)) { (&item.title) } }
                            p { (label(&item.kind)) " · " (label(&item.priority)) }
                            (issue_tags(item))
                            @if !item.archived && !item.project_archived { (status_form(&auth,item.id,item.revision,&item.status)) }
                        } }
                        @if !items.iter().any(|i| i.status == status) { p.muted { "No issues on this page" } }
                    }
                } } }
            } @else {
                div.table-wrap { table { thead { tr { th scope="col" { "Issue" } th scope="col" { "Status" } th scope="col" { "Priority" } th scope="col" { "Type" } } }
                    tbody { @for item in &items { tr {
                        td { div.muted { (&item.project_name) " · " (&item.project_key) "-" (item.number) }
                            a href=(format!("/issues/{}",item.id)) { (&item.title) } (issue_tags(item)) }
                        td { (label(&item.status)) } td { (label(&item.priority)) } td { (label(&item.kind)) }
                    } } }
                } }
            }
            @if more || page > 1 { nav.actions aria-label="Issue result pages" {
                @if page > 1 { (page_button(&f,page-1,"Previous page")) }
                span { "Page " (page) }
                @if more { (page_button(&f,page.saturating_add(1),"Next page")) }
            } }
            @if can_create { (new_issue(&auth,&projects,project)) }
        },
    ))
}

fn hidden(name: &str, value: impl std::fmt::Display) -> Markup {
    html! { input type="hidden" name=(name) value=(value); }
}

fn command(auth: &Auth, revision: Option<i64>) -> Markup {
    html! { (ui::csrf(auth)) (hidden("command_id", Uuid::new_v4())) @if let Some(r) = revision { (hidden("revision", r)) } }
}

fn options(values: &[&str], selected: &str) -> Markup {
    html! { @for &v in values { option value=(v) selected[v == selected] { (label(v)) } } }
}

fn optional_filter(name: &str, title: &str, values: &[&str], selected: &str) -> Markup {
    html! { label { (title) select name=(name) { option value="" { "All" } (options(values, selected)) } } }
}

fn filter_bar(f: &Filters, projects: &[Project], selected: Option<Uuid>) -> Markup {
    html! { form.panel.filters method="get" action="/issues" role="search" id="search" {
        label { "Search issues" input type="search" name="q" maxlength="250" placeholder="Words, issue key, or label" aria-describedby="issue-search-help" value=(&f.q); small id="issue-search-help" { "For long descriptions, search an exact phrase." } }
        label { "Project" select name="project" { option value="" { "All projects" } @for p in projects { option value=(p.id) selected[Some(p.id) == selected] { (&p.name) @if p.archived { " (archived)" } } } } }
        (optional_filter("status", "Status", STATUSES, &f.status))
        (optional_filter("priority", "Priority", PRIORITIES, &f.priority))
        (optional_filter("kind", "Type", KINDS, &f.kind))
        label { "Scope" select name="scope" {
            option value="active" selected[f.scope == "active"] { "Active" }
            option value="archived" selected[f.scope == "archived"] { "Archived" } option value="all" selected[f.scope == "all"] { "Include archived" }
        } }
        label { input type="checkbox" name="critical" value="open" checked[f.critical == "open"]; " Open manual criticals" }
        label { "View" select name="view" { option value="list" selected[!f.board()] { "List" } option value="board" selected[f.board()] { "Board" } } }
        button type="submit" { "Apply" }
        a href=(f.save_view_url()) { "Save this view" }
    } }
}

fn page_button(f: &Filters, page: u32, title: &str) -> Markup {
    html! { form action="/issues" method="get" { (hidden("project",f.project.as_deref().unwrap_or(""))) (hidden("q",&f.q))
        (hidden("status",&f.status)) (hidden("priority",&f.priority)) (hidden("kind",&f.kind)) (hidden("scope",&f.scope)) (hidden("critical",&f.critical)) (hidden("view",&f.view))
        button type="submit" name="p" value=(page) { (title) }
    } }
}

fn issue_tags(item: &ListItem) -> Markup {
    html! { @if item.archived || item.project_archived || !item.labels.is_empty() || item.manual_severity != "none" {
        div.tags {
            @if item.archived || item.project_archived { span.badge { "Archived" } }
            @if item.manual_severity != "none" { span.badge { (label(&item.manual_severity)) " finding" @if item.finding_closed { " · closed" } } }
            @for tag in &item.labels { span.tag { (tag) } }
        }
    } }
}

fn new_issue(auth: &Auth, projects: &[Project], selected: Option<Uuid>) -> Markup {
    html! { section.panel id="new-issue" { h2 { "New issue" }
        form.stack action="/issues" method="post" {
            (command(auth,None))
            label { "Project" select name="project" required { @for p in projects.iter().filter(|p| !p.archived) {
                option value=(p.id) selected[Some(p.id) == selected] { (&p.name) }
            } } }
            label { "Title" input name="title" maxlength="240" required; }
            label { "Description" textarea name="body" rows="3" {} }
            label { "Next action" textarea name="next_action" rows="2" maxlength="2000" aria-describedby="new-action-hint" {} }
            small.muted id="new-action-hint" { "Adding a next action to an open issue saves a checkpoint on Home." }
            details { summary { "Issue settings" } (settings_fields(None)) }
            div.actions { button.primary type="submit" { "Create issue" } }
        }
    } }
}

fn settings_fields(issue: Option<&Issue>) -> Markup {
    html! { div.grid {
        label { "Status" select name="status" { (options(STATUSES,issue.map_or("ready",|i| &i.status))) } }
        label { "Priority" select name="priority" { (options(PRIORITIES,issue.map_or("normal",|i| &i.priority))) } }
        label { "Type" select name="kind" { (options(KINDS,issue.map_or("feature",|i| &i.kind))) } }
        label { "Labels" input name="labels" placeholder="Comma-separated labels" value=(issue.map_or_else(String::new, |i| i.labels.join(", "))); }
        label { "Manual finding severity" select name="severity" { (options(SEVERITIES,issue.map_or("none",|i| &i.manual_severity))) } }
        @if let Some(i) = issue { label { input type="checkbox" name="finding_closed" value="true" checked[i.finding_closed]; " Finding closed" } }
    } }
}

fn status_form(auth: &Auth, id: Uuid, revision: i64, status: &str) -> Markup {
    html! { form.actions method="post" action=(format!("/issues/{id}/status")) { (command(auth,Some(revision)))
        label { "Status" select name="status" { (options(STATUSES,status)) } }
        button type="submit" { "Update status" }
    } }
}

#[derive(Default, Deserialize)]
pub(super) struct DetailQuery {
    #[serde(default)]
    blocker_q: String,
    history_page: Option<u32>,
}

#[derive(FromRow)]
struct Peer {
    id: Uuid,
    number: i64,
    title: String,
    status: String,
    archived: bool,
}

#[derive(FromRow)]
struct HistoryRow {
    action: String,
    old_status: Option<String>,
    new_status: String,
    occurred_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct Origin {
    source_id: String,
    parent_source: Option<String>,
    parent_id: Option<Uuid>,
    number: Option<i64>,
    title: Option<String>,
}

pub(super) async fn detail(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Query(q): Query<DetailQuery>,
) -> AppResult<Html<String>> {
    let db = &state.db;
    let owner = auth.user_id;
    let issue: Issue = sqlx::query_as("SELECT * FROM issues WHERE owner_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)?;
    let project = store::project(db, owner, issue.project_id).await?;
    let can_write = !project.archived && !issue.archived;
    let open = matches!(issue.status.as_str(), "ready" | "backlog" | "in_progress");
    if q.blocker_q.chars().count() > 250 {
        return Err(AppError::BadRequest(
            "Use up to 250 characters to find a blocker.".into(),
        ));
    }
    let history_page = q.history_page.unwrap_or(1).max(1);
    const HISTORY_SIZE: usize = 25;
    let offset = i64::from(history_page - 1) * HISTORY_SIZE as i64;
    let (blockers, dependents, mut history, origin) = tokio::try_join!(
        sqlx::query_as::<_, Peer>("SELECT i.id,i.number,i.title,i.status,i.archived FROM issue_blockers b JOIN issues i ON (i.owner_id,i.project_id,i.id)=(b.owner_id,b.project_id,b.blocker_id) WHERE b.owner_id=$1 AND b.issue_id=$2 ORDER BY i.number").bind(owner).bind(id).fetch_all(db),
        sqlx::query_as::<_, Peer>("SELECT i.id,i.number,i.title,i.status,i.archived FROM issue_blockers b JOIN issues i ON (i.owner_id,i.project_id,i.id)=(b.owner_id,b.project_id,b.issue_id) WHERE b.owner_id=$1 AND b.blocker_id=$2 ORDER BY i.number").bind(owner).bind(id).fetch_all(db),
        sqlx::query_as::<_, HistoryRow>("SELECT action,old_status,new_status,occurred_at FROM issue_events WHERE owner_id=$1 AND issue_id=$2 ORDER BY occurred_at DESC,revision_after DESC NULLS LAST,id DESC LIMIT $3 OFFSET $4").bind(owner).bind(id).bind(HISTORY_SIZE as i64 + 1).bind(offset).fetch_all(db),
        sqlx::query_as::<_, Origin>("SELECT l.source_id,l.parent_source,p.id AS parent_id,p.number,p.title FROM transfer_linear l LEFT JOIN issues p ON (p.owner_id,p.project_id,p.id)=(l.owner_id,l.project_id,l.parent_id) WHERE l.owner_id=$1 AND l.issue_id=$2").bind(owner).bind(id).fetch_optional(db),
    )?;
    let older = history.len() > HISTORY_SIZE;
    history.truncate(HISTORY_SIZE);
    let candidates: Vec<Peer> = if can_write {
        sqlx::query_as("SELECT i.id,i.number,i.title,i.status,i.archived FROM issues i
            WHERE i.owner_id=$1 AND i.project_id=$2 AND i.id<>$3 AND NOT i.archived
              AND NOT EXISTS (SELECT 1 FROM issue_blockers b WHERE b.owner_id=$1 AND b.issue_id=$3 AND b.blocker_id=i.id)
              AND ($4='' OR strpos(lower(i.title),lower($4))>0 OR lower($5 || '-' || i.number::text)=lower($4))
            ORDER BY i.updated_at DESC,i.id LIMIT 30")
            .bind(owner).bind(issue.project_id).bind(id).bind(q.blocker_q.trim()).bind(&project.key).fetch_all(db).await?
    } else {
        Vec::new()
    };
    let (milestones, release_scope, shipments) = tokio::try_join!(
        sqlx::query_as::<_,(Uuid,String)>(
            "SELECT m.id,m.title FROM milestone_issues mi
             JOIN milestones m ON (m.owner_id,m.project_id,m.id)=(mi.owner_id,mi.project_id,mi.milestone_id)
             WHERE mi.owner_id=$1 AND mi.project_id=$2 AND mi.issue_id=$3
             ORDER BY m.target_on NULLS LAST,m.title,m.id"
        ).bind(owner).bind(issue.project_id).bind(id).fetch_all(db),
        sqlx::query_as::<_,(Uuid,String)>("SELECT r.id,r.name FROM release_members m JOIN releases r ON r.id=m.release_id AND r.owner_id=m.owner_id WHERE m.owner_id=$1 AND m.issue_id=$2 ORDER BY r.name,r.id").bind(owner).bind(id).fetch_all(db),
        sqlx::query_as::<_,(Uuid,String)>("SELECT r.id,r.name FROM release_entries e JOIN releases r ON r.id=e.release_id AND r.owner_id=e.owner_id WHERE e.owner_id=$1 AND e.issue_id=$2 GROUP BY r.id,r.name,r.published_on ORDER BY r.published_on DESC,r.id").bind(owner).bind(id).fetch_all(db),
    )?;
    let (notes_result, github) = tokio::join!(
        crate::notes::issue_panel(db, owner, id),
        crate::github::issue_panel(&state, &auth, id)
    );
    let notes = notes_result?;
    let key = format!("{}-{}", project.key, issue.number);
    let can_undo = if !project.archived && history_page == 1 {
        sqlx::query_scalar::<_,bool>("SELECT EXISTS (SELECT 1 FROM issue_events WHERE owner_id=$1 AND issue_id=$2 AND revision_after=$3 AND before_state IS NOT NULL AND action IN ('status','edit','archive','checkpoint','blocker') AND (NOT $4 OR action='archive'))")
            .bind(owner).bind(id).bind(issue.revision).bind(issue.archived).fetch_one(db).await?
    } else {
        false
    };
    Ok(ui::page(
        &auth,
        &format!("{key} · {}", issue.title),
        "issues",
        html! {
            nav.breadcrumbs aria-label="Breadcrumb" { a href=(format!("/issues?project={}",project.id)) { "Issues" } " / " a href=(format!("/projects/{}",project.id)) { (&project.name) } }
            header.page-head {
                div { p.muted { (&key) } h1 { (&issue.title) } p { (label(&issue.status)) " · " (label(&issue.priority)) " priority · " (label(&issue.kind)) }
                    @if !issue.labels.is_empty() { div.tags { @for tag in &issue.labels { span.tag { (tag) } } } }
                }
                @if can_write { a.btn href="#edit-issue" { "Edit issue" } }
            }
            @if project.archived || issue.archived { p.notice { @if project.archived { "This project is archived." } @else { "This issue is archived." } } }
            @if issue.manual_severity != "none" {
                section.panel { h2 { "Manual security finding" }
                    p { (label(&issue.manual_severity)) " · " @if issue.finding_closed { "Closed" } @else { "Open" } }
                    @if can_write { a href="#finding-settings" { "Edit finding" } }
                }
            }
            section.panel { h2 { "Next action" }
                @if !issue.next_action.is_empty() { p.prose { (&issue.next_action) } }
                @else { p.muted { "No next action saved." } }
                @if let Some(t) = issue.checkpoint_at { p.muted { "Checkpoint saved " (timestamp(t)) } }
                @if can_write && open {
                    form.stack method="post" action=(format!("/issues/{id}/checkpoint")) { (command(&auth,Some(issue.revision)))
                        label { "Checkpoint next action" textarea name="next_action" required maxlength="2000" rows="2" { (&issue.next_action) } }
                        div.actions { button.primary type="submit" { "Save checkpoint" } }
                    }
                }
            }
            @if !issue.body.is_empty() { section.panel { h2 { "Description" } div.prose { (&issue.body) } } }
            @if can_write { (status_form(&auth,id,issue.revision,&issue.status)) }
            section.panel id="blockers" { h2 { "Dependencies" }
                h3 { "Blocked by" } (peers(&project.key,&blockers,&auth,&issue,can_write))
                h3 { "Blocks" } (peers(&project.key,&dependents,&auth,&issue,false))
                @if can_write {
                    details open[!q.blocker_q.is_empty()] { summary { "Add a blocker" }
                        form.actions method="get" action=(format!("/issues/{id}#blockers")) {
                            label { "Find a project issue" input type="search" name="blocker_q" value=(&q.blocker_q) maxlength="250" placeholder="Title or issue key"; }
                            button type="submit" { "Find" }
                        }
                        @if candidates.is_empty() { p.muted { "No matching issues available." } }
                        @else {
                            form.actions method="post" action=(format!("/issues/{id}/blocker")) { (command(&auth,Some(issue.revision))) (hidden("operation","add"))
                                label { "Blocker" select name="blocker_id" required { @for c in &candidates { option value=(c.id) { (&project.key) "-" (c.number) " · " (&c.title) } } } }
                                button type="submit" { "Add blocker" }
                            }
                        }
                    }
                }
            }
            @if let Some(o) = origin { section.panel { h2 { "Linear import" }
                p { "Source issue " (o.source_id) }
                @if let (Some(parent),Some(number),Some(title))=(o.parent_id,o.number,o.title) {
                    p { "Parent " a href=(format!("/issues/{parent}")) { (&project.key) "-" (number) " · " (title) } }
                } @else if let Some(source) = o.parent_source { p.muted { "Parent “" (source) "” was not found in this project." } }
            } }
            div id="linked-notes" { (notes) }
            div id="pull-requests" { (github) }
            div.grid {
                section.panel {
                    h2 { "Milestones" }
                    @if milestones.is_empty() { p.muted { "No linked milestones." } }
                    @else { ul { @for (milestone,title) in &milestones { li {
                        a href=(format!("/roadmap?project={}&milestone={milestone}#milestone-detail",project.id)) { (title) }
                    } } } }
                    a href=(format!("/roadmap?project={}",project.id)) { "Open project roadmap" }
                }
                section.panel { h2 { "Releases" }
                    @if !release_scope.is_empty() { h3 { "In scope" } (release_links(&release_scope)) }
                    @if !shipments.is_empty() { h3 { "Recorded shipments" } (release_links(&shipments)) }
                    @if release_scope.is_empty() && shipments.is_empty() { p.muted { "No linked releases." } }
                    a href=(format!("/releases?project={}",project.id)) { "Open project releases" }
                }
            }
            section.panel id="history" { h2 { "Issue history" }
                @if history.is_empty() { p.muted { "No recorded events on this page." } }
                @else { ol.timeline { @for event in &history { li {
                    strong { (event_title(&event.action)) } " · " (timestamp(event.occurred_at))
                    @if event.old_status.as_deref() != Some(&event.new_status) {
                        span { " · " @if let Some(old) = &event.old_status { (label(old)) " → " } (label(&event.new_status)) }
                    }
                } } } }
                @if can_undo { form method="post" action=(format!("/issues/{id}/undo")) { (command(&auth,Some(issue.revision))) button type="submit" { "Undo latest change" } } }
                @if older || history_page > 1 { nav.actions aria-label="History pages" {
                    @if history_page > 1 { a href=(format!("/issues/{id}?history_page={}#history",history_page-1)) { "Newer events" } }
                    @if older { a href=(format!("/issues/{id}?history_page={}#history",history_page.saturating_add(1))) { "Older events" } }
                } }
            }
            @if can_write { (edit_issue(&auth,&issue)) }
            @if !project.archived {
                section.panel { h2 { @if issue.archived { "Restore issue" } @else { "Archive issue" } }
                    form method="post" action=(format!("/issues/{id}/archive")) { (command(&auth,Some(issue.revision)))
                        (hidden("archived",!issue.archived))
                        button type="submit" { @if issue.archived { "Restore issue" } @else { "Archive issue" } }
                    }
                }
            }
        },
    ))
}

fn peers(key: &str, rows: &[Peer], auth: &Auth, issue: &Issue, removable: bool) -> Markup {
    html! { @if rows.is_empty() { p.muted { "None" } } @else { ul { @for p in rows { li {
        a href=(format!("/issues/{}",p.id)) { (key) "-" (p.number) " · " (&p.title) }
        " · " (label(&p.status)) @if p.archived { " · Archived" }
        @if removable { form.inline method="post" action=(format!("/issues/{}/blocker",issue.id)) {
            (command(auth,Some(issue.revision))) (hidden("operation","remove")) (hidden("blocker_id",p.id))
            button type="submit" aria-label=(format!("Remove blocker {key}-{}",p.number)) { "Remove" }
        } }
    } } } } }
}

fn release_links(releases: &[(Uuid, String)]) -> Markup {
    html! { ul { @for (id, name) in releases { li { a href=(format!("/releases/{id}")) { (name) } } } } }
}

fn timestamp(t: DateTime<Utc>) -> Markup {
    html! { time datetime=(t.to_rfc3339()) { (t.format("%d %b %Y, %H:%M UTC")) } }
}

fn event_title(value: &str) -> &str {
    match value {
        "create" => "Issue created",
        "edit" => "Issue edited",
        "status" => "Status recorded",
        "checkpoint" => "Checkpoint saved",
        "archive" => "Archive state changed",
        "blocker" => "Dependencies changed",
        "undo" => "Change undone",
        other => other,
    }
}

fn edit_issue(auth: &Auth, i: &Issue) -> Markup {
    html! { section.panel id="edit-issue" { h2 { "Edit issue" }
        form.stack method="post" action=(format!("/issues/{}/edit",i.id)) { (command(auth,Some(i.revision)))
            label { "Title" input name="title" maxlength="240" value=(&i.title) required; }
            label { "Description" textarea name="body" rows="5" { (&i.body) } }
            label { "Next action" textarea name="next_action" rows="2" maxlength="2000" { (&i.next_action) } }
            div id="finding-settings" { (settings_fields(Some(i))) }
            div.actions { button.primary type="submit" { "Save changes" } }
        }
    } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_key_search_requires_a_canonical_project_key_and_positive_number() {
        assert_eq!(key_number(" m2-13 "), Some(13));
        for invalid in [
            "-1",
            "1M-2",
            "M-0",
            "M-01",
            "M-123-2",
            "M-+1",
            "M-9223372036854775808",
        ] {
            assert_eq!(key_number(invalid), None);
        }
    }

    #[test]
    fn workspace_filter_accepts_the_empty_project_submitted_by_a_browser() {
        let filters = Filters {
            project: Some(String::new()),
            view: "board".into(),
            critical: "open".into(),
            ..Default::default()
        };
        assert!(matches!(filters.check(), Ok(None)));
        assert!(filters.board());
        let malformed = Filters {
            project: Some("other-workspace".into()),
            ..Default::default()
        };
        assert!(malformed.check().is_err());
    }

    #[test]
    fn search_and_pagination_forms_escape_and_preserve_filters() {
        let filters = Filters {
            q: "Widget <draft>".into(),
            scope: "all".into(),
            critical: "open".into(),
            priority: "high".into(),
            view: "board".into(),
            ..Default::default()
        };
        let html = page_button(&filters, 2, "Next page").into_string();
        for value in ["Widget &lt;draft&gt;", "all", "open", "high", "board", "2"] {
            assert!(html.contains(&format!("value=\"{value}\"")));
        }
        assert!(!html.contains("<draft>"));
        let save = filters.save_view_url();
        assert!(save.contains("q=Widget+%3Cdraft%3E"));
        assert!(save.contains("critical=open"));
        assert!(save.ends_with("#views"));
        let invalid = Filters {
            status: "unlisted".into(),
            ..Default::default()
        };
        assert!(invalid.check().is_err());
    }
}
