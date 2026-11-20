use crate::{AppError, AppResult, AppState, auth::Auth, store};
use axum::{
    Form,
    extract::{Path, State},
    response::Redirect,
};
use chrono::{Datelike, NaiveDate};
use serde::Deserialize;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

#[derive(Deserialize)]
pub(super) struct MilestoneForm {
    pub(super) csrf: String,
    pub(super) id: Option<Uuid>,
    pub(super) revision: Option<i64>,
    pub(super) title: String,
    #[serde(default)]
    pub(super) details: String,
    pub(super) status: String,
    #[serde(default)]
    pub(super) start_on: String,
    #[serde(default)]
    pub(super) end_on: String,
    #[serde(default)]
    pub(super) target_on: String,
}

struct MilestoneValues<'a> {
    title: &'a str,
    details: &'a str,
    status: &'a str,
    start: Option<NaiveDate>,
    end: Option<NaiveDate>,
    target: Option<NaiveDate>,
}

pub(super) fn calendar_date(value: &str) -> AppResult<Option<NaiveDate>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<NaiveDate>()
        .ok()
        .filter(|d| (1..=9999).contains(&d.year()))
        .map(Some)
        .ok_or_else(|| AppError::BadRequest("Enter a calendar date in YYYY-MM-DD format.".into()))
}

impl MilestoneForm {
    fn values(&self) -> AppResult<MilestoneValues<'_>> {
        let title = self.title.trim();
        if title.is_empty() || title.chars().count() > 200 || title.contains('\0') {
            return Err(AppError::BadRequest(
                "Enter a milestone title of 1–200 characters.".into(),
            ));
        }
        if self.details.chars().count() > 8000 || self.details.contains('\0') {
            return Err(AppError::BadRequest(
                "Milestone notes can contain up to 8,000 text characters.".into(),
            ));
        }
        if !matches!(self.status.as_str(), "planned" | "in_progress" | "reached") {
            return Err(AppError::BadRequest("Choose a milestone status.".into()));
        }
        let start = calendar_date(&self.start_on)?;
        let end = calendar_date(&self.end_on)?;
        if start.is_some() != end.is_some() || start > end {
            return Err(AppError::BadRequest(
                "Enter both span dates, with the end on or after the start.".into(),
            ));
        }
        Ok(MilestoneValues {
            title,
            details: self.details.trim(),
            status: &self.status,
            start,
            end,
            target: calendar_date(&self.target_on)?,
        })
    }
}

fn detail_redirect(project: Uuid, milestone: Uuid) -> Redirect {
    Redirect::to(&format!(
        "/roadmap?project={project}&milestone={milestone}#milestone-detail"
    ))
}

async fn locked_project(
    state: &AppState,
    owner: Uuid,
    project: Uuid,
) -> AppResult<Transaction<'static, Postgres>> {
    let mut tx = state.db.begin().await?;
    store::lock_project(&mut tx, owner, project).await?;
    Ok(tx)
}

async fn milestone_revision(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    project: Uuid,
    id: Uuid,
) -> AppResult<i64> {
    sqlx::query_scalar(
        "SELECT revision FROM milestones WHERE owner_id=$1 AND project_id=$2 AND id=$3",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)
}

