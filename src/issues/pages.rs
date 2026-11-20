use super::{Issue, KINDS, PRIORITIES, SEVERITIES, STATUSES, label};
use crate::{
    AppError, AppResult, AppState,
    auth::Auth,
    store::{self, Project},
    ui,
    workflows::{VIEW_GROUPS, VIEW_SORTS},
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
    #[serde(default)]
    sort: String,
    #[serde(default)]
    group: String,
    p: Option<u32>,
}

impl Filters {
    fn check(&self) -> AppResult<Option<Uuid>> {
        if self.q.contains('\0') || self.q.chars().count() > 250 {
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
        for (value, options) in [
            (self.sort.as_str(), VIEW_SORTS),
            (self.group.as_str(), VIEW_GROUPS),
        ] {
            if !value.is_empty() && !options.iter().any(|(key, _)| key == &value) {
                return Err(AppError::BadRequest(
                    "Choose a listed display option.".into(),
                ));
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

    fn parameters(&self) -> [(&'static str, &str); 10] {
        [
            ("project", self.project.as_deref().unwrap_or_default()),
            ("q", self.q.as_str()),
            ("status", self.status.as_str()),
            ("priority", self.priority.as_str()),
            ("kind", self.kind.as_str()),
            ("critical", self.critical.as_str()),
            ("scope", self.scope.as_str()),
            ("view", self.view.as_str()),
            ("group", self.group.as_str()),
            ("sort", self.sort.as_str()),
        ]
    }

    fn view_url(&self, view: &str) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.extend_pairs(
            self.parameters()
                .into_iter()
                .filter(|(k, v)| *k != "view" && !v.is_empty()),
        );
        query.append_pair("view", view);
        format!("/issues?{}", query.finish())
    }

    fn filter_count(&self) -> usize {
        [
            self.project.as_deref().is_some_and(|v| !v.is_empty()),
            !self.status.is_empty(),
            !self.priority.is_empty(),
            !self.kind.is_empty(),
            self.scope != "active",
            !self.critical.is_empty(),
        ]
        .into_iter()
        .filter(|v| *v)
        .count()
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
    updated_at: DateTime<Utc>,
    matched: i64,
    checkpoint: bool,
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
    if f.sort.is_empty() {
        f.sort = "updated".into();
    }
    if f.group.is_empty() || (f.board() && f.group == "none") {
        f.group = "status".into();
    }
    let page = f.p.unwrap_or(1).max(1);
    let offset = i64::from(page - 1) * PAGE_SIZE as i64;
    let ordering = issue_order(&f);
    let query = format!(
        "SELECT i.id, p.key AS project_key, p.name AS project_name, i.number, i.title, i.status,
                i.priority, i.kind, i.labels, i.manual_severity, i.finding_closed, i.archived,
                p.archived AS project_archived, i.revision, i.updated_at, count(*) OVER () AS matched,
                i.id IS NOT DISTINCT FROM (
                    SELECT c.id FROM issues c JOIN projects cp ON (cp.owner_id,cp.id)=(c.owner_id,c.project_id)
                    WHERE c.owner_id=$1 AND ($2::uuid IS NULL OR c.project_id=$2) AND NOT c.archived AND NOT cp.archived
                      AND c.status IN ('backlog','ready','in_progress') AND c.checkpoint_at IS NOT NULL
                    ORDER BY c.checkpoint_at DESC, c.updated_at DESC, c.id LIMIT 1
                ) AS checkpoint
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
         ORDER BY {ordering} LIMIT $8 OFFSET $9"
    );
    let mut items: Vec<ListItem> = sqlx::query_as(&query)
        .bind(auth.user_id)
        .bind(project)
        .bind(f.q.trim())
        .bind(&f.status)
        .bind(&f.kind)
        .bind(&f.priority)
        .bind(&f.scope)
        .bind(PAGE_SIZE as i64 + 1)
        .bind(offset)
        .bind(&f.critical)
        .bind(key_number(&f.q))
        .fetch_all(&state.db)
        .await?;
    let more = items.len() > PAGE_SIZE;
    items.truncate(PAGE_SIZE);
    let inbox = f.status == "backlog" && f.scope == "active";
    let title = if inbox { "Inbox" } else { "Issues" };
    let inbox_clear = inbox
        && f.q.is_empty()
        && f.kind.is_empty()
        && f.priority.is_empty()
        && f.critical.is_empty()
        && page == 1;
    let can_create = selected.map_or_else(|| projects.iter().any(|p| !p.archived), |p| !p.archived);
    Ok(ui::page(
        &auth,
        title,
        if inbox { "inbox" } else { "issues" },
        html! {
            header.page-head.issue-heading {
                div {
                    @if let Some(p) = selected { a.issue-project-crumb href=(format!("/projects/{}",p.id)) { (&p.name) } }
                    div.issue-heading-line { h1 { (title) }
                        @if let Some(first) = items.first() {
                            span.issue-total title="Matching issues" { (first.matched) }
                        }
                    }
                    @if inbox { p.issue-subheading { "Backlog issues awaiting triage." } }
                }
                div.actions {
                    a.issue-saved href="/workflows#views" { (ledger_icon("bookmark")) span { "Saved views" } }
                    @if can_create { a.btn.primary.issue-create href="#new-issue" { (ui::icon("plus")) "New issue" } }
                }
            }
            (filter_bar(&f,&projects,project))
            @if items.is_empty() {
                section.empty.issue-empty { h2 { @if inbox_clear { "Inbox is clear" } @else { "No matching issues" } }
                    @if projects.is_empty() { p { "Create a project to start tracking work." } a.btn href="/projects" { "Create a project" } }
                    @else if inbox_clear {
                        p { "Issues with Backlog status appear here." }
                        @if can_create { a href="#new-issue" { "Add a backlog issue" } }
                    } @else if !f.q.is_empty() || !f.status.is_empty() || !f.priority.is_empty() || !f.kind.is_empty() || !f.critical.is_empty() || f.scope != "active" || page > 1 {
                        a href=(project.map_or_else(|| "/issues".into(), |id| format!("/issues?project={id}"))) { "Clear filters" }
                    } @else if can_create { a href="#new-issue" { "Create the first issue" } }
                }
            } @else {
                (issue_results(&auth, &f, &items))
            }
            @if more || page > 1 { nav.actions aria-label="Issue result pages" {
                @if page > 1 { (page_button(&f,page-1,"Previous page")) }
                span { "Page " (page) @if let Some(i) = items.first() { " of " ((i.matched + PAGE_SIZE as i64 - 1) / PAGE_SIZE as i64) } }
                @if more { (page_button(&f,page.saturating_add(1),"Next page")) }
            } }
            @if can_create { (new_issue(&auth,&projects,project,if inbox { "backlog" } else { "ready" })) }
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

const STATUS_GROUPS: &[&str] = &["in_progress", "ready", "backlog", "done", "canceled"];
const PRIORITY_GROUPS: &[&str] = &["urgent", "high", "normal", "low"];
const STATUS_ORDER: &str = "CASE i.status WHEN 'in_progress' THEN 0 WHEN 'ready' THEN 1 WHEN 'backlog' THEN 2 WHEN 'done' THEN 3 ELSE 4 END";
const PRIORITY_ORDER: &str =
    "CASE i.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END";

fn issue_order(f: &Filters) -> String {
    // Every fragment is fixed here. The UUID makes ties stable between page requests.
    let sort = match f.sort.as_str() {
        "priority" => format!("{PRIORITY_ORDER}, i.updated_at DESC"),
        "newest" => "i.created_at DESC".into(),
        "title" => "lower(i.title), i.title".into(),
        _ => "i.updated_at DESC".into(),
    };
    let (field, rank) = match f.group.as_str() {
        "priority" => ("i.priority", PRIORITY_ORDER),
        "none" => return format!("{sort}, i.id"),
        _ => ("i.status", STATUS_ORDER),
    };
    if f.board() {
        // Interleave lanes before paging so one long backlog cannot hide other work.
        format!("row_number() OVER (PARTITION BY {field} ORDER BY {sort}, i.id), {rank}, i.id")
    } else {
        format!("{rank}, {sort}, i.id")
    }
}

fn ledger_icon(name: &str) -> Markup {
    let path = match name {
        "list" => "M3 5h11M3 9h11M3 13h11",
        "board" => "M2.5 3.5h5v11h-5zM10.5 3.5h5v7h-5z",
        "filter" => "M2 4.5h14M4.8 9h8.4M7 13.5h4",
        "bookmark" => "M4 2.5h10v13l-5-3.3L4 15.5z",
        "chevron" => "m6 7 3 3 3-3",
        _ => return html! {},
    };
    html! { svg.ledger-icon viewBox="0 0 18 18" width="16" height="16" fill="none" stroke="currentColor"
    stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" { path d=(path) {} } }
}

fn status_mark(status: &str) -> Markup {
    html! {
        svg.issue-state-mark data-status=(status) viewBox="0 0 16 16" width="14" height="14"
            fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true" {
            circle cx="8" cy="8" r="5.7" stroke-dasharray=[(status == "backlog").then_some("2 2")] {}
            @match status {
                "in_progress" => { path d="M8 3.7a4.3 4.3 0 0 1 0 8.6z" fill="currentColor" stroke="none" {} }
                "done" => { path d="m5.1 8.1 2 2.1 4.1-4.4" stroke-linecap="round" stroke-linejoin="round" {} }
                "canceled" => { path d="m5.2 10.8 5.6-5.6" {} }
                _ => {}
            }
        }
    }
}

fn priority_mark(priority: &str) -> Markup {
    let bars = match priority {
        "urgent" => 4,
        "high" => 3,
        "normal" => 2,
        _ => 1,
    };
    html! {
        svg.issue-priority-mark data-priority=(priority) viewBox="0 0 16 16" width="14" height="14"
            fill="currentColor" aria-hidden="true" {
            @for (index, height) in [3, 5, 8, 11].iter().enumerate() {
                rect x=(1 + index * 4) y=(14 - height) width="2.25" height=(height) rx=".4"
                    opacity=(if index < bars { "1" } else { ".22" }) {}
            }
        }
    }
}

fn group_mark(group: &str, value: &str) -> Markup {
    if group == "priority" {
        priority_mark(value)
    } else {
        status_mark(value)
    }
}

fn group_heading(group: &str, value: &str, shown: usize) -> Markup {
    html! {
        (group_mark(group, value)) span { (label(value)) }
        span.issue-group-count title="Issues shown on this page" { (shown) }
    }
}

fn issue_row(item: &ListItem) -> Markup {
    html! {
        li {
            a.issue-ledger-row data-checkpoint=[item.checkpoint.then_some("current")] href=(format!("/issues/{}", item.id)) {
                span.issue-ref title=(&item.project_name) {
                    @if item.checkpoint { span.checkpoint-pin title="Latest checkpoint" { span.sr-only { "Latest checkpoint · " } } } (&item.project_key) "-" (item.number) }
                span.issue-name-cell {
                    span.issue-item-state title=(label(&item.status)) {
                        (status_mark(&item.status)) span.sr-only { (label(&item.status)) }
                    }
                    span.issue-title { (&item.title) }
                    @if item.archived || item.project_archived { span.issue-flag { "Archived" } }
                    @if item.manual_severity != "none" {
                        span.issue-flag data-severity=[(!item.finding_closed).then_some(&item.manual_severity)] {
                            (label(&item.manual_severity)) " finding" @if item.finding_closed { " · closed" }
                        }
                    }
                    @if !item.labels.is_empty() {
                        span.issue-row-labels {
                            @for tag in &item.labels { span { (tag) } }
                        }
                    }
                }
                span.issue-kind { (label(&item.kind)) }
                span.issue-priority title=(label(&item.priority)) {
                    (priority_mark(&item.priority)) span { (label(&item.priority)) }
                }
                time.issue-date datetime=(item.updated_at.to_rfc3339()) title=(item.updated_at.format("%Y-%m-%d %H:%M UTC")) {
                    (item.updated_at.format("%b %e"))
                }
            }
        }
    }
}

fn issue_results(auth: &Auth, f: &Filters, items: &[ListItem]) -> Markup {
    let priority = f.group == "priority";
    let groups = if priority {
        PRIORITY_GROUPS
    } else {
        STATUS_GROUPS
    };
    let belongs = |item: &ListItem, group: &str| {
        if priority {
            item.priority == group
        } else {
            item.status == group
        }
    };
    if f.board() {
        return html! {
            div.board.issue-board aria-label="Issue board" tabindex="0" {
                @for &group in groups {
                    @let selected = if priority { &f.priority } else { &f.status };
                    @if selected.is_empty() || selected == group {
                        @let lane: Vec<_> = items.iter().filter(|i| belongs(i, group)).collect();
                        section.issue-lane aria-label=(label(group)) {
                            h2.issue-group-label { (group_heading(&f.group, group, lane.len())) }
                            @for item in &lane {
                                article.issue-board-item data-checkpoint=[item.checkpoint.then_some("current")] {
                                    div.issue-board-meta {
                                        span.issue-ref title=(&item.project_name) { (&item.project_key) "-" (item.number) }
                                        span.issue-priority title=(label(&item.priority)) {
                                            (priority_mark(&item.priority)) span { (label(&item.priority)) }
                                        }
                                    }
                                    h3 { a href=(format!("/issues/{}",item.id)) { (&item.title) } }
                                    (issue_tags(item))
                                    @if !item.archived && !item.project_archived {
                                        details.issue-move {
                                            summary title="Change status" {
                                                span { (label(&item.kind)) }
                                                span.issue-move-label { span.sr-only { "Change status · " }
                                                    (status_mark(&item.status)) (label(&item.status)) (ledger_icon("chevron"))
                                                }
                                            }
                                            (status_form(auth,item.id,item.revision,&item.status))
                                        }
                                    } @else {
                                        p.issue-board-kind { (label(&item.kind)) " · " (label(&item.status)) }
                                    }
                                }
                            }
                            @if lane.is_empty() { p.issue-lane-empty { "No issues on this page" } }
                        }
                    }
                }
            }
        };
    }
    html! {
        section.issue-ledger data-group=(&f.group) aria-label="Issue list" {
            div.issue-column-head aria-hidden="true" {
                span { "ID" } span { "Issue" } span.issue-kind { "Type" } span.issue-priority { "Priority" } span.issue-date { "Updated" }
            }
            @if f.group == "none" {
                ul.issue-rows { @for item in items { (issue_row(item)) } }
            } @else {
                @for &group in groups {
                    @let rows: Vec<_> = items.iter().filter(|i| belongs(i, group)).collect();
                    @if !rows.is_empty() {
                        details.issue-group open {
                            summary.issue-group-label { (ledger_icon("chevron")) (group_heading(&f.group, group, rows.len())) }
                            ul.issue-rows { @for item in rows { (issue_row(item)) } }
                        }
                    }
                }
            }
        }
    }
}

fn filter_bar(f: &Filters, projects: &[Project], selected: Option<Uuid>) -> Markup {
    let count = f.filter_count();
    html! {
        form.issue-controls method="get" action="/issues" role="search" id="search" {
            (hidden("view", if f.board() { "board" } else { "list" }))
            nav.issue-view-tabs aria-label="Issue layout" {
                @for (key, name) in [("list", "List"), ("board", "Board")] {
                    a href=(f.view_url(key)) aria-current=[((key == "board") == f.board()).then_some("page")] {
                        (ledger_icon(key)) (name)
                    }
                }
            }
            div.issue-query {
                label { span.sr-only { "Search issues" } (ui::icon("search"))
                    input type="search" name="q" maxlength="250" placeholder="Search issues…"
                        aria-describedby="issue-search-help" value=(&f.q);
                }
                button type="submit" aria-label="Submit search" title="Submit search" { (ui::icon("corner")) }
            }
            details.issue-menu name="issue-menu" {
                summary { (ledger_icon("filter")) "Filter" @if count > 0 { span.issue-menu-count { (count) } } }
                div.issue-menu-panel {
                    h2 { "Filters" }
                    label { "Project" select name="project" {
                        option value="" { "All projects" } @for p in projects {
                            option value=(p.id) selected[Some(p.id) == selected] {
                                (&p.name) @if p.archived { " (archived)" }
                            }
                        }
                    } }
                    (optional_filter("status", "Status", STATUSES, &f.status))
                    (optional_filter("priority", "Priority", PRIORITIES, &f.priority))
                    (optional_filter("kind", "Type", KINDS, &f.kind))
                    label { "Scope" select name="scope" {
                        option value="active" selected[f.scope == "active"] { "Active" }
                        option value="archived" selected[f.scope == "archived"] { "Archived" }
                        option value="all" selected[f.scope == "all"] { "Include archived" }
                    } }
                    label.issue-critical { input type="checkbox" name="critical" value="open" checked[f.critical == "open"]; "Open manual criticals" }
                    p id="issue-search-help" { "For long descriptions, search an exact phrase." }
                    div.issue-menu-actions {
                        button.issue-save-option type="submit" formaction="/workflows#views" name="save" value="1" { "Save this view" }
                        button type="submit" { "Apply filters" }
                    }
                }
            }
            details.issue-menu name="issue-menu" {
                summary { (ui::icon("settings")) "Display" }
                div.issue-menu-panel {
                    h2 { "Display options" }
                    label { "Group by" select name="group" {
                        @for &(key, name) in VIEW_GROUPS {
                            @if key != "none" || !f.board() {
                                option value=(key) selected[f.group == key] { (name) }
                            }
                        }
                    } }
                    label { "Sort by" select name="sort" {
                        @for &(key, name) in VIEW_SORTS { option value=(key) selected[f.sort == key] { (name) } }
                    } }
                    div.issue-menu-actions { button.issue-save-option type="submit" formaction="/workflows#views" name="save" value="1" { "Save this view" } button type="submit" { "Apply display" } }
                }
            }
        }
    }
}

fn page_button(f: &Filters, page: u32, title: &str) -> Markup {
    html! { form action="/issues" method="get" {
        @for (name, value) in f.parameters() { (hidden(name, value)) }
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

fn new_issue(
    auth: &Auth,
    projects: &[Project],
    selected: Option<Uuid>,
    initial_status: &str,
) -> Markup {
    html! { details.issue-compose {
        summary { (ui::icon("plus")) "New issue" }
        form.stack id="new-issue" action="/issues" method="post" {
            (command(auth,None))
            label { "Project" select name="project" required { @for p in projects.iter().filter(|p| !p.archived) {
                option value=(p.id) selected[Some(p.id) == selected] { (&p.name) }
            } } }
            label { "Title" input name="title" maxlength="240" required; }
            label { "Description" textarea name="body" rows="3" {} }
            label { "Next action" textarea name="next_action" rows="2" maxlength="2000" aria-describedby="new-action-hint" {} }
            small.muted id="new-action-hint" { "Adding a next action to an open issue saves a checkpoint on Home." }
            details { summary { "Issue settings" } (settings_fields(None,initial_status)) }
            div.actions { button.primary type="submit" { "Create issue" } }
        }
    } }
}

fn settings_fields(issue: Option<&Issue>, initial_status: &str) -> Markup {
    html! { div.grid {
        label { "Status" select name="status" { (options(STATUSES,issue.map_or(initial_status,|i| &i.status))) } }
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
    #[serde(default)]
    edit: bool,
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
            article.issue-detail aria-label=(&issue.title) {
                header.issue-detail-head {
                    nav.breadcrumbs aria-label="Breadcrumb" {
                        a href=(format!("/projects/{}", project.id)) { (&project.name) }
                        span aria-hidden="true" { "/" }
                        a href=(format!("/issues?project={}", project.id)) { "Issues" }
                        span aria-hidden="true" { "/" }
                        code aria-current="page" { (&key) }
                    }
                    h1 { (&issue.title) }
                    @if can_write { (edit_issue(&auth, &issue, q.edit)) }
                    @if project.archived || issue.archived {
                        p.notice { @if project.archived { "This project is archived." } @else { "This issue is archived." } }
                    }
                }

                div.issue-detail-grid {
                    div.issue-reading {
                        section.issue-description aria-labelledby="description-title" {
                            h2 id="description-title" { "Description" }
                            @if issue.body.is_empty() { p.muted { "No description." } }
                            @else { div.prose { (&issue.body) } }
                        }
                        section.issue-checkpoint aria-labelledby="next-action-title" {
                            h2 id="next-action-title" { "Next action" }
                            @if issue.next_action.is_empty() { p.muted { "No next action saved." } }
                            @else { p.prose { (&issue.next_action) } }
                            @if let Some(t) = issue.checkpoint_at { p.checkpoint-stamp { "Checkpoint saved " (timestamp(t)) } }
                            @if can_write && open {
                                details.issue-disclosure {
                                    summary { @if issue.next_action.is_empty() { "Add next action" } @else { "Edit next action" } }
                                    form.stack method="post" action=(format!("/issues/{id}/checkpoint")) {
                                        (command(&auth, Some(issue.revision)))
                                        label { "Checkpoint next action" textarea name="next_action" required maxlength="2000" rows="2" { (&issue.next_action) } }
                                        div.actions { button.primary type="submit" { "Save checkpoint" } }
                                    }
                                }
                            }
                        }
                    }

                    aside.issue-inspector aria-labelledby="properties-title" {
                        section {
                            h2 id="properties-title" { "Properties" }
                            dl.issue-props {
                                div {
                                    dt { "Status" }
                                    dd {
                                        @if can_write {
                                            details.issue-status-picker {
                                                summary title="Change status" {
                                                    (status_mark(&issue.status)) (label(&issue.status))
                                                }
                                                (status_form(&auth, id, issue.revision, &issue.status))
                                            }
                                        } @else {
                                            span.property-value { (status_mark(&issue.status)) (label(&issue.status)) }
                                        }
                                    }
                                }
                                div { dt { "Priority" } dd.property-value { (priority_mark(&issue.priority)) (label(&issue.priority)) } }
                                div { dt { "Type" } dd { (label(&issue.kind)) } }
                                @if !issue.labels.is_empty() {
                                    div { dt { "Labels" } dd.issue-labels { @for tag in &issue.labels { span.tag { (tag) } } } }
                                }
                            }
                        }
                        @if issue.manual_severity != "none" {
                            section.issue-finding {
                                h2 { "Manual security finding" }
                                p { (label(&issue.manual_severity)) " · " @if issue.finding_closed { "Closed" } @else { "Open" } }
                                @if can_write { a href=(format!("/issues/{id}?edit=true#finding-settings")) { "Edit finding" } }
                            }
                        }
                        section {
                            h2 { "Milestones" }
                            @if milestones.is_empty() { p.muted { "No linked milestones." } }
                            @else { ul { @for (milestone, title) in &milestones { li {
                                a href=(format!("/roadmap?project={}&milestone={milestone}#milestone-detail", project.id)) { (title) }
                            } } } }
                            a.inspector-link href=(format!("/roadmap?project={}", project.id)) { "Open project roadmap" (ui::icon("arrow")) }
                        }
                        section {
                            h2 { "Releases" }
                            @if !release_scope.is_empty() { h3 { "In scope" } (release_links(&release_scope)) }
                            @if !shipments.is_empty() { h3 { "Recorded shipments" } (release_links(&shipments)) }
                            @if release_scope.is_empty() && shipments.is_empty() { p.muted { "No linked releases." } }
                            a.inspector-link href=(format!("/releases?project={}", project.id)) { "Open project releases" (ui::icon("arrow")) }
                        }
                        @if let Some(o) = origin {
                            section {
                                h2 { "Linear import" }
                                p { "Source issue " (o.source_id) }
                                @if let (Some(parent), Some(number), Some(title)) = (o.parent_id, o.number, o.title) {
                                    p { "Parent " a href=(format!("/issues/{parent}")) { (&project.key) "-" (number) " · " (title) } }
                                } @else if let Some(source) = o.parent_source { p.muted { "Parent “" (source) "” was not found in this project." } }
                            }
                        }
                        @if !project.archived {
                            details.issue-disclosure.issue-archive {
                                summary { @if issue.archived { "Restore issue" } @else { "Archive issue" } }
                                form method="post" action=(format!("/issues/{id}/archive")) {
                                    (command(&auth, Some(issue.revision))) (hidden("archived", !issue.archived))
                                    button.secondary type="submit" { @if issue.archived { "Restore issue" } @else { "Archive issue" } }
                                }
                            }
                        }
                    }

                    section.issue-activity id="history" aria-labelledby="history-title" {
                        h2 id="history-title" { "Issue history" }
                        @if history.is_empty() { p.muted { "No recorded events on this page." } }
                        @else {
                            ol.timeline { @for event in &history {
                                li {
                                    div {
                                        strong { (event_title(&event.action)) }
                                        @if event.old_status.as_deref() != Some(&event.new_status) {
                                            span.history-change { @if let Some(old) = &event.old_status { (label(old)) " → " } (label(&event.new_status)) }
                                        }
                                    }
                                    (timestamp(event.occurred_at))
                                }
                            } }
                        }
                        @if can_undo { form method="post" action=(format!("/issues/{id}/undo")) {
                            (command(&auth, Some(issue.revision))) button.secondary type="submit" { "Undo latest change" }
                        } }
                        @if older || history_page > 1 { nav.actions aria-label="History pages" {
                            @if history_page > 1 { a href=(format!("/issues/{id}?history_page={}#history", history_page - 1)) { "Newer events" } }
                            @if older { a href=(format!("/issues/{id}?history_page={}#history", history_page.saturating_add(1))) { "Older events" } }
                        } }
                    }

                    div.issue-connections {
                        section id="blockers" aria-labelledby="dependencies-title" {
                            h2 id="dependencies-title" { "Dependencies" }
                            div.issue-dependencies {
                                div { h3 { "Blocked by" } (peers(&project.key, &blockers, &auth, &issue, can_write)) }
                                div { h3 { "Blocks" } (peers(&project.key, &dependents, &auth, &issue, false)) }
                            }
                            @if can_write {
                                details.issue-disclosure open[!q.blocker_q.is_empty()] {
                                    summary { "Add a blocker" }
                                    form.actions method="get" action=(format!("/issues/{id}#blockers")) {
                                        label { "Find a project issue" input type="search" name="blocker_q" value=(&q.blocker_q) maxlength="250" placeholder="Title or issue key"; }
                                        button type="submit" { "Find" }
                                    }
                                    @if candidates.is_empty() { p.muted { "No matching issues available." } }
                                    @else {
                                        form.actions method="post" action=(format!("/issues/{id}/blocker")) {
                                            (command(&auth, Some(issue.revision))) (hidden("operation", "add"))
                                            label { "Blocker" select name="blocker_id" required { @for c in &candidates {
                                                option value=(c.id) { (&project.key) "-" (c.number) " · " (&c.title) }
                                            } } }
                                            button type="submit" { "Add blocker" }
                                        }
                                    }
                                }
                            }
                        }
                        div id="linked-notes" { (notes) }
                        div id="pull-requests" { (github) }
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

fn edit_issue(auth: &Auth, i: &Issue, expanded: bool) -> Markup {
    html! { details.issue-editor id="edit-issue" open[expanded] {
        summary {
            (ui::icon("edit"))
            span.editor-closed-label { "Edit issue" }
            span.editor-open-label { "Close editor" }
        }
        form.stack method="post" action=(format!("/issues/{}/edit",i.id)) { (command(auth,Some(i.revision)))
            label { "Title" input name="title" maxlength="240" value=(&i.title) required; }
            label { "Description" textarea name="body" rows="5" { (&i.body) } }
            label { "Next action" textarea name="next_action" rows="2" maxlength="2000" { (&i.next_action) } }
            div id="finding-settings" { (settings_fields(Some(i), "ready")) }
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
        let view = filters.view_url("list");
        assert!(view.contains("q=Widget+%3Cdraft%3E"));
        assert!(view.contains("critical=open"));
        assert!(view.contains("view=list"));
        let toolbar = filter_bar(&filters, &[], None).into_string();
        assert!(toolbar.contains("formaction=\"/workflows#views\" name=\"save\" value=\"1\""));
        let invalid = Filters {
            status: "unlisted".into(),
            ..Default::default()
        };
        assert!(invalid.check().is_err());
    }
}
