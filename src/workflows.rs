use axum::{
    Router,
    extract::{Form, Path, Query, State},
    response::{Html, Redirect},
    routing::{get, post},
};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{
    AppError, AppResult, AppState,
    auth::Auth,
    issues::{NewIssue, create_in},
    store::{self, Project},
    ui::{csrf, page},
};

mod digest;
mod digest_email;
pub use digest::run_due_digests;
pub use digest_email::deliver_digest_emails;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/workflows", get(index))
        .route("/workflows/views", post(create_view))
        .route("/workflows/views/{id}", get(open_view))
        .route("/workflows/views/{id}/delete", post(delete_view))
        .route("/workflows/templates", post(create_template))
        .route("/workflows/templates/{id}", get(show_template))
        .route("/workflows/templates/{id}/apply", post(apply_template))
        .route("/workflows/templates/{id}/delete", post(delete_template))
        .route("/workflows/digest/{id}", get(digest::show))
        .route(
            "/workflows/digest",
            get(digest::history).post(digest::schedule),
        )
}

const STATUSES: &[(&str, &str)] = &[
    ("backlog", "Backlog"),
    ("ready", "Ready"),
    ("in_progress", "In progress"),
    ("done", "Done"),
    ("canceled", "Canceled"),
];
const PRIORITIES: &[(&str, &str)] = &[
    ("low", "Low"),
    ("normal", "Normal"),
    ("high", "High"),
    ("urgent", "Urgent"),
];
const KINDS: &[(&str, &str)] = &[
    ("feature", "Feature"),
    ("bug", "Bug"),
    ("maintenance", "Maintenance"),
    ("security", "Security"),
];
const CRITICAL: &[(&str, &str)] = &[("open", "Open critical findings")];
const SCOPES: &[(&str, &str)] = &[
    ("active", "Active issues"),
    ("archived", "Archived issues"),
    ("all", "All issues"),
];
const LAYOUTS: &[(&str, &str)] = &[("list", "List"), ("board", "Board")];
pub(crate) const VIEW_SORTS: &[(&str, &str)] = &[
    ("updated", "Recently updated"),
    ("priority", "Highest priority"),
    ("newest", "Newest created"),
    ("title", "Title A–Z"),
];
pub(crate) const VIEW_GROUPS: &[(&str, &str)] = &[
    ("status", "Status"),
    ("priority", "Priority"),
    ("none", "None"),
];
const PAGE_SIZE: i64 = 20;

fn paid(plan: &str) -> bool {
    matches!(plan, "plus" | "max")
}

fn authorize_paid(plan: &str) -> AppResult<()> {
    if paid(plan) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

fn authorize_view(plan: &str, project: Option<Uuid>) -> AppResult<()> {
    authorize_paid(plan)?;
    if project.is_none() && plan != "max" {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

async fn plan_for_update(tx: &mut Transaction<'_, Postgres>, owner: Uuid) -> AppResult<String> {
    sqlx::query_scalar("SELECT plan FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)
}

async fn current_plan(db: &PgPool, owner: Uuid) -> AppResult<String> {
    sqlx::query_scalar("SELECT plan FROM users WHERE id=$1")
        .bind(owner)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

fn valid_option(value: &str, choices: &[(&str, &str)], optional: bool) -> bool {
    (optional && value.is_empty()) || choices.iter().any(|(key, _)| *key == value)
}

fn validate_name(name: &str) -> AppResult<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "Enter a name between 1 and 100 characters.".into(),
        ));
    }
    Ok(name)
}

fn validate_issue_options(
    status: &str,
    priority: &str,
    kind: &str,
    optional: bool,
) -> AppResult<()> {
    if valid_option(status, STATUSES, optional)
        && valid_option(priority, PRIORITIES, optional)
        && valid_option(kind, KINDS, optional)
    {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "Choose a listed status, priority and issue type.".into(),
        ))
    }
}

