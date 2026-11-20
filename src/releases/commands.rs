use std::{collections::BTreeMap, str::FromStr};

use axum::{
    Form, Router,
    extract::{Path, State},
    response::Redirect,
    routing::post,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, store};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/releases/{id}/members", post(membership))
        .route("/releases/{id}/publish", post(publish))
        .route("/releases/{id}/carryover", post(carryover))
        .route("/releases/{id}/amend", post(amend))
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum MemberAction {
    Assign,
    Remove,
}

#[derive(Deserialize)]
struct MembershipForm {
    csrf: String,
    issue: Uuid,
    from: String,
    action: MemberAction,
}

async fn membership(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(form): Form<MembershipForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let expected = if form.from.is_empty() {
        None
    } else {
        Some(read_value(&form.from, "previous draft")?)
    };
    change_member(
        &state.db,
        auth.user_id,
        id,
        form.issue,
        expected,
        form.action,
    )
    .await?;
    Ok(Redirect::to(&format!("/releases/{id}")))
}

async fn owner_transaction(db: &PgPool, owner: Uuid) -> AppResult<Transaction<'_, Postgres>> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(tx)
}

#[derive(sqlx::FromRow)]
struct LockedRelease {
    project_id: Uuid,
    revision: i64,
    published: bool,
}

impl LockedRelease {
    fn require_draft(&self) -> AppResult<()> {
        if self.published {
            return Err(AppError::Conflict(
                "This release has already been published.".into(),
            ));
        }
        Ok(())
    }
}