async fn touch(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    project: Uuid,
    ids: &[Uuid],
) -> AppResult<()> {
    sqlx::query(
        "UPDATE milestones SET revision=revision+1,updated_at=now()
         WHERE owner_id=$1 AND project_id=$2 AND id=ANY($3)",
    )
    .bind(owner)
    .bind(project)
    .bind(ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn expected_revision(current: i64, expected: Option<i64>) -> AppResult<()> {
    if expected == Some(current) {
        return Ok(());
    }
    Err(AppError::Conflict("This milestone changed since you opened it. Reload the roadmap to review the latest revision.".into()))
}

pub(super) async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Path(project): Path<Uuid>,
    Form(form): Form<MilestoneForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let v = form.values()?;
    let owner = auth.user_id;
    let id = form.id.unwrap_or_else(Uuid::new_v4);
    let mut tx = locked_project(&state, owner, project).await?;
    let result = sqlx::query(
        "INSERT INTO milestones(id,owner_id,project_id,title,details,status,start_on,end_on,target_on)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(id) DO NOTHING"
    ).bind(id).bind(owner).bind(project).bind(v.title).bind(v.details).bind(v.status)
     .bind(v.start).bind(v.end).bind(v.target).execute(&mut *tx).await?;
    if result.rows_affected() == 0 {
        milestone_revision(&mut tx, owner, project, id).await?;
    }
    tx.commit().await?;
    Ok(detail_redirect(project, id))
}

pub(super) async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, id)): Path<(Uuid, Uuid)>,
    Form(form): Form<MilestoneForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let v = form.values()?;
    let owner = auth.user_id;
    let mut tx = locked_project(&state, owner, project).await?;
    expected_revision(
        milestone_revision(&mut tx, owner, project, id).await?,
        form.revision,
    )?;
    sqlx::query(
        "UPDATE milestones SET title=$4,details=$5,status=$6,start_on=$7,end_on=$8,target_on=$9,
         revision=revision+1,updated_at=now() WHERE owner_id=$1 AND project_id=$2 AND id=$3",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .bind(v.title)
    .bind(v.details)
    .bind(v.status)
    .bind(v.start)
    .bind(v.end)
    .bind(v.target)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(detail_redirect(project, id))
}

#[derive(Deserialize)]
pub(super) struct DependencyForm {
    pub(super) csrf: String,
    pub(super) prerequisite_id: Uuid,
    pub(super) operation: String,
}

#[derive(Deserialize)]
pub(super) struct IssueForm {
    pub(super) csrf: String,
    pub(super) issue_id: Uuid,
    pub(super) operation: String,
}

fn is_add(operation: &str) -> AppResult<bool> {
    match operation {
        "add" => Ok(true),
        "remove" => Ok(false),
        _ => Err(AppError::BadRequest("Choose add or remove.".into())),
    }
}

pub(super) async fn save_dependency(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, id)): Path<(Uuid, Uuid)>,
    Form(form): Form<DependencyForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let owner = auth.user_id;
    let adding = is_add(&form.operation)?;
    let mut tx = locked_project(&state, owner, project).await?;
    milestone_revision(&mut tx, owner, project, id).await?;
    milestone_revision(&mut tx, owner, project, form.prerequisite_id).await?;
    let changed = if adding {
        sqlx::query(
            "INSERT INTO milestone_dependencies(owner_id,project_id,prerequisite_id,dependent_id)
             VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(owner)
        .bind(project)
        .bind(form.prerequisite_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            if matches!(
                error.as_database_error().and_then(|e| e.constraint()),
                Some("milestone_dependencies_acyclic" | "milestone_dependencies_not_self")
            ) {
                return AppError::Conflict(
                    "That prerequisite would create a dependency cycle.".into(),
                );
            }
            AppError::from(error)
        })?
    } else {
        sqlx::query(
            "DELETE FROM milestone_dependencies WHERE owner_id=$1 AND project_id=$2 AND prerequisite_id=$3 AND dependent_id=$4"
        ).bind(owner).bind(project).bind(form.prerequisite_id).bind(id).execute(&mut *tx).await?
    };
    if changed.rows_affected() > 0 {
        touch(&mut tx, owner, project, &[id, form.prerequisite_id]).await?;
    }
    tx.commit().await?;
    Ok(detail_redirect(project, id))
}

