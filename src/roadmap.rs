mod calendar;
mod health;
mod inspector;
mod timeline;
mod writes;

#[cfg(test)]
mod db_tests;

use axum::{
    Router,
    extract::{Query, State},
    http::header::CONTENT_TYPE,
    response::Html,
    routing::{get, post},
};
use chrono::NaiveDate;
use maud::html;
use serde::Deserialize;
use sqlx::FromRow;
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, store, ui::section_page};
use writes::*;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/roadmap", get(roadmap))
        .route("/health", get(health::health))
        .route(
            "/roadmap/timeline.css",
            get(|| async {
                (
                    [(CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("roadmap/timeline.css"),
                )
            }),
        )
        .route("/roadmap/{project}/milestones", post(create))
        .route(
            "/roadmap/{project}/availability",
            post(calendar::save_availability),
        )
        .route(
            "/roadmap/{project}/releases/{id}/span",
            post(calendar::save_release_span),
        )
        .route("/roadmap/{project}/milestones/{id}", post(update))
        .route(
            "/roadmap/{project}/milestones/{id}/dependencies",
            post(save_dependency),
        )
        .route(
            "/roadmap/{project}/milestones/{id}/issues",
            post(save_issue),
        )
        .route(
            "/roadmap/{project}/milestones/{id}/release",
            post(save_release),
        )
        .route("/roadmap/{project}/milestones/{id}/delete", post(delete))
}

#[derive(Clone, Debug, FromRow)]
struct Milestone {
    id: Uuid,
    title: String,
    details: String,
    status: String,
    start_on: Option<NaiveDate>,
    end_on: Option<NaiveDate>,
    target_on: Option<NaiveDate>,
    revision: i64,
}

#[derive(Clone, Debug, FromRow)]
struct Dependency {
    prerequisite_id: Uuid,
    dependent_id: Uuid,
}

#[derive(Debug, FromRow)]
struct IssueChoice {
    id: Uuid,
    number: i64,
    title: String,
    status: String,
    archived: bool,
}

#[derive(Default, Deserialize)]
struct RoadmapQuery {
    project: Option<Uuid>,
    milestone: Option<Uuid>,
    #[serde(default)]
    new: bool,
    #[serde(default)]
    issue_q: String,
}

async fn roadmap(
    State(state): State<AppState>,
    auth: Auth,
    Query(query): Query<RoadmapQuery>,
) -> AppResult<Html<String>> {
    if query.issue_q.contains('\0') || query.issue_q.chars().count() > 500 {
        return Err(AppError::BadRequest(
            "Search issues with up to 500 text characters.".into(),
        ));
    }
    let owner = auth.user_id;
    let projects = store::projects(&state.db, owner).await?;
    let project = match query.project {
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
    let Some(project) = project else {
        return Ok(section_page(
            &auth,
            "Roadmap",
            "roadmap",
            html! {
                header.view-head { h1 { "Roadmap" } }
                section.empty-state {
                    h2 { "Plan your first milestone" }
                    p { "Create a project to set target dates and connect the work that depends on them." }
                    a.action-primary href="/projects" { "Create a project" }
                }
            },
            None,
        ));
    };

    let milestones: Vec<Milestone> = sqlx::query_as(
        "SELECT id,title,details,status,start_on,end_on,target_on,revision
         FROM milestones WHERE owner_id=$1 AND project_id=$2
         ORDER BY coalesce(start_on,target_on) NULLS LAST, title, id",
    )
    .bind(owner)
    .bind(project.id)
    .fetch_all(&state.db)
    .await?;
    let deps: Vec<Dependency> = sqlx::query_as(
        "SELECT prerequisite_id,dependent_id FROM milestone_dependencies
         WHERE owner_id=$1 AND project_id=$2 ORDER BY prerequisite_id,dependent_id",
    )
    .bind(owner)
    .bind(project.id)
    .fetch_all(&state.db)
    .await?;
    let (weeks, releases) = calendar::load_calendar(&state.db, owner, project.id).await?;
    let (today, _) = workspace_clock(&state.db, owner).await?;
    let selected = match query.milestone {
        Some(id) => Some(
            milestones
                .iter()
                .find(|m| m.id == id)
                .ok_or(AppError::NotFound)?,
        ),
        None => None,
    };

    let detail = if let Some(milestone) = selected {
        let (issues, candidates, release_id) =
            picker_data(&state, owner, project, milestone.id, &query.issue_q).await?;
        inspector::inspector(
            &auth,
            inspector::Selection {
                project,
                milestone,
                milestones: &milestones,
                dependencies: &deps,
                issues: &issues,
                candidates: &candidates,
                search: query.issue_q.trim(),
                releases: &releases,
                release_id,
            },
        )
    } else {
        html! {}
    };

    Ok(section_page(
        &auth,
        "Roadmap",
        "roadmap",
        html! {
            link rel="stylesheet" href="/roadmap/timeline.css";
            header.view-head {
                div {
                    h1 { "Roadmap" }
                    p.muted { (project.name) " · " (milestones.len()) " " @if milestones.len() == 1 { "milestone" } @else { "milestones" } }
                }
                div.view-actions {
                    a.action-quiet href=(format!("/health?project={}",project.id)) { (crate::ui::icon("health")) "Project health" }
                    @if !project.archived && !milestones.is_empty() && !query.new {
                        a.action-primary href=(format!("/roadmap?project={}&new=true#new-milestone",project.id)) { (crate::ui::icon("plus")) "New milestone" }
                    }
                }
            }
            @if projects.len() > 1 {
                form.view-tools method="get" action="/roadmap" {
                    label.field { "Project" select name="project" {
                        @for p in &projects {
                            option value=(p.id) selected[p.id == project.id] { (p.name) @if p.archived { " (archived)" } }
                        }
                    } }
                    button.secondary type="submit" { "Switch project" }
                }
            }
            @if project.archived { p.status-note { "Archived project · Read-only" } }
            @if query.new && !project.archived { (inspector::creation(&auth,project.id,true)) }
            div.rm-workbench {
                div class=(if selected.is_some() { "rm-work rm-has-selection" } else { "rm-work" }) {
                    (detail)
                    div.rm-schedule-column {
                        (timeline::timeline(project.id,&milestones,&deps,selected.map(|m|m.id),today,&weeks,&releases))
                        @if !project.archived && !query.new && milestones.is_empty() { (inspector::creation(&auth,project.id, true)) }
                        section.rm-settings aria-label="Calendar settings" {
                            (calendar::release_editor(&auth,project.id,project.archived,&releases))
                            (calendar::calendar_editor(&auth,project.id,project.archived,&weeks))
                        }
                    }
                }
            }
        },
        Some(project.id),
    ))
}

async fn workspace_clock(db: &sqlx::PgPool, owner: Uuid) -> AppResult<(NaiveDate, String)> {
    sqlx::query_as("SELECT (now() AT TIME ZONE timezone)::date, timezone FROM users WHERE id=$1")
        .bind(owner)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

async fn picker_data(
    state: &AppState,
    owner: Uuid,
    project: &store::Project,
    milestone: Uuid,
    search: &str,
) -> AppResult<(Vec<IssueChoice>, Vec<IssueChoice>, Option<Uuid>)> {
    let issues = sqlx::query_as(
        "SELECT i.id,i.number,i.title,i.status,i.archived FROM issues i
         JOIN milestone_issues mi ON (mi.owner_id,mi.project_id,mi.issue_id)=(i.owner_id,i.project_id,i.id)
         WHERE mi.owner_id=$1 AND mi.project_id=$2 AND mi.milestone_id=$3 ORDER BY i.number"
    ).bind(owner).bind(project.id).bind(milestone).fetch_all(&state.db).await?;
    let release_id = sqlx::query_scalar(
        "SELECT release_id FROM milestone_releases WHERE owner_id=$1 AND project_id=$2 AND milestone_id=$3"
    ).bind(owner).bind(project.id).bind(milestone).fetch_optional(&state.db).await?;
    let candidates = if project.archived {
        vec![]
    } else {
        sqlx::query_as(
            "SELECT i.id,i.number,i.title,i.status,i.archived FROM issues i
             WHERE i.owner_id=$1 AND i.project_id=$2 AND NOT i.archived
               AND position(lower($4) in lower($5 || '-' || i.number::text || ' ' || i.title)) > 0
               AND NOT EXISTS (SELECT 1 FROM milestone_issues mi WHERE
                 (mi.owner_id,mi.project_id,mi.issue_id)=(i.owner_id,i.project_id,i.id)
                 AND mi.milestone_id=$3)
             ORDER BY i.updated_at DESC,i.number DESC LIMIT 50",
        )
        .bind(owner)
        .bind(project.id)
        .bind(milestone)
        .bind(search.trim())
        .bind(&project.key)
        .fetch_all(&state.db)
        .await?
    };
    Ok((issues, candidates, release_id))
}