async fn lock_release(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    id: Uuid,
) -> AppResult<LockedRelease> {
    let project: Uuid =
        sqlx::query_scalar("SELECT project_id FROM releases WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(AppError::NotFound)?;
    store::lock_project(tx, owner, project).await?;
    let release = sqlx::query_as(
        "SELECT project_id,revision,published_at IS NOT NULL AS published
         FROM releases WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(release)
}

async fn change_member(
    db: &PgPool,
    owner: Uuid,
    id: Uuid,
    issue: Uuid,
    expected: Option<Uuid>,
    action: MemberAction,
) -> AppResult<()> {
    let mut tx = owner_transaction(db, owner).await?;
    let release = lock_release(&mut tx, owner, id).await?;
    release.require_draft()?;
    let project = release.project_id;
    let current: Option<Uuid> = sqlx::query_scalar(
        "SELECT release_id FROM release_members WHERE owner_id=$1 AND project_id=$2 AND issue_id=$3")
        .bind(owner).bind(project).bind(issue).fetch_optional(&mut *tx).await?;
    if current != expected || (action == MemberAction::Remove && current != Some(id)) {
        return Err(AppError::Conflict(
            "The issue’s draft membership changed. Refresh the release.".into(),
        ));
    }
    let destination = (action == MemberAction::Assign).then_some(id);
    if let Some(previous) = current.filter(|&previous| previous != id) {
        sqlx::query("SELECT id FROM releases WHERE owner_id=$1 AND project_id=$2 AND id=$3 AND published_at IS NULL FOR UPDATE")
            .bind(owner).bind(project).bind(previous).fetch_optional(&mut *tx).await?
            .ok_or_else(|| AppError::Conflict("The previous release has already closed.".into()))?;
    }
    let (status, archived): (String, bool) = sqlx::query_as(
        "SELECT status,archived FROM issues WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE")
        .bind(owner).bind(project).bind(issue).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
    if destination.is_some() && archived {
        return Err(AppError::Conflict(
            "Restore the archived issue before assigning it.".into(),
        ));
    }
    if current == destination {
        tx.commit().await?;
        return Ok(());
    }
    let at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    if let Some(previous) = current {
        sqlx::query("DELETE FROM release_members WHERE owner_id=$1 AND project_id=$2 AND release_id=$3 AND issue_id=$4")
            .bind(owner).bind(project).bind(previous).bind(issue).execute(&mut *tx).await?;
    }
    if let Some(next) = destination {
        sqlx::query("INSERT INTO release_members(owner_id,project_id,release_id,issue_id) VALUES($1,$2,$3,$4)")
            .bind(owner).bind(project).bind(next).bind(issue).execute(&mut *tx).await?;
    }
    let source = match action {
        MemberAction::Remove => "removal",
        MemberAction::Assign if current.is_some() => "move",
        MemberAction::Assign => "assignment",
    };
    for (release_id, in_scope) in [(current, false), (destination, true)] {
        if let Some(release_id) = release_id {
            sqlx::query("INSERT INTO release_membership_events(owner_id,project_id,release_id,issue_id,in_scope,status,occurred_at,source)
                         VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
                .bind(owner).bind(project).bind(release_id).bind(issue).bind(in_scope)
                .bind(&status).bind(at).bind(source).execute(&mut *tx).await?;
            sqlx::query("UPDATE releases SET revision=revision+1 WHERE owner_id=$1 AND project_id=$2 AND id=$3")
                .bind(owner).bind(project).bind(release_id).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Debug)]
struct PublishInput {
    csrf: String,
    revision: i64,
    command: Uuid,
    published_on: NaiveDate,
    carry_to: String,
    ship: BTreeMap<Uuid, i64>,
    carry: BTreeMap<Uuid, i64>,
}

fn read_value<T: FromStr>(raw: &str, field: &str) -> AppResult<T> {
    raw.parse()
        .map_err(|_| AppError::BadRequest(format!("Invalid {field}.")))
}

fn check_text(value: &str, field: &str, min: usize, max: usize) -> AppResult<()> {
    if value.contains('\0') || !(min..=max).contains(&value.chars().count()) {
        return Err(AppError::BadRequest(format!(
            "{field} must contain {min} to {max} characters."
        )));
    }
    Ok(())
}

#[derive(Default)]
struct ReviewFields {
    scalars: BTreeMap<String, String>,
    ship: BTreeMap<Uuid, i64>,
    carry: BTreeMap<Uuid, i64>,
}

impl ReviewFields {
    fn parse(fields: Vec<(String, String)>) -> AppResult<Self> {
        let mut parsed = Self::default();
        for (key, value) in fields {
            if key == "ship" || key == "carry" {
                let (issue, revision) = value
                    .split_once(':')
                    .ok_or_else(|| AppError::BadRequest("Invalid issue selection.".into()))?;
                let selection = if key == "ship" {
                    &mut parsed.ship
                } else {
                    &mut parsed.carry
                };
                let issue = read_value(issue, "issue ID")?;
                let revision = read_value(revision, "issue revision")?;
                if selection.insert(issue, revision).is_some() {
                    return Err(AppError::BadRequest(
                        "The same issue was selected twice.".into(),
                    ));
                }
            } else if matches!(
                key.as_str(),
                "csrf" | "revision" | "command" | "published_on" | "carry_to"
            ) && parsed.scalars.insert(key, value).is_some()
            {
                return Err(AppError::BadRequest("Duplicate confirmation field.".into()));
            }
        }
        Ok(parsed)
    }

    fn require(&mut self, name: &str) -> AppResult<String> {
        self.scalars
            .remove(name)
            .ok_or_else(|| AppError::BadRequest(format!("Missing {name}.")))
    }
}

impl PublishInput {
    fn parse(fields: Vec<(String, String)>) -> AppResult<Self> {
        let mut fields = ReviewFields::parse(fields)?;
        let input = Self {
            csrf: fields.require("csrf")?,
            revision: read_value(&fields.require("revision")?, "release revision")?,
            command: read_value(&fields.require("command")?, "publication command")?,
            published_on: read_value(&fields.require("published_on")?, "publication date")?,
            carry_to: fields
                .scalars
                .remove("carry_to")
                .unwrap_or_default()
                .trim()
                .to_owned(),
            ship: fields.ship,
            carry: fields.carry,
        };
        input.validate()?;
        Ok(input)
    }

    fn validate(&self) -> AppResult<()> {
        if self.ship.is_empty() {
            return Err(AppError::BadRequest(
                "Select at least one Done issue to publish.".into(),
            ));
        }
        if self.revision < 1
            || self
                .ship
                .values()
                .chain(self.carry.values())
                .any(|&r| r < 1)
        {
            return Err(AppError::BadRequest("Invalid preview revision.".into()));
        }
        if self.ship.keys().any(|id| self.carry.contains_key(id)) {
            return Err(AppError::BadRequest(
                "An issue can be shipped or carried forward once per release.".into(),
            ));
        }
        let min_name = usize::from(!self.carry.is_empty());
        check_text(self.carry_to.trim(), "New draft name", min_name, 150)
    }
}

async fn publish(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Redirect> {
    let input = PublishInput::parse(fields)?;
    auth.check_csrf(&input.csrf)?;
    publish_draft(&state.db, auth.user_id, id, &input).await?;
    Ok(Redirect::to(&format!("/releases/{id}")))
}

async fn publish_draft(db: &PgPool, owner: Uuid, id: Uuid, form: &PublishInput) -> AppResult<()> {
    let mut tx = owner_transaction(db, owner).await?;
    let prior: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM releases WHERE owner_id=$1 AND publish_command=$2")
            .bind(owner)
            .bind(form.command)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(prior) = prior {
        if prior != id {
            return Err(AppError::Conflict(
                "This confirmation was used for another release.".into(),
            ));
        }
        tx.commit().await?;
        return Ok(());
    }
    form.validate()?;
    let release = lock_release(&mut tx, owner, id).await?;
    release.require_draft()?;
    let project = release.project_id;
    if form.revision != release.revision {
        return Err(AppError::Conflict(
            "The release scope changed. Review the current preview.".into(),
        ));
    }
    let today: NaiveDate = sqlx::query_scalar(
        "SELECT (clock_timestamp() AT TIME ZONE timezone)::date FROM users WHERE id=$1",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await?;
    if form.published_on > today {
        return Err(AppError::BadRequest(
            "The shipment date must be today or earlier.".into(),
        ));
    }
    let members: Vec<(Uuid,String,i64,bool)> = sqlx::query_as(
        "SELECT i.id,i.status,i.revision,i.archived FROM release_members m
         JOIN issues i ON (i.owner_id,i.project_id,i.id)=(m.owner_id,m.project_id,m.issue_id)
         WHERE m.owner_id=$1 AND m.project_id=$2 AND m.release_id=$3 ORDER BY i.id FOR UPDATE OF i,m")
        .bind(owner).bind(project).bind(id).fetch_all(&mut *tx).await?;
    let mut matched = 0;
    for (issue, status, revision, archived) in members {
        let verified = if let Some(preview) = form.ship.get(&issue) {
            *preview == revision && status == "done"
        } else if let Some(preview) = form.carry.get(&issue) {
            *preview == revision && matches!(status.as_str(), "backlog" | "ready" | "in_progress")
        } else {
            continue;
        };
        if archived || !verified {
            return Err(AppError::Conflict(
                "A selected issue changed. Review the current preview.".into(),
            ));
        }
        matched += 1;
    }
    if matched != form.ship.len() + form.carry.len() {
        return Err(AppError::Conflict(
            "A selected issue is outside this release. Review its current scope.".into(),
        ));
    }
    let shipped: Vec<Uuid> = form.ship.keys().copied().collect();
    let carried: Vec<Uuid> = form.carry.keys().copied().collect();
    let next: Option<Uuid> = if carried.is_empty() {
        None
    } else {
        Some(create_carry_draft(&mut tx, owner, project, &form.carry_to).await?)
    };
    sqlx::query(
        "INSERT INTO release_entries(owner_id,project_id,release_id,issue_id,number,title,issue_revision)
         SELECT owner_id,project_id,$3,id,number,title,revision FROM issues
         WHERE owner_id=$1 AND project_id=$2 AND id=ANY($4)")
        .bind(owner).bind(project).bind(id).bind(&shipped).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO release_carryovers(owner_id,project_id,release_id,issue_id,number,title,status,issue_revision,next_release_id)
         SELECT m.owner_id,m.project_id,m.release_id,i.id,i.number,i.title,i.status,i.revision,
                CASE WHEN i.id=ANY($5) THEN $6::uuid END
         FROM release_members m JOIN issues i ON (i.owner_id,i.project_id,i.id)=(m.owner_id,m.project_id,m.issue_id)
         WHERE m.owner_id=$1 AND m.project_id=$2 AND m.release_id=$3 AND i.id<>ALL($4)")
        .bind(owner).bind(project).bind(id).bind(&shipped).bind(&carried).bind(next).execute(&mut *tx).await?;

    // The old scope history stays frozen at publication; mutable memberships become available for later drafts.
    sqlx::query(
        "DELETE FROM release_members WHERE owner_id=$1 AND project_id=$2 AND release_id=$3",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    if let Some(next) = next {
        assign_carry(&mut tx, owner, project, next, &carried, at).await?;
    }
    sqlx::query(
        "UPDATE releases SET published_on=$4,publish_command=$5,published_at=$6,revision=revision+1
                 WHERE owner_id=$1 AND project_id=$2 AND id=$3",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .bind(form.published_on)
    .bind(form.command)
    .bind(at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

struct CarryInput {
    csrf: String,
    carry_to: String,
    carry: BTreeMap<Uuid, i64>,
}

impl CarryInput {
    fn parse(fields: Vec<(String, String)>) -> AppResult<Self> {
        let mut fields = ReviewFields::parse(fields)?;
        if !fields.ship.is_empty() {
            return Err(AppError::BadRequest(
                "Shipped selections are only accepted at publication.".into(),
            ));
        }
        let input = Self {
            csrf: fields.require("csrf")?,
            carry_to: fields.require("carry_to")?.trim().to_owned(),
            carry: fields.carry,
        };
        input.validate()?;
        Ok(input)
    }

    fn validate(&self) -> AppResult<()> {
        check_text(self.carry_to.trim(), "New draft name", 1, 150)?;
        if self.carry.is_empty() || self.carry.values().any(|&revision| revision < 1) {
            return Err(AppError::BadRequest(
                "Select at least one open carryover issue with a current revision.".into(),
            ));
        }
        Ok(())
    }
}

async fn create_carry_draft(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    project: Uuid,
    name: &str,
) -> AppResult<Uuid> {
    let id = sqlx::query_scalar(
        "INSERT INTO releases(owner_id,project_id,name) VALUES($1,$2,$3) RETURNING id",
    )
    .bind(owner)
    .bind(project)
    .bind(name.trim())
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

async fn assign_carry(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    project: Uuid,
    next: Uuid,
    issues: &[Uuid],
    at: DateTime<Utc>,
) -> AppResult<()> {
    sqlx::query(
        "WITH added AS (
           INSERT INTO release_members(owner_id,project_id,release_id,issue_id)
           SELECT owner_id,project_id,$3,id FROM issues
           WHERE owner_id=$1 AND project_id=$2 AND id=ANY($4)
           RETURNING owner_id,project_id,release_id,issue_id
         )
         INSERT INTO release_membership_events
           (owner_id,project_id,release_id,issue_id,in_scope,status,occurred_at,source)
         SELECT a.owner_id,a.project_id,a.release_id,a.issue_id,true,i.status,$5,'carryover'
         FROM added a JOIN issues i ON (i.owner_id,i.project_id,i.id)=(a.owner_id,a.project_id,a.issue_id)",
    )
    .bind(owner)
    .bind(project)
    .bind(next)
    .bind(issues)
    .bind(at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn carryover(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Redirect> {
    let input = CarryInput::parse(fields)?;
    auth.check_csrf(&input.csrf)?;
    let next = move_published_carry(&state.db, auth.user_id, id, &input).await?;
    Ok(Redirect::to(&format!("/releases/{next}")))
}

async fn move_published_carry(
    db: &PgPool,
    owner: Uuid,
    id: Uuid,
    input: &CarryInput,
) -> AppResult<Uuid> {
    input.validate()?;
    let mut tx = owner_transaction(db, owner).await?;
    let release = lock_release(&mut tx, owner, id).await?;
    if !release.published {
        return Err(AppError::Conflict(
            "Publish this release before moving its remaining carryover.".into(),
        ));
    }
    let project = release.project_id;
    let selected: Vec<_> = input.carry.keys().copied().collect();
    let eligible: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT i.id,i.revision
         FROM release_carryovers c
         JOIN issues i ON (i.owner_id,i.project_id,i.id)=(c.owner_id,c.project_id,c.issue_id)
         WHERE c.owner_id=$1 AND c.project_id=$2 AND c.release_id=$3 AND c.issue_id=ANY($4)
           AND c.next_release_id IS NULL AND c.status IN ('backlog','ready','in_progress')
           AND NOT i.archived AND i.status IN ('backlog','ready','in_progress')
           AND NOT EXISTS (
             SELECT 1 FROM release_members m
             WHERE (m.owner_id,m.project_id,m.issue_id)=(c.owner_id,c.project_id,c.issue_id)
           )
         ORDER BY i.id FOR UPDATE OF i",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .bind(&selected)
    .fetch_all(&mut *tx)
    .await?;
    if eligible.len() != selected.len()
        || eligible
            .iter()
            .any(|(issue, revision)| input.carry.get(issue) != Some(revision))
    {
        return Err(AppError::Conflict(
            "A selected issue changed or moved. Refresh the release and review its carryover."
                .into(),
        ));
    }
    let next = create_carry_draft(&mut tx, owner, project, &input.carry_to).await?;
    let at = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    assign_carry(&mut tx, owner, project, next, &selected, at).await?;
    tx.commit().await?;
    Ok(next)
}

#[derive(Deserialize)]
struct AmendmentForm {
    csrf: String,
    issue: Uuid,
    reason: String,
    replacement: String,
}

async fn amend(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(form): Form<AmendmentForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    record_amendment(
        &state.db,
        auth.user_id,
        id,
        form.issue,
        &form.reason,
        &form.replacement,
    )
    .await?;
    Ok(Redirect::to(&format!("/releases/{id}")))
}

async fn record_amendment(
    db: &PgPool,
    owner: Uuid,
    id: Uuid,
    issue: Uuid,
    reason: &str,
    replacement: &str,
) -> AppResult<()> {
    let reason = reason.trim();
    let replacement = replacement.trim();
    check_text(reason, "Amendment reason", 1, 2000)?;
    check_text(replacement, "Corrected description", 1, 2000)?;
    let mut tx = owner_transaction(db, owner).await?;
    let release = lock_release(&mut tx, owner, id).await?;
    if !release.published {
        return Err(AppError::Conflict(
            "Publish this draft before amending its manifest.".into(),
        ));
    }
    let changed=sqlx::query(
        "INSERT INTO release_amendments(owner_id,project_id,release_id,issue_id,actor_id,reason,replacement)
         SELECT owner_id,project_id,release_id,issue_id,owner_id,$5,$6 FROM release_entries
         WHERE owner_id=$1 AND project_id=$2 AND release_id=$3 AND issue_id=$4")
        .bind(owner).bind(release.project_id).bind(id).bind(issue).bind(reason).bind(replacement)
        .execute(&mut *tx).await?.rows_affected();
    if changed != 1 {
        return Err(AppError::NotFound);
    }
    tx.commit().await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn preview() -> Vec<(String, String)> {
        [
            ("csrf", "csrf-value"),
            ("revision", "7"),
            ("command", "00000000-0000-0000-0000-000000000010"),
            ("published_on", "2026-11-01"),
            ("ship", "00000000-0000-0000-0000-000000000001:7"),
            ("ship", "00000000-0000-0000-0000-000000000002:3"),
            ("carry", "00000000-0000-0000-0000-000000000003:6"),
            ("carry_to", " Next weekly release "),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
    }
    #[test]
    fn later_carryover_accepts_only_carryover_checkboxes_and_requires_a_named_draft() {
        let fields: Vec<_> = preview()
            .into_iter()
            .filter(|(k, _)| matches!(k.as_str(), "csrf" | "carry" | "carry_to"))
            .collect();
        let valid = CarryInput::parse(fields.clone()).unwrap();
        assert_eq!(valid.carry_to, "Next weekly release");
        assert_eq!(valid.carry, BTreeMap::from([(Uuid::from_u128(3), 6)]));
        let mut ship = fields.clone();
        ship.push((
            "ship".into(),
            "00000000-0000-0000-0000-000000000001:1".into(),
        ));
        assert!(CarryInput::parse(ship).is_err());
        for key in ["carry_to", "carry", "csrf"] {
            let missing = fields.iter().filter(|(k, _)| k != key).cloned().collect();
            assert!(CarryInput::parse(missing).is_err());
        }
    }

    #[test]
    fn publication_reads_each_checkbox_and_its_own_preview_revision() {
        let p = PublishInput::parse(preview()).unwrap();
        assert_eq!(
            p.ship,
            BTreeMap::from([(Uuid::from_u128(1), 7), (Uuid::from_u128(2), 3)])
        );
        assert_eq!(p.carry, BTreeMap::from([(Uuid::from_u128(3), 6)]));
        assert_eq!(p.carry_to, "Next weekly release");
    }
    #[test]
    fn arbitrary_text_with_nul_cannot_reach_postgres() {
        let mut fields = preview();
        fields.retain(|(k, _)| k != "carry_to");
        fields.push(("carry_to".into(), "Next\0draft".into()));
        assert!(PublishInput::parse(fields).is_err());
        assert!(check_text("Correction\0reason", "Amendment reason", 1, 2000).is_err());
        assert!(check_text("Corrected\0caption", "Corrected description", 1, 2000).is_err());
    }
    #[test]
    fn ambiguous_selections_and_duplicate_confirmation_fields_are_invalid() {
        for field in [
            ("ship", "00000000-0000-0000-0000-000000000001:6"),
            ("carry", "00000000-0000-0000-0000-000000000002:3"),
            ("ship", "00000000-0000-0000-0000-000000000004:0"),
            ("ship", "no-issue:1"),
            ("revision", "8"),
        ] {
            let mut fields = preview();
            fields.push((field.0.into(), field.1.into()));
            assert!(PublishInput::parse(fields).is_err());
        }
    }
    #[test]
    fn a_publication_requires_a_shipment_and_a_destination_for_selected_carry() {
        for key in ["ship", "carry_to"] {
            let mut fields = preview();
            fields.retain(|(k, _)| k != key);
            assert!(PublishInput::parse(fields).is_err());
        }
    }
}
#[cfg(test)]
mod command_db_tests {
    use super::*;

    struct Workspace {
        db: PgPool,
        owner: Uuid,
        project: Uuid,
    }

    impl Workspace {
        async fn new(db: &PgPool) -> Self {
            let owner = Uuid::new_v4();
            sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
                .bind(owner)
                .bind(format!("{owner}@release.test"))
                .execute(db)
                .await
                .unwrap();
            let project = sqlx::query_scalar("INSERT INTO projects(owner_id,key,name) VALUES($1,'REL','Release test') RETURNING id")
                .bind(owner).fetch_one(db).await.unwrap();
            Self {
                db: db.clone(),
                owner,
                project,
            }
        }

        async fn draft(&self, name: &str) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO releases(owner_id,project_id,name) VALUES($1,$2,$3) RETURNING id",
            )
            .bind(self.owner)
            .bind(self.project)
            .bind(name)
            .fetch_one(&self.db)
            .await
            .unwrap()
        }

        async fn issue(&self, number: i64, status: &str) -> Uuid {
            sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title,status) VALUES($1,$2,$3,$4,$5) RETURNING id")
                .bind(self.owner).bind(self.project).bind(number)
                .bind(format!("Issue {number}")).bind(status)
                .fetch_one(&self.db).await.unwrap()
        }

        async fn assign(&self, release: Uuid, issue: Uuid) {
            change_member(
                &self.db,
                self.owner,
                release,
                issue,
                None,
                MemberAction::Assign,
            )
            .await
            .unwrap();
        }

        async fn preview(&self, release: Uuid, issue: Uuid) -> PublishInput {
            let revision = sqlx::query_scalar("SELECT revision FROM releases WHERE id=$1")
                .bind(release)
                .fetch_one(&self.db)
                .await
                .unwrap();
            PublishInput {
                csrf: "release-test".into(),
                revision,
                command: Uuid::new_v4(),
                published_on: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
                carry_to: String::new(),
                ship: [(issue, 1)].into_iter().collect(),
                carry: Default::default(),
            }
        }
    }

    #[sqlx::test]
    async fn racing_draft_moves_preserve_single_membership_and_both_sides_of_history(db: PgPool) {
        let ws = Workspace::new(&db).await;
        let first = ws.draft("First").await;
        let alternatives = [ws.draft("Second").await, ws.draft("Third").await];
        let issue = ws.issue(1, "in_progress").await;
        ws.assign(first, issue).await;
        let move_to =
            |id| change_member(&db, ws.owner, id, issue, Some(first), MemberAction::Assign);
        let (a, b) = tokio::join!(move_to(alternatives[0]), move_to(alternatives[1]));
        let winner = match (a, b) {
            (Ok(()), Err(AppError::Conflict(_))) => alternatives[0],
            (Err(AppError::Conflict(_)), Ok(())) => alternatives[1],
            _ => panic!("one competing move must succeed and the outdated move must conflict"),
        };
        let current: Vec<Uuid> =
            sqlx::query_scalar("SELECT release_id FROM release_members WHERE issue_id=$1")
                .bind(issue)
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(current, vec![winner]);
        change_member(
            &db,
            ws.owner,
            winner,
            issue,
            Some(winner),
            MemberAction::Remove,
        )
        .await
        .unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM release_members WHERE issue_id=$1")
                .bind(issue)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(remaining, 0);
        let events: Vec<(Uuid, bool, String, String)> = sqlx::query_as(
            "SELECT release_id,in_scope,status,source FROM release_membership_events WHERE issue_id=$1",
        )
        .bind(issue)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(events.len(), 4);
        for (draft, in_scope, source) in [
            (first, true, "assignment"),
            (first, false, "move"),
            (winner, true, "move"),
            (winner, false, "removal"),
        ] {
            assert!(events.contains(&(draft, in_scope, "in_progress".into(), source.into())));
        }
        let move_times: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT occurred_at) FROM release_membership_events WHERE issue_id=$1 AND source='move'",
        ).bind(issue).fetch_one(&db).await.unwrap();
        assert_eq!(move_times, 1);
    }

    #[sqlx::test]
    async fn membership_refuses_foreign_issues_and_archived_projects_without_writing_history(
        db: PgPool,
    ) {
        let ws = Workspace::new(&db).await;
        let outsider = Workspace::new(&db).await;
        let release = ws.draft("Protected draft").await;
        let local = ws.issue(1, "ready").await;
        let foreign = outsider.issue(1, "ready").await;
        assert!(
            change_member(
                &db,
                outsider.owner,
                release,
                local,
                None,
                MemberAction::Assign
            )
            .await
            .is_err()
        );
        assert!(
            change_member(&db, ws.owner, release, foreign, None, MemberAction::Assign)
                .await
                .is_err()
        );
        sqlx::query("UPDATE projects SET archived=true WHERE id=$1")
            .bind(ws.project)
            .execute(&db)
            .await
            .unwrap();
        assert!(
            change_member(&db, ws.owner, release, local, None, MemberAction::Assign)
                .await
                .is_err()
        );
        let counts: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM release_members),
                    (SELECT count(*) FROM release_membership_events)",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(counts, (0, 0));
    }

    #[sqlx::test]
    async fn reopened_selection_rejects_entire_publish_and_creates_no_carryover_draft(db: PgPool) {
        let ws = Workspace::new(&db).await;
        let release = ws.draft("Current").await;
        let valid = ws.issue(1, "done").await;
        let reopened = ws.issue(2, "done").await;
        let carry = ws.issue(3, "ready").await;
        for id in [valid, reopened, carry] {
            ws.assign(release, id).await;
        }
        let mut form = ws.preview(release, valid).await;
        form.ship.insert(reopened, 1);
        form.carry.insert(carry, 1);
        form.carry_to = "Next".into();
        sqlx::query("UPDATE issues SET status='in_progress',revision=revision+1 WHERE id=$1")
            .bind(reopened)
            .execute(&db)
            .await
            .unwrap();
        assert!(matches!(
            publish_draft(&db, ws.owner, release, &form).await,
            Err(AppError::Conflict(_))
        ));
        let state: (bool, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT published_at IS NULL,
                (SELECT count(*) FROM releases),
                (SELECT count(*) FROM release_entries),
                (SELECT count(*) FROM release_carryovers),
                (SELECT count(*) FROM release_members)
             FROM releases WHERE id=$1",
        )
        .bind(release)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(state, (true, 1, 0, 0, 3));
    }

    #[sqlx::test]
    async fn editing_a_selected_done_issue_requires_a_fresh_preview(db: PgPool) {
        let ws = Workspace::new(&db).await;
        let release = ws.draft("Draft").await;
        let issue = ws.issue(1, "done").await;
        ws.assign(release, issue).await;
        let mut preview = ws.preview(release, issue).await;
        sqlx::query("UPDATE issues SET title='Updated title',revision=revision+1 WHERE id=$1")
            .bind(issue)
            .execute(&db)
            .await
            .unwrap();
        assert!(matches!(
            publish_draft(&db, ws.owner, release, &preview).await,
            Err(AppError::Conflict(_))
        ));
        preview.ship.insert(issue, 2);
        publish_draft(&db, ws.owner, release, &preview)
            .await
            .unwrap();
        let snapshot: (String, i64) =
            sqlx::query_as("SELECT title,issue_revision FROM release_entries WHERE release_id=$1")
                .bind(release)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(snapshot, ("Updated title".into(), 2));
    }

    #[sqlx::test]
    async fn archived_done_members_cannot_ship_even_with_current_revision(db: PgPool) {
        let ws = Workspace::new(&db).await;
        let release = ws.draft("Draft").await;
        let issue = ws.issue(1, "done").await;
        ws.assign(release, issue).await;
        sqlx::query("UPDATE issues SET archived=true,revision=2 WHERE id=$1")
            .bind(issue)
            .execute(&db)
            .await
            .unwrap();
        let mut preview = ws.preview(release, issue).await;
        preview.ship.insert(issue, 2);
        assert!(matches!(
            publish_draft(&db, ws.owner, release, &preview).await,
            Err(AppError::Conflict(_)) | Err(AppError::BadRequest(_))
        ));
        let entries: i64 =
            sqlx::query_scalar("SELECT count(*) FROM release_entries WHERE release_id=$1")
                .bind(release)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(entries, 0);
    }

    #[sqlx::test]
    async fn a_done_member_cannot_be_moved_as_active_carryover(db: PgPool) {
        let ws = Workspace::new(&db).await;
        let release = ws.draft("Draft").await;
        let shipped = ws.issue(1, "done").await;
        let unshipped = ws.issue(2, "done").await;
        ws.assign(release, shipped).await;
        ws.assign(release, unshipped).await;
        let mut preview = ws.preview(release, shipped).await;
        preview.carry_to = "New draft".into();
        preview.carry.insert(unshipped, 1);
        assert!(
            publish_draft(&db, ws.owner, release, &preview)
                .await
                .is_err()
        );
        let counts: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM releases),
                (SELECT count(*) FROM release_entries),(SELECT count(*) FROM release_members)",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(counts, (1, 0, 2));
    }

    #[sqlx::test]
    async fn empty_and_future_publications_leave_the_release_unpublished(db: PgPool) {
        let ws = Workspace::new(&db).await;
        let release = ws.draft("Draft").await;
        let issue = ws.issue(1, "done").await;
        ws.assign(release, issue).await;
        let mut preview = ws.preview(release, issue).await;
        preview.ship.clear();
        assert!(
            publish_draft(&db, ws.owner, release, &preview)
                .await
                .is_err()
        );
        preview.ship.insert(issue, 1);
        preview.published_on = chrono::Utc::now().date_naive().succ_opt().unwrap();
        assert!(
            publish_draft(&db, ws.owner, release, &preview)
                .await
                .is_err()
        );
        let draft: bool =
            sqlx::query_scalar("SELECT published_at IS NULL FROM releases WHERE id=$1")
                .bind(release)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(draft);
    }

    #[sqlx::test]
    async fn publish_retries_snapshot_once_and_move_only_selected_active_carryover(db: PgPool) {
        let ws = Workspace::new(&db).await;
        let outsider = Workspace::new(&db).await;
        let release = ws.draft("Launch").await;
        let shipped = ws.issue(1, "done").await;
        let skipped = ws.issue(2, "done").await;
        let carry = ws.issue(3, "backlog").await;
        let canceled = ws.issue(4, "canceled").await;
        for id in [shipped, skipped, carry, canceled] {
            ws.assign(release, id).await;
        }
        let mut form = ws.preview(release, shipped).await;
        form.carry.insert(carry, 1);
        form.carry_to = "Next".into();
        assert!(
            publish_draft(&db, outsider.owner, release, &form)
                .await
                .is_err()
        );
        let (first, retry) = tokio::join!(
            publish_draft(&db, ws.owner, release, &form),
            publish_draft(&db, ws.owner, release, &form),
        );
        first.unwrap();
        retry.unwrap();
        let next: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM releases WHERE project_id=$1 AND name='Next' AND published_at IS NULL",
        )
        .bind(ws.project)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(next.len(), 1);
        let next = next[0];
        let members: Vec<(Uuid, Uuid)> =
            sqlx::query_as("SELECT release_id,issue_id FROM release_members")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(members, vec![(next, carry)]);
        let kept: Vec<(Uuid,String,Option<Uuid>)> = sqlx::query_as("SELECT issue_id,status,next_release_id FROM release_carryovers WHERE release_id=$1 ORDER BY number")
            .bind(release).fetch_all(&db).await.unwrap();
        assert_eq!(
            kept,
            vec![
                (skipped, "done".into(), None),
                (carry, "backlog".into(), Some(next)),
                (canceled, "canceled".into(), None)
            ]
        );
        let carry_events: Vec<(Uuid, bool)> = sqlx::query_as(
            "SELECT release_id,in_scope FROM release_membership_events WHERE issue_id=$1",
        )
        .bind(carry)
        .fetch_all(&db)
        .await
        .unwrap();
        assert!(carry_events.contains(&(release, true)));
        assert!(carry_events.contains(&(next, true)));
        let entries: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT issue_id,title FROM release_entries WHERE release_id=$1")
                .bind(release)
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(entries, vec![(shipped, "Issue 1".into())]);

        assert!(
            record_amendment(
                &db,
                outsider.owner,
                release,
                shipped,
                "Error",
                "Other title"
            )
            .await
            .is_err()
        );
        assert!(
            record_amendment(&db, ws.owner, release, skipped, "Error", "Other title")
                .await
                .is_err()
        );
        record_amendment(
            &db,
            ws.owner,
            release,
            shipped,
            "Clarify scope",
            "Issue 1 clarified",
        )
        .await
        .unwrap();
        record_amendment(
            &db,
            ws.owner,
            release,
            shipped,
            "Fix spelling",
            "Issue 1 corrected",
        )
        .await
        .unwrap();
        let amendments: Vec<(Uuid,String,String)> = sqlx::query_as("SELECT actor_id,reason,replacement FROM release_amendments WHERE release_id=$1 AND issue_id=$2 ORDER BY reason")
            .bind(release).bind(shipped).fetch_all(&db).await.unwrap();
        assert_eq!(
            amendments,
            vec![
                (ws.owner, "Clarify scope".into(), "Issue 1 clarified".into()),
                (ws.owner, "Fix spelling".into(), "Issue 1 corrected".into())
            ]
        );
        let original: String = sqlx::query_scalar(
            "SELECT title FROM release_entries WHERE release_id=$1 AND issue_id=$2",
        )
        .bind(release)
        .bind(shipped)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(original, "Issue 1");

        ws.assign(next, shipped).await;
        let later = ws.preview(next, shipped).await;
        publish_draft(&db, ws.owner, next, &later).await.unwrap();
        let appearances: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT release_id) FROM release_entries WHERE issue_id=$1",
        )
        .bind(shipped)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(appearances, 2);
    }
    #[sqlx::test]
    async fn later_carry_uses_live_revision_and_preserves_the_published_snapshot(db: PgPool) {
        let ws = Workspace::new(&db).await;
        let release = ws.draft("Original").await;
        let shipped = ws.issue(1, "done").await;
        let open = ws.issue(2, "ready").await;
        ws.assign(release, shipped).await;
        ws.assign(release, open).await;
        let publish = ws.preview(release, shipped).await;
        publish_draft(&db, ws.owner, release, &publish)
            .await
            .unwrap();
        let source_events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM release_membership_events WHERE release_id=$1",
        )
        .bind(release)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("UPDATE issues SET title='Current issue name', status='in_progress', revision=2 WHERE id=$1")
        .bind(open).execute(&db).await.unwrap();
        let input = CarryInput {
            csrf: "release-test".into(),
            carry_to: "Follow-up".into(),
            carry: [(open, 2)].into_iter().collect(),
        };
        let (a, b) = tokio::join!(
            move_published_carry(&db, ws.owner, release, &input),
            move_published_carry(&db, ws.owner, release, &input)
        );
        let next = match (a, b) {
            (Ok(id), Err(AppError::Conflict(_))) | (Err(AppError::Conflict(_)), Ok(id)) => id,
            _ => panic!("only one concurrent carry may assign an issue and create a draft"),
        };
        let destination: (Uuid, String) = sqlx::query_as(
            "SELECT m.release_id,e.status FROM release_members m JOIN release_membership_events e
            ON e.release_id=m.release_id AND e.issue_id=m.issue_id AND e.in_scope
         WHERE m.issue_id=$1",
        )
        .bind(open)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(destination, (next, "in_progress".into()));
        let historic: (String,String,i64,Option<Uuid>) = sqlx::query_as(
        "SELECT title,status,issue_revision,next_release_id FROM release_carryovers WHERE release_id=$1 AND issue_id=$2")
        .bind(release).bind(open).fetch_one(&db).await.unwrap();
        assert_eq!(historic, ("Issue 2".into(), "ready".into(), 1, None));
        let totals: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM release_entries WHERE release_id=$1 AND issue_id=$2),
                (SELECT count(*) FROM release_membership_events WHERE release_id=$1),
                (SELECT count(*) FROM releases WHERE published_at IS NULL)",
        )
        .bind(release)
        .bind(shipped)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(totals, (1, source_events, 1));
    }

    #[sqlx::test]
    async fn a_late_carry_rejects_the_entire_selection_if_any_item_is_ineligible(db: PgPool) {
        let ws = Workspace::new(&db).await;
        let other = Workspace::new(&db).await;
        let release = ws.draft("Original").await;
        let assigned_draft = ws.draft("Existing draft").await;
        let shipped = ws.issue(1, "done").await;
        let valid = ws.issue(2, "ready").await;
        let stale = ws.issue(3, "backlog").await;
        let closed = ws.issue(4, "ready").await;
        let assigned = ws.issue(5, "ready").await;
        let unrelated = ws.issue(6, "ready").await;
        let initially_canceled = ws.issue(7, "canceled").await;
        let archived = ws.issue(8, "ready").await;
        let carried_at_publish = ws.issue(9, "ready").await;
        for id in [
            shipped,
            valid,
            stale,
            closed,
            assigned,
            initially_canceled,
            archived,
            carried_at_publish,
        ] {
            ws.assign(release, id).await;
        }
        let mut publish = ws.preview(release, shipped).await;
        publish.carry_to = "Initial follow-up".into();
        publish.carry.insert(carried_at_publish, 1);
        publish_draft(&db, ws.owner, release, &publish)
            .await
            .unwrap();
        sqlx::query("UPDATE issues SET revision=2 WHERE id=$1")
            .bind(stale)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE issues SET status='done',revision=2 WHERE id=$1")
            .bind(closed)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE issues SET status='ready',revision=2 WHERE id=$1")
            .bind(initially_canceled)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE issues SET archived=true,revision=2 WHERE id=$1")
            .bind(archived)
            .execute(&db)
            .await
            .unwrap();
        let first_followup: Uuid = sqlx::query_scalar(
            "SELECT next_release_id FROM release_carryovers WHERE release_id=$1 AND issue_id=$2",
        )
        .bind(release)
        .bind(carried_at_publish)
        .fetch_one(&db)
        .await
        .unwrap();
        change_member(
            &db,
            ws.owner,
            first_followup,
            carried_at_publish,
            Some(first_followup),
            MemberAction::Remove,
        )
        .await
        .unwrap();
        ws.assign(assigned_draft, assigned).await;
        for (actor, extra, revision) in [
            (ws.owner, stale, 1),
            (ws.owner, closed, 2),
            (ws.owner, assigned, 1),
            (ws.owner, unrelated, 1),
            (ws.owner, initially_canceled, 2),
            (ws.owner, archived, 2),
            (ws.owner, carried_at_publish, 1),
            (other.owner, stale, 2),
        ] {
            let input = CarryInput {
                csrf: "release-test".into(),
                carry_to: "Unexpected draft".into(),
                carry: [(valid, 1), (extra, revision)].into_iter().collect(),
            };
            assert!(matches!(
                move_published_carry(&db, actor, release, &input).await,
                Err(AppError::NotFound
                    | AppError::Forbidden
                    | AppError::Conflict(_)
                    | AppError::BadRequest(_))
            ));
        }
        let drafts: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM releases WHERE published_at IS NULL")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(drafts.len(), 2);
        assert!(drafts.contains(&assigned_draft));
        assert!(drafts.contains(&first_followup));
        let members: Vec<(Uuid, Uuid)> =
            sqlx::query_as("SELECT release_id,issue_id FROM release_members")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(members, vec![(assigned_draft, assigned)]);
    }
}