pub(super) async fn save_issue(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, id)): Path<(Uuid, Uuid)>,
    Form(form): Form<IssueForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let owner = auth.user_id;
    let adding = is_add(&form.operation)?;
    let mut tx = locked_project(&state, owner, project).await?;
    milestone_revision(&mut tx, owner, project, id).await?;
    let changed = if adding {
        let archived: bool = sqlx::query_scalar(
            "SELECT archived FROM issues WHERE owner_id=$1 AND project_id=$2 AND id=$3",
        )
        .bind(owner)
        .bind(project)
        .bind(form.issue_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        if archived {
            return Err(AppError::Conflict(
                "Restore this issue before linking it to a milestone.".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO milestone_issues(owner_id,project_id,milestone_id,issue_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING"
        ).bind(owner).bind(project).bind(id).bind(form.issue_id).execute(&mut *tx).await?
    } else {
        sqlx::query("DELETE FROM milestone_issues WHERE owner_id=$1 AND project_id=$2 AND milestone_id=$3 AND issue_id=$4")
            .bind(owner).bind(project).bind(id).bind(form.issue_id).execute(&mut *tx).await?
    };
    if changed.rows_affected() > 0 {
        touch(&mut tx, owner, project, &[id]).await?;
    }
    tx.commit().await?;
    Ok(detail_redirect(project, id))
}

#[derive(Deserialize)]
pub(super) struct ReleaseForm {
    csrf: String,
    release_id: String,
    revision: i64,
}

pub(super) async fn save_release(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, id)): Path<(Uuid, Uuid)>,
    Form(form): Form<ReleaseForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let owner = auth.user_id;
    let mut tx = locked_project(&state, owner, project).await?;
    expected_revision(
        milestone_revision(&mut tx, owner, project, id).await?,
        Some(form.revision),
    )?;
    if form.release_id.is_empty() {
        sqlx::query("DELETE FROM milestone_releases WHERE owner_id=$1 AND project_id=$2 AND milestone_id=$3")
            .bind(owner).bind(project).bind(id).execute(&mut *tx).await?;
    } else {
        let release: Uuid = form
            .release_id
            .parse()
            .map_err(|_| AppError::BadRequest("Choose a release from this project.".into()))?;
        let result = sqlx::query(
            "INSERT INTO milestone_releases(owner_id,project_id,milestone_id,release_id)
             SELECT owner_id,project_id,$3,id FROM releases WHERE owner_id=$1 AND project_id=$2 AND id=$4
             ON CONFLICT (owner_id,project_id,milestone_id) DO UPDATE SET release_id=EXCLUDED.release_id"
        ).bind(owner).bind(project).bind(id).bind(release).execute(&mut *tx).await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
    }
    touch(&mut tx, owner, project, &[id]).await?;
    tx.commit().await?;
    Ok(detail_redirect(project, id))
}

#[derive(Deserialize)]
pub(super) struct DeleteForm {
    csrf: String,
    revision: i64,
}

pub(super) async fn delete(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, id)): Path<(Uuid, Uuid)>,
    Form(form): Form<DeleteForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let owner = auth.user_id;
    let mut tx = locked_project(&state, owner, project).await?;
    expected_revision(
        milestone_revision(&mut tx, owner, project, id).await?,
        Some(form.revision),
    )?;
    sqlx::query("DELETE FROM milestones WHERE owner_id=$1 AND project_id=$2 AND id=$3")
        .bind(owner)
        .bind(project)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/roadmap?project={project}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn span_and_target_have_independent_meaning() {
        let mut form = MilestoneForm {
            csrf: String::new(),
            id: None,
            revision: None,
            title: "Target".into(),
            details: String::new(),
            status: "planned".into(),
            start_on: String::new(),
            end_on: String::new(),
            target_on: "2028-02-29".into(),
        };
        assert!(form.values().is_ok());
        form.start_on = "2028-03-03".into();
        assert!(form.values().is_err());
        form.end_on = "2028-03-01".into();
        assert!(form.values().is_err());
        form.end_on = form.start_on.clone();
        assert!(form.values().is_ok());
        form.target_on = "2027-02-29".into();
        assert!(form.values().is_err());
    }
}