#[derive(Deserialize, FromRow)]
struct Filters {
    #[serde(default)]
    status: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    critical: String,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default = "default_layout", alias = "view")]
    layout: String,
    #[serde(default = "default_sort", rename = "sort")]
    sort_order: String,
    #[serde(default = "default_group", rename = "group")]
    grouping: String,
    #[serde(default, alias = "q")]
    search: String,
}
fn default_scope() -> String {
    "active".into()
}
fn default_layout() -> String {
    "list".into()
}
fn default_sort() -> String {
    "updated".into()
}
fn default_group() -> String {
    "status".into()
}

impl Filters {
    fn normalize_display(&mut self) {
        if self.layout == "board" && self.grouping == "none" {
            self.grouping = default_group();
        }
    }

    fn validate(&self) -> AppResult<()> {
        validate_issue_options(&self.status, &self.priority, &self.kind, true)?;
        if !valid_option(&self.critical, CRITICAL, true)
            || !valid_option(&self.scope, SCOPES, false)
            || !valid_option(&self.layout, LAYOUTS, false)
            || !valid_option(&self.sort_order, VIEW_SORTS, false)
            || !valid_option(&self.grouping, VIEW_GROUPS, false)
        {
            return Err(AppError::BadRequest(
                "Choose a listed issue scope, layout, sort order and grouping.".into(),
            ));
        }
        if self.search.chars().count() > 250 || self.search.chars().any(char::is_control) {
            return Err(AppError::BadRequest(
                "Search text must be one line of up to 250 characters.".into(),
            ));
        }
        Ok(())
    }

    fn issue_url(&self, project: Option<Uuid>) -> String {
        let mut pairs = url::form_urlencoded::Serializer::new(String::new());
        if let Some(id) = project {
            pairs.append_pair("project", &id.to_string());
        }
        for (key, value) in [
            ("status", &self.status),
            ("priority", &self.priority),
            ("kind", &self.kind),
            ("critical", &self.critical),
            ("scope", &self.scope),
            ("view", &self.layout),
            ("sort", &self.sort_order),
            ("group", &self.grouping),
            ("q", &self.search),
        ] {
            if !value.is_empty() {
                pairs.append_pair(key, value);
            }
        }
        format!("/issues?{}", pairs.finish())
    }
}

#[derive(Deserialize)]
struct SaveView {
    csrf: String,
    name: String,
    #[serde(default)]
    project: String,
    #[serde(flatten)]
    filters: Filters,
}

async fn create_view(
    State(s): State<AppState>,
    auth: Auth,
    Form(mut f): Form<SaveView>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let project = match f.project.as_str() {
        "" => None,
        value => Some(
            Uuid::parse_str(value)
                .map_err(|_| AppError::BadRequest("Select a valid project.".into()))?,
        ),
    };
    let mut tx = s.db.begin().await?;
    authorize_view(&plan_for_update(&mut tx, auth.user_id).await?, project)?;
    let name = validate_name(&f.name)?;
    f.filters.normalize_display();
    f.filters.validate()?;
    if let Some(project_id) = project {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects WHERE owner_id=$1 AND id=$2)")
                .bind(auth.user_id)
                .bind(project_id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(AppError::NotFound);
        }
    }
    let search = f.filters.search.trim();
    store::check_storage(&mut tx, auth.user_id, (name.len() + search.len()) as i64).await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO workflow_views(owner_id,name,project_id,status,priority,kind,scope,layout,search,critical,sort_order,grouping)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT DO NOTHING RETURNING id",
    ).bind(auth.user_id).bind(name).bind(project)
        .bind(&f.filters.status).bind(&f.filters.priority).bind(&f.filters.kind)
        .bind(&f.filters.scope).bind(&f.filters.layout).bind(search).bind(&f.filters.critical)
        .bind(&f.filters.sort_order).bind(&f.filters.grouping)
        .fetch_optional(&mut *tx).await?;
    id.ok_or_else(|| AppError::Conflict("You already have a view with that name.".into()))?;
    tx.commit().await?;
    Ok(Redirect::to("/workflows#views"))
}

#[derive(FromRow)]
struct SavedView {
    id: Uuid,
    name: String,
    project_id: Option<Uuid>,
    #[sqlx(flatten)]
    filters: Filters,
}

async fn open_view(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> AppResult<Redirect> {
    let mut tx = s.db.begin().await?;
    let plan = plan_for_update(&mut tx, auth.user_id).await?;
    let v: SavedView = sqlx::query_as("SELECT * FROM workflow_views WHERE owner_id=$1 AND id=$2")
        .bind(auth.user_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    authorize_view(&plan, v.project_id)?;
    tx.commit().await?;
    Ok(Redirect::to(&v.filters.issue_url(v.project_id)))
}

#[derive(Deserialize, FromRow)]
struct TemplateDraft {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    next_action: String,
    status: String,
    priority: String,
    kind: String,
}

impl TemplateDraft {
    fn into_issue(self, project: Uuid) -> NewIssue {
        NewIssue {
            project,
            title: self.title.trim().to_owned(),
            body: self.body,
            next_action: self.next_action.trim().to_owned(),
            kind: self.kind,
            priority: self.priority,
            status: self.status,
            labels: String::new(),
            severity: "none".into(),
        }
    }
}

#[derive(Deserialize)]
struct SaveTemplate {
    csrf: String,
    name: String,
    #[serde(flatten)]
    draft: TemplateDraft,
}

#[derive(FromRow)]
struct Template {
    id: Uuid,
    name: String,
    #[sqlx(flatten)]
    draft: TemplateDraft,
}

async fn create_template(
    State(s): State<AppState>,
    auth: Auth,
    Form(f): Form<SaveTemplate>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let mut tx = s.db.begin().await?;
    authorize_paid(&plan_for_update(&mut tx, auth.user_id).await?)?;
    let name = validate_name(&f.name)?;
    // Templates are personal. The owner selects the target project when applying one.
    let draft = f.draft.into_issue(Uuid::nil());
    draft.validate()?;
    let bytes = name.len() + draft.title.len() + draft.body.len() + draft.next_action.len();
    store::check_storage(&mut tx, auth.user_id, bytes as i64).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO issue_templates(owner_id,name,title,body,next_action,status,priority,kind)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(auth.user_id)
    .bind(name)
    .bind(&draft.title)
    .bind(&draft.body)
    .bind(&draft.next_action)
    .bind(&draft.status)
    .bind(&draft.priority)
    .bind(&draft.kind)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::Conflict("You already have a template with that name.".into()))?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/workflows/templates/{id}")))
}

#[derive(Deserialize)]
struct ApplyTemplate {
    csrf: String,
    command_id: Uuid,
    project: Uuid,
}

async fn apply_template(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<ApplyTemplate>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let mut tx = s.db.begin().await?;
    authorize_paid(&plan_for_update(&mut tx, auth.user_id).await?)?;
    let t: Template = sqlx::query_as("SELECT * FROM issue_templates WHERE owner_id=$1 AND id=$2")
        .bind(auth.user_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    let issue_id = create_in(
        &mut tx,
        auth.user_id,
        f.command_id,
        t.draft.into_issue(f.project),
    )
    .await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/issues/{issue_id}")))
}

#[derive(Deserialize)]
struct Confirm {
    csrf: String,
}

async fn delete_item(
    db: &PgPool,
    auth: &Auth,
    id: Uuid,
    form: &Confirm,
    statement: &str,
    location: &str,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let mut tx = db.begin().await?;
    plan_for_update(&mut tx, auth.user_id).await?;
    if sqlx::query(statement)
        .bind(auth.user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Err(AppError::NotFound);
    }
    tx.commit().await?;
    Ok(Redirect::to(location))
}

async fn delete_view(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<Confirm>,
) -> AppResult<Redirect> {
    delete_item(
        &s.db,
        &auth,
        id,
        &f,
        "DELETE FROM workflow_views WHERE owner_id=$1 AND id=$2",
        "/workflows#views",
    )
    .await
}

async fn delete_template(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<Confirm>,
) -> AppResult<Redirect> {
    delete_item(
        &s.db,
        &auth,
        id,
        &f,
        "DELETE FROM issue_templates WHERE owner_id=$1 AND id=$2",
        "/workflows#templates",
    )
    .await
}

#[derive(Deserialize)]
struct Pages {
    #[serde(default)]
    views: u32,
    #[serde(default)]
    templates: u32,
    save: Option<u8>,
    #[serde(default)]
    project: String,
    #[serde(flatten)]
    filters: Filters,
}

#[derive(FromRow)]
struct TemplateSummary {
    id: Uuid,
    name: String,
    title: String,
}

async fn index(
    State(s): State<AppState>,
    auth: Auth,
    Query(mut pages): Query<Pages>,
) -> AppResult<Html<String>> {
    let plan = current_plan(&s.db, auth.user_id).await?;
    let projects = store::projects(&s.db, auth.user_id).await?;
    let selected_project = if pages.project.is_empty() {
        None
    } else {
        let id: Uuid = pages
            .project
            .parse()
            .map_err(|_| AppError::BadRequest("Select a valid project.".into()))?;
        if !projects.iter().any(|p| p.id == id) {
            return Err(AppError::NotFound);
        }
        Some(id)
    };
    if pages.filters.layout.is_empty() {
        pages.filters.layout = default_layout();
    }
    if pages.filters.scope.is_empty() {
        pages.filters.scope = if pages.filters.critical == "open" {
            "all".into()
        } else {
            default_scope()
        };
    }
    pages.filters.normalize_display();
    pages.filters.validate()?;
    let mut views: Vec<SavedView> = sqlx::query_as(
        "SELECT * FROM workflow_views WHERE owner_id=$1 ORDER BY lower(name), id LIMIT $2 OFFSET $3",
    ).bind(auth.user_id).bind(PAGE_SIZE+1).bind(i64::from(pages.views)*PAGE_SIZE).fetch_all(&s.db).await?;
    let more_views = views.len() as i64 > PAGE_SIZE;
    views.truncate(PAGE_SIZE as usize);
    let mut templates: Vec<TemplateSummary> = sqlx::query_as(
        "SELECT id,name,title FROM issue_templates WHERE owner_id=$1 ORDER BY lower(name), id LIMIT $2 OFFSET $3",
    ).bind(auth.user_id).bind(PAGE_SIZE+1).bind(i64::from(pages.templates)*PAGE_SIZE).fetch_all(&s.db).await?;
    let more_templates = templates.len() as i64 > PAGE_SIZE;
    templates.truncate(PAGE_SIZE as usize);
    let digest = digest::section(&s, &auth, &plan).await?;
    Ok(crate::ui::section_page(
        &auth,
        "Workflows",
        "workflows",
        html! {
            div.workflow-workspace {
                header.view-head {
                    h1 { "Workflows" }
                    div.view-actions {
                        span.workflow-plan { (match plan.as_str() { "plus" => "Plus plan", "max" => "Max plan", _ => "Free plan" }) }
                        @if plan != "max" { a.action-secondary href="/billing" { "Compare plans" } }
                    }
                }
                @if views.len() > 5 || templates.len() > 5 {
                    nav.workflow-jumps aria-label="Workflow sections" {
                        a href="#views" { "Saved views" }
                        a href="#templates" { "Issue templates" }
                        a href="#portfolio-digest" { "Weekly digest" }
                    }
                }
                section.workflow-section id="views" aria-labelledby="saved-views-title" {
                    header.workflow-label {
                        h2 id="saved-views-title" { "Saved views" }
                        @if !paid(&plan) { span.workflow-access { "Save with Plus or Max" } }
                    }
                    div.workflow-body {
                        @if !paid(&plan) {
                            p { "Reopen an issue list or board with its filters, sort order and grouping. Max also saves views across projects." }
                            @if pages.save == Some(1) {
                                div.workflow-current {
                                    strong { "Your current view" }
                                    (view_summary(&pages.filters, selected_project, &projects))
                                    a href=(pages.filters.issue_url(selected_project)) { "Return to filtered issues" }
                                }
                            }
                        } @else if views.is_empty() && pages.views == 0 && pages.save != Some(1) {
                            p { "Save your first filtered issue list or board." }
                        }
                        @if paid(&plan) && (!projects.is_empty() || plan == "max") {
                            div.workflow-create {
                                details open[pages.save == Some(1)] {
                                    summary { "Save an issue view" }
                                    form.workflow-form method="post" action="/workflows/views" {
                                        (csrf(&auth))
                                        @if plan == "plus" && pages.save == Some(1) && selected_project.is_none() {
                                            p { "Choose a project for this view. Max can save a view across projects." }
                                        }
                                        div.workflow-fields {
                                            label.field { "View name" input name="name" required maxlength="100"; }
                                            label.field { "Project" select name="project" required[plan != "max"] {
                                                @if plan == "max" { option value="" selected[selected_project.is_none()] { "All projects" } }
                                                @else { option value="" disabled selected[selected_project.is_none()] { "Choose a project" } }
                                                @for project in &projects { (project_option(project,selected_project)) }
                                            } }
                                        }
                                        @if pages.save == Some(1) {
                                            p.workflow-source {
                                                span { "From Issues" }
                                                (view_summary(&pages.filters, selected_project, &projects))
                                            }
                                        }
                                        details.workflow-options open[pages.save != Some(1)] {
                                            summary { "Filters and display" }
                                            fieldset.workflow-fieldset {
                                                legend { "Filters" }
                                                div.workflow-fields {
                                                    label.field.workflow-span { "Search issues" input name="search" maxlength="250" value=(&pages.filters.search); }
                                                    (select_field("Status","status",STATUSES,Some("All statuses"),&pages.filters.status))
                                                    (select_field("Priority","priority",PRIORITIES,Some("All priorities"),&pages.filters.priority))
                                                    (select_field("Issue type","kind",KINDS,Some("All types"),&pages.filters.kind))
                                                    (select_field("Security","critical",CRITICAL,Some("All issues"),&pages.filters.critical))
                                                    (select_field("Include","scope",SCOPES,None,&pages.filters.scope))
                                                }
                                            }
                                            fieldset.workflow-fieldset {
                                                legend { "Display" }
                                                div.workflow-fields {
                                                    (select_field("Layout","layout",LAYOUTS,None,&pages.filters.layout))
                                                    (select_field("Sort by","sort",VIEW_SORTS,None,&pages.filters.sort_order))
                                                    (select_field("Group by","group",VIEW_GROUPS,None,&pages.filters.grouping))
                                                }
                                            }
                                        }
                                        div.view-actions {
                                            button.action-primary type="submit" { "Save view" }
                                            @if pages.save == Some(1) {
                                                a href=(pages.filters.issue_url(selected_project)) { "Return to filtered issues" }
                                            }
                                        }
                                    }
                                }
                            }
                        } @else if plan == "plus" && projects.is_empty() {
                            p { a href="/projects" { "Create a project" } " to save a project view." }
                        }
                        @if !views.is_empty() {
                            ul.workflow-records aria-label="Saved views" {
                                @for v in &views {
                                    li.workflow-record {
                                        @if authorize_view(&plan, v.project_id).is_ok() {
                                            a.workflow-record-main href=(format!("/workflows/views/{}",v.id)) {
                                                strong { (v.name) }
                                                (view_summary(&v.filters, v.project_id, &projects))
                                            }
                                        } @else {
                                            div.workflow-record-main {
                                                span.workflow-record-name {
                                                    strong { (v.name) }
                                                    span.workflow-access { @if v.project_id.is_some() { "Open with Plus or Max" } @else { "Open with Max" } }
                                                }
                                                (view_summary(&v.filters, v.project_id, &projects))
                                            }
                                        }
                                        (delete_button(&auth, "views", v.id, &v.name))
                                    }
                                }
                            }
                        } @else if pages.views > 0 { p { "No saved views on this page." } }
                        (pager(&pages, true, more_views))
                    }
                }
                section.workflow-section id="templates" aria-labelledby="templates-title" {
                    header.workflow-label {
                        h2 id="templates-title" { "Issue templates" }
                        @if !paid(&plan) { span.workflow-access { "Create with Plus or Max" } }
                    }
                    div.workflow-body {
                        @if !paid(&plan) {
                            p { "Start recurring issues with the same description, next action and properties." }
                        } @else if templates.is_empty() && pages.templates == 0 {
                            p { "Create your first template for recurring issues." }
                        }
                        @if paid(&plan) {
                            div.workflow-create {
                                details {
                                    summary { "Create a template" }
                                    form.workflow-form method="post" action="/workflows/templates" {
                                        (csrf(&auth))
                                        label.field { "Template name" input name="name" required maxlength="100"; }
                                        fieldset.workflow-fieldset {
                                            legend { "Issue defaults" }
                                            div.workflow-fields {
                                                label.field.workflow-span { "Issue title" input name="title" required maxlength="240"; }
                                                label.field.workflow-span { "Description" textarea name="body" rows="4" {} }
                                                label.field.workflow-span { "Next action" textarea name="next_action" rows="2" maxlength="2000" {} }
                                                (select_field("Status","status",STATUSES,None,"ready"))
                                                (select_field("Priority","priority",PRIORITIES,None,"normal"))
                                                (select_field("Issue type","kind",KINDS,None,"feature"))
                                            }
                                        }
                                        button.action-primary type="submit" { "Save template" }
                                    }
                                }
                            }
                        }
                        @if !templates.is_empty() {
                            ul.workflow-records aria-label="Issue templates" {
                                @for t in &templates {
                                    li.workflow-record {
                                        a.workflow-record-main href=(format!("/workflows/templates/{}",t.id)) {
                                            strong { (t.name) } span.workflow-summary { (t.title) }
                                        }
                                        (delete_button(&auth, "templates", t.id, &t.name))
                                    }
                                }
                            }
                        } @else if pages.templates > 0 { p { "No issue templates on this page." } }
                        (pager(&pages, false, more_templates))
                    }
                }
                (digest)
            }
        },
        None,
    ))
}

fn view_summary(filters: &Filters, project: Option<Uuid>, projects: &[Project]) -> Markup {
    let scope = project.map_or("All projects", |id| {
        projects
            .iter()
            .find(|p| p.id == id)
            .map_or("Project", |p| p.name.as_str())
    });
    html! {
        span.workflow-summary {
            (scope) " · " (option_label(&filters.layout, LAYOUTS)) " · " (option_label(&filters.scope, SCOPES))
            @for (value, options) in [(&filters.status, STATUSES), (&filters.priority, PRIORITIES),
                                     (&filters.kind, KINDS), (&filters.critical, CRITICAL)] {
                @if !value.is_empty() { " · " (option_label(value, options)) }
            }
            @if !filters.search.is_empty() { " · “" (&filters.search) "”" }
            @if filters.sort_order != "updated" { " · " (option_label(&filters.sort_order, VIEW_SORTS)) }
            @if filters.grouping == "none" { " · Ungrouped" }
            @else if filters.grouping == "priority" { " · Grouped by priority" }
        }
    }
}

fn option_label<'a>(value: &'a str, options: &'a [(&'a str, &'a str)]) -> &'a str {
    options
        .iter()
        .find(|(key, _)| *key == value)
        .map_or(value, |(_, label)| *label)
}

fn select_field(
    label: &str,
    name: &str,
    options: &[(&str, &str)],
    any: Option<&str>,
    selected: &str,
) -> Markup {
    html! {
        label class="field" { (label)
            select name=(name) {
                @if let Some(label) = any { option value="" selected[selected.is_empty()] { (label) } }
                @for (key, text) in options { option value=(*key) selected[*key == selected] { (*text) } }
            }
        }
    }
}

fn project_option(project: &Project, selected: Option<Uuid>) -> Markup {
    html! { option value=(project.id) selected[selected == Some(project.id)] { (project.name) @if project.archived { " (archived)" } } }
}

fn delete_button(auth: &Auth, group: &str, id: Uuid, name: &str) -> Markup {
    let item = if group == "views" { "view" } else { "template" };
    html! {
        details.workflow-delete {
            summary aria-label=(format!("Delete {name}")) { "Delete" }
            form method="post" action=(format!("/workflows/{group}/{id}/delete")) {
                (csrf(auth))
                p { "Delete “" (name) "”?" }
                button.action-secondary.danger type="submit" { "Delete " (item) }
            }
        }
    }
}

fn pager(pages: &Pages, views: bool, more: bool) -> Markup {
    let current = if views { pages.views } else { pages.templates };
    let url = |n| {
        if views {
            format!("/workflows?views={n}&templates={}#views", pages.templates)
        } else {
            format!("/workflows?views={}&templates={n}#templates", pages.views)
        }
    };
    html! {
        @if current > 0 || more {
            nav class="workflow-jumps" aria-label=(if views { "Saved view pages" } else { "Template pages" }) {
                @if current > 0 { a href=(url(current-1)) { "Previous" } }
                @if more { a href=(url(current.saturating_add(1))) { "Next" } }
            }
        }
    }
}

async fn show_template(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> AppResult<Html<String>> {
    let t: Template = sqlx::query_as("SELECT * FROM issue_templates WHERE owner_id=$1 AND id=$2")
        .bind(auth.user_id)
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let plan = current_plan(&s.db, auth.user_id).await?;
    let projects = store::projects(&s.db, auth.user_id).await?;
    Ok(page(
        &auth,
        &t.name,
        "workflows",
        html! {
            div.workflow-workspace {
                a.workflow-back href="/workflows#templates" { "All templates" }
                header.view-head { h1 { (t.name) } }
                section.workflow-section aria-labelledby="template-preview" {
                    header.workflow-label { h2 id="template-preview" { "Issue preview" } }
                    div.workflow-body {
                        h3.workflow-draft-title { (t.draft.title) }
                        dl.workflow-properties {
                            div { dt { "Type" } dd { (option_label(&t.draft.kind,KINDS)) } }
                            div { dt { "Status" } dd { (option_label(&t.draft.status,STATUSES)) } }
                            div { dt { "Priority" } dd { (option_label(&t.draft.priority,PRIORITIES)) } }
                        }
                        @if !t.draft.body.is_empty() { p.prose { (t.draft.body) } }
                        @if !t.draft.next_action.is_empty() {
                            div.workflow-next { h4 { "Next action" } p.prose { (t.draft.next_action) } }
                        }
                    }
                }
                section.workflow-section aria-labelledby="template-apply" {
                    header.workflow-label { h2 id="template-apply" { "Use template" } }
                    div.workflow-body {
                        @if paid(&plan) {
                            @if projects.iter().any(|p| !p.archived) {
                                form.workflow-form method="post" action=(format!("/workflows/templates/{}/apply",t.id)) {
                                    (csrf(&auth))
                                    input type="hidden" name="command_id" value=(Uuid::new_v4());
                                    label.field { "Create issue in" select name="project" required {
                                        @for project in projects.iter().filter(|p| !p.archived) { (project_option(project,None)) }
                                    } }
                                    button.action-primary type="submit" { "Create issue from template" }
                                }
                            } @else { p { a href="/projects" { "Open projects" } " to create or activate a project for this issue." } }
                        } @else {
                            p { "Plus and Max can create issues from this template." }
                            a.action-secondary href="/billing" { "Compare plans" }
                        }
                    }
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests;
