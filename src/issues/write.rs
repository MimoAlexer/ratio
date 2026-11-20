use axum::{
    extract::{Form, Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Postgres, Transaction};
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, store};

type Tx<'a> = Transaction<'a, Postgres>;
type Time = DateTime<Utc>;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct NewIssue {
    pub project: Uuid,
    pub title: String,
    pub body: String,
    pub status: String,
    pub priority: String,
    pub kind: String,
    pub labels: String,
    pub next_action: String,
    pub severity: String,
}

impl NewIssue {
    pub(crate) fn validate(&self) -> AppResult<()> {
        Content::from_draft(self.clone(), false).map(|_| ())
    }
}

#[derive(Deserialize, Serialize)]
pub(super) struct WriteForm {
    #[serde(skip_serializing)]
    csrf: String,
    #[serde(skip_serializing)]
    command_id: Uuid,
    revision: Option<i64>,
    project: Option<Uuid>,
    title: Option<String>,
    body: Option<String>,
    status: Option<String>,
    priority: Option<String>,
    kind: Option<String>,
    labels: Option<String>,
    next_action: Option<String>,
    severity: Option<String>,
    finding_closed: Option<String>,
    archived: Option<bool>,
    blocker_id: Option<Uuid>,
    operation: Option<String>,
}

fn bad(message: &str) -> AppError {
    AppError::BadRequest(message.to_string())
}

fn conflict(message: &str) -> AppError {
    AppError::Conflict(message.to_string())
}

fn valid_status(status: &str) -> bool {
    super::STATUSES.contains(&status)
}

fn parse_labels(raw: &str) -> AppResult<Vec<String>> {
    let mut labels: Vec<String> = Vec::new();
    for label in raw
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        if label.chars().count() > 48 {
            return Err(bad("Each label can contain up to 48 characters."));
        }
        if !labels.iter().any(|existing| existing == label) {
            if labels.len() == 24 {
                return Err(bad("An issue can have up to 24 labels."));
            }
            labels.push(label.to_owned());
        }
    }
    Ok(labels)
}

impl WriteForm {
    fn draft(&self, project: Uuid) -> AppResult<NewIssue> {
        Ok(NewIssue {
            project,
            title: self
                .title
                .clone()
                .ok_or_else(|| bad("Enter an issue title."))?,
            body: self.body.clone().unwrap_or_default(),
            status: self.status.clone().unwrap_or_else(|| "ready".to_owned()),
            priority: self.priority.clone().unwrap_or_else(|| "normal".to_owned()),
            kind: self.kind.clone().unwrap_or_else(|| "feature".to_owned()),
            labels: self.labels.clone().unwrap_or_default(),
            next_action: self.next_action.clone().unwrap_or_default(),
            severity: self.severity.clone().unwrap_or_else(|| "none".to_owned()),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow)]
struct Content {
    title: String,
    body: String,
    status: String,
    priority: String,
    kind: String,
    labels: Vec<String>,
    next_action: String,
    manual_severity: String,
    finding_closed: bool,
    archived: bool,
    checkpoint_at: Option<Time>,
}

impl Content {
    fn from_draft(draft: NewIssue, closed: bool) -> AppResult<Self> {
        let mut content = Self {
            title: draft.title.trim().to_owned(),
            body: draft.body,
            status: draft.status,
            priority: draft.priority,
            kind: draft.kind,
            labels: parse_labels(&draft.labels)?,
            next_action: draft.next_action.trim().to_owned(),
            manual_severity: draft.severity,
            finding_closed: closed,
            archived: false,
            checkpoint_at: None,
        };
        if content.manual_severity == "none" {
            content.finding_closed = false;
        }
        content.validate()?;
        Ok(content)
    }

    fn edited(&self, form: &WriteForm) -> AppResult<Self> {
        let field =
            |incoming: &Option<String>, old: &String| incoming.as_ref().unwrap_or(old).clone();
        let severity = field(&form.severity, &self.manual_severity);
        let closed = match form.finding_closed.as_deref() {
            Some("true" | "on") => true,
            Some("false" | "off") => false,
            None if form.severity.is_some() => false, // An unchecked checkbox in the full form.
            None => self.finding_closed,
            _ => return Err(bad("Choose whether the manual finding is closed.")),
        };
        let next = Self {
            title: form
                .title
                .as_deref()
                .unwrap_or(&self.title)
                .trim()
                .to_owned(),
            body: field(&form.body, &self.body),
            status: field(&form.status, &self.status),
            priority: field(&form.priority, &self.priority),
            kind: field(&form.kind, &self.kind),
            labels: match &form.labels {
                Some(raw) => parse_labels(raw)?,
                None => self.labels.clone(),
            },
            next_action: form
                .next_action
                .as_deref()
                .unwrap_or(&self.next_action)
                .trim()
                .to_owned(),
            finding_closed: severity != "none" && closed,
            manual_severity: severity,
            archived: self.archived,
            checkpoint_at: self.checkpoint_at,
        };
        next.validate()?;
        Ok(next)
    }

    fn validate(&self) -> AppResult<()> {
        if self.title.trim().is_empty() || self.title.chars().count() > 240 {
            return Err(bad("Enter an issue title between 1 and 240 characters."));
        }
        if self.body.len() > 2 * 1024 * 1024 {
            return Err(bad("An issue description can contain up to 2 MiB."));
        }
        if self.next_action.chars().count() > 2_000 {
            return Err(bad("A next action can contain up to 2,000 characters."));
        }
        if !valid_status(&self.status)
            || !matches!(self.priority.as_str(), "low" | "normal" | "high" | "urgent")
            || !matches!(
                self.kind.as_str(),
                "feature" | "bug" | "maintenance" | "security"
            )
            || !matches!(
                self.manual_severity.as_str(),
                "none" | "low" | "medium" | "high" | "critical"
            )
        {
            return Err(bad("Choose a listed status, priority, kind and severity."));
        }
        if self.manual_severity != "none" && self.next_action.trim().is_empty() {
            return Err(bad("Enter a next action for the manual security finding."));
        }
        if self.labels.len() > 24 || self.labels.iter().any(|label| label.chars().count() > 48) {
            return Err(bad("Use up to 24 labels of 48 characters each."));
        }
        Ok(())
    }

    fn billed_bytes(&self) -> i64 {
        let label_bytes = self.labels.iter().map(String::len).sum::<usize>();
        (self.title.len() + self.body.len() + self.next_action.len() + label_bytes) as i64
    }

    fn open_critical(&self) -> bool {
        self.manual_severity == "critical" && !self.finding_closed
    }
}

#[derive(FromRow)]
struct Stored {
    revision: i64,
    started_at: Option<Time>,
    completed_at: Option<Time>,
    #[sqlx(flatten)]
    content: Content,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct Relation {
    blocker_id: Uuid,
    present: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    fields: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relation: Option<Relation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_hash: Option<String>,
}

fn as_fields(content: &Content) -> AppResult<Map<String, Value>> {
    match serde_json::to_value(content) {
        Ok(Value::Object(fields)) => Ok(fields),
        _ => Err(AppError::Internal(
            "Could not encode the issue revision.".to_owned(),
        )),
    }
}

fn changes(old: &Content, new: &Content) -> AppResult<(Snapshot, Snapshot)> {
    let old_fields = as_fields(old)?;
    let new_fields = as_fields(new)?;
    let (mut before, mut after) = (Snapshot::default(), Snapshot::default());
    for (name, value) in new_fields {
        if let Some(previous) = old_fields.get(&name).filter(|previous| **previous != value) {
            before.fields.insert(name.clone(), previous.clone());
            after.fields.insert(name, value);
        }
    }
    Ok((before, after))
}

fn restore(content: &Content, snapshot: &Snapshot) -> AppResult<Content> {
    let mut fields = as_fields(content)?;
    for (name, old) in &snapshot.fields {
        if !fields.contains_key(name) {
            return Err(conflict(
                "This historical revision contains an unknown field.",
            ));
        }
        fields.insert(name.clone(), old.clone());
    }
    let restored: Content = serde_json::from_value(Value::Object(fields))
        .map_err(|_| conflict("This historical revision cannot be applied."))?;
    restored.validate()?;
    Ok(restored)
}

fn changed_times(
    old_status: Option<&str>,
    new_status: &str,
    started: Option<Time>,
    completed: Option<Time>,
    now: Time,
) -> (Option<Time>, Option<Time>) {
    if old_status == Some(new_status) {
        return (started, completed);
    }
    let first_start = started.or_else(|| (new_status == "in_progress").then_some(now));
    let last_done = (new_status == "done").then_some(now);
    (first_start, last_done)
}

fn fingerprint(payload: &impl Serialize) -> AppResult<String> {
    let bytes = serde_json::to_vec(payload)
        .map_err(|_| AppError::Internal("Could not identify this issue command.".to_owned()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

async fn lock_owner(tx: &mut Tx<'_>, owner: Uuid) -> AppResult<()> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(())
}

#[derive(FromRow)]
struct Receipt {
    issue_id: Uuid,
    project_id: Uuid,
    action: String,
    hash: Option<String>,
}

async fn receipt(tx: &mut Tx<'_>, owner: Uuid, command: Uuid) -> AppResult<Option<Receipt>> {
    Ok(sqlx::query_as(
        "SELECT issue_id, project_id, action, after_state->>'command_hash' AS hash
         FROM issue_events WHERE owner_id=$1 AND command_id=$2",
    )
    .bind(owner)
    .bind(command)
    .fetch_optional(&mut **tx)
    .await?)
}

impl Receipt {
    fn verify(&self, action: &str, hash: &str) -> AppResult<()> {
        if self.action == action && self.hash.as_deref() == Some(hash) {
            Ok(())
        } else {
            Err(conflict(
                "This command was already used for another change. Reload the issue and try again.",
            ))
        }
    }
}

pub(crate) async fn create_in(
    tx: &mut Tx<'_>,
    owner: Uuid,
    command: Uuid,
    draft: NewIssue,
) -> AppResult<Uuid> {
    insert_issue(tx, owner, command, draft, CreateSource::LinkedWorkflow).await
}

#[derive(Clone, Copy)]
enum CreateSource {
    IssueForm,
    LinkedWorkflow,
}

async fn insert_issue(
    tx: &mut Tx<'_>,
    owner: Uuid,
    command: Uuid,
    draft: NewIssue,
    source: CreateSource,
) -> AppResult<Uuid> {
    let project = draft.project;
    let hash = fingerprint(&draft)?;
    lock_owner(tx, owner).await?;
    if let Some(prior) = receipt(tx, owner, command).await? {
        prior.verify("create", &hash)?;
        if prior.project_id != project {
            return Err(conflict("This command belongs to another project."));
        }
        return Ok(prior.issue_id);
    }
    store::lock_project(tx, owner, project).await?;
    let mut content = Content::from_draft(draft, false)?;
    store::check_storage(tx, owner, content.billed_bytes()).await?;
    let now: Time = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    if matches!(source, CreateSource::IssueForm)
        && !content.next_action.is_empty()
        && matches!(content.status.as_str(), "backlog" | "ready" | "in_progress")
    {
        content.checkpoint_at = Some(now);
    }
    let (started, completed) = changed_times(None, &content.status, None, None, now);
    let issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues
         (owner_id,project_id,number,title,body,status,priority,kind,labels,next_action,manual_severity,
          security_critical,created_at,updated_at,started_at,completed_at,checkpoint_at)
         SELECT $1,$2,COALESCE(MAX(number),0)+1,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$12,$13,$14,$15
         FROM issues WHERE owner_id=$1 AND project_id=$2 RETURNING id",
    )
    .bind(owner).bind(project).bind(&content.title).bind(&content.body)
    .bind(&content.status).bind(&content.priority).bind(&content.kind).bind(&content.labels)
    .bind(&content.next_action).bind(&content.manual_severity).bind(content.open_critical())
    .bind(now).bind(started).bind(completed).bind(content.checkpoint_at)
    .fetch_one(&mut **tx).await?;

    let after = Snapshot {
        fields: as_fields(&content)?,
        command_hash: Some(hash),
        ..Default::default()
    };
    sqlx::query(
        "INSERT INTO issue_events (owner_id,project_id,issue_id,new_status,command_id,action,
             after_state,revision_after,occurred_at,ingested_at)
         VALUES ($1,$2,$3,$4,$5,'create',$6,1,$7,$7)",
    )
    .bind(owner)
    .bind(project)
    .bind(issue)
    .bind(&content.status)
    .bind(command)
    .bind(sqlx::types::Json(after))
    .bind(now)
    .execute(&mut **tx)
    .await?;
    crate::notes::refresh_issue(tx, owner, project, issue).await?;
    Ok(issue)
}

pub(super) async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Form(form): Form<WriteForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let project = form.project.ok_or_else(|| bad("Choose a project."))?;
    let draft = form.draft(project)?;
    let mut tx = state.db.begin().await?;
    let issue = insert_issue(
        &mut tx,
        auth.user_id,
        form.command_id,
        draft,
        CreateSource::IssueForm,
    )
    .await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/issues/{issue}")))
}

async fn set_blocker(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    issue: Uuid,
    relation: Relation,
) -> AppResult<bool> {
    let blocker = relation.blocker_id;
    if issue == blocker {
        return Err(bad("An issue cannot block itself."));
    }
    let archived: bool = sqlx::query_scalar(
        "SELECT archived FROM issues WHERE owner_id=$1 AND project_id=$2 AND id=$3",
    )
    .bind(owner)
    .bind(project)
    .bind(blocker)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if !relation.present {
        let result = sqlx::query("DELETE FROM issue_blockers WHERE owner_id=$1 AND project_id=$2 AND issue_id=$3 AND blocker_id=$4")
            .bind(owner).bind(project).bind(issue).bind(blocker).execute(&mut **tx).await?;
        return Ok(result.rows_affected() > 0);
    }
    if archived {
        return Err(conflict("Restore the archived blocker before linking it."));
    }
    // All writers hold the same project lock, including a compensating dependency change.
    let cycle: bool = sqlx::query_scalar(
        "WITH RECURSIVE upstream(id) AS (
             SELECT $3::uuid
             UNION
             SELECT b.blocker_id FROM issue_blockers b JOIN upstream u ON u.id=b.issue_id
             WHERE b.owner_id=$1 AND b.project_id=$2
         ) SELECT EXISTS(SELECT 1 FROM upstream WHERE id=$4)",
    )
    .bind(owner)
    .bind(project)
    .bind(blocker)
    .bind(issue)
    .fetch_one(&mut **tx)
    .await?;
    if cycle {
        return Err(conflict("This blocker would create a dependency cycle."));
    }
    let result = sqlx::query("INSERT INTO issue_blockers(owner_id,project_id,issue_id,blocker_id) VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(owner).bind(project).bind(issue).bind(blocker).execute(&mut **tx).await?;
    Ok(result.rows_affected() > 0)
}

pub(super) async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path((issue, action)): Path<(Uuid, String)>,
    Form(form): Form<WriteForm>,
) -> AppResult<Response> {
    auth.check_csrf(&form.csrf)?;
    let mut tx = state.db.begin().await?;
    match apply_in(&mut tx, auth.user_id, issue, &action, &form).await {
        Ok(()) => {
            tx.commit().await?;
            Ok(Redirect::to(&format!("/issues/{issue}")).into_response())
        }
        Err(AppError::Conflict(reason)) if action == "edit" || action == "checkpoint" => {
            tx.rollback().await?;
            let current: Option<SavedIssue> = sqlx::query_as(
                "SELECT i.*, p.key AS project_key, p.archived AS project_archived
                 FROM issues i JOIN projects p ON (p.owner_id,p.id)=(i.owner_id,i.project_id)
                 WHERE i.owner_id=$1 AND i.id=$2",
            )
            .bind(auth.user_id)
            .bind(issue)
            .fetch_optional(&state.db)
            .await
            .unwrap_or_else(|_| {
                tracing::warn!("Could not load saved issue for conflict review");
                None
            });
            let body = conflict_review(&auth, issue, &action, &reason, &form, current.as_ref());
            let page = crate::ui::page(&auth, "Review submitted changes", "issues", body);
            Ok((StatusCode::CONFLICT, page).into_response())
        }
        Err(err) => Err(err),
    }
}

#[derive(FromRow)]
struct SavedIssue {
    #[sqlx(flatten)]
    issue: super::Issue,
    project_key: String,
    project_archived: bool,
}

fn conflict_review(
    auth: &Auth,
    id: Uuid,
    action: &str,
    reason: &str,
    form: &WriteForm,
    saved: Option<&SavedIssue>,
) -> maud::Markup {
    use maud::html;
    let retry = saved.filter(|s| {
        !s.project_archived
            && !s.issue.archived
            && (action == "edit" || !matches!(s.issue.status.as_str(), "done" | "canceled"))
    });
    let closed = form
        .finding_closed
        .as_deref()
        .or_else(|| form.severity.as_ref().map(|_| "false"));
    let choices = [
        ("status", "Status", form.status.as_deref(), super::STATUSES),
        (
            "priority",
            "Priority",
            form.priority.as_deref(),
            super::PRIORITIES,
        ),
        ("kind", "Type", form.kind.as_deref(), super::KINDS),
        (
            "severity",
            "Manual severity",
            form.severity.as_deref(),
            super::SEVERITIES,
        ),
        (
            "finding_closed",
            "Finding closed",
            closed,
            &["true", "false"][..],
        ),
    ];
    html! {
        header.page-head { h1 { "Review your submitted changes" } }
        p.notice { (reason) }
        @if let Some(saved) = saved {
            @let current = &saved.issue;
            details.panel {
                summary { "Compare saved version · " (saved.project_key) "-" (current.number) " · Revision " (current.revision) }
                h2 { (&current.title) }
                @if saved.project_archived || current.archived { p.muted { "Archived · Read-only" } }
                dl.stack {
                    @for (name, value) in [
                        ("Description", current.body.as_str()),
                        ("Next action", current.next_action.as_str()),
                        ("Labels", &current.labels.join(", ")),
                        ("Status", super::label(&current.status)),
                        ("Priority", super::label(&current.priority)),
                        ("Type", super::label(&current.kind)),
                        ("Manual severity", super::label(&current.manual_severity)),
                        ("Finding closed", if current.finding_closed { "Yes" } else { "No" }),
                    ] {
                        div { dt { (name) } dd.prose { @if value.is_empty() { "—" } @else { (value) } } }
                    }
                }
            }
        } @else {
            p { "The saved version is unavailable. You can copy your draft below." }
        }
        section.panel {
            h2 { "Your draft" }
            form.stack method="post" action=(format!("/issues/{id}/{action}")) {
                @if let Some(saved) = retry {
                    (crate::ui::csrf(auth))
                    input type="hidden" name="revision" value=(saved.issue.revision);
                    input type="hidden" name="command_id" value=(Uuid::new_v4());
                }
                @for (name, caption, submitted, rows) in [
                    ("title", "Title", form.title.as_deref(), 1),
                    ("body", "Description", form.body.as_deref(), 5),
                    ("next_action", "Next action", form.next_action.as_deref(), 2),
                    ("labels", "Labels", form.labels.as_deref(), 1),
                ] {
                    @if let Some(value) = submitted {
                        label { (caption)
                            @if retry.is_some() && rows == 1 && !value.contains(['\n', '\r']) {
                                input type="text" name=(name) value=(value);
                            } @else {
                                textarea name=(name) readonly[retry.is_none()] rows=(rows) { "\n" (value) }
                            }
                        }
                    }
                }
                @for (name, caption, submitted, options) in choices {
                    @if let Some(raw) = submitted {
                        @let value = match (name, raw) {
                            ("finding_closed", "on") => "true",
                            ("finding_closed", "off") => "false",
                            _ => raw,
                        };
                        label { (caption)
                            @if retry.is_some() {
                                select name=(name) {
                                    @if !options.contains(&value) { option value=(value) selected { (value) } }
                                    @for option in options { option value=(option) selected[*option == value] {
                                        @match *option {
                                            "true" => { "Yes" }, "false" => { "No" }, choice => { (super::label(choice)) }
                                        }
                                    } }
                                }
                            } @else { textarea readonly rows="1" { "\n" (value) } }
                        }
                    }
                }
                @if retry.is_some() {
                    button.primary type="submit" { "Save reviewed changes" }
                }
            }
            @if saved.is_some() { p { a href=(format!("/issues/{id}")) { "Return to saved issue" } } }
        }
    }
}

async fn apply_in(
    tx: &mut Tx<'_>,
    owner: Uuid,
    issue: Uuid,
    action: &str,
    form: &WriteForm,
) -> AppResult<()> {
    if !matches!(
        action,
        "edit" | "status" | "checkpoint" | "archive" | "undo" | "blocker"
    ) {
        return Err(AppError::NotFound);
    }
    let hash = fingerprint(form)?;
    lock_owner(tx, owner).await?;
    if let Some(prior) = receipt(tx, owner, form.command_id).await? {
        if prior.issue_id != issue {
            return Err(conflict("This command belongs to another issue."));
        }
        return prior.verify(action, &hash);
    }
    let project: Uuid =
        sqlx::query_scalar("SELECT project_id FROM issues WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(issue)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(AppError::NotFound)?;
    store::lock_project(tx, owner, project).await?;
    let row: Stored = sqlx::query_as(
        "SELECT * FROM issues WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(owner)
    .bind(project)
    .bind(issue)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if row.revision
        != form
            .revision
            .ok_or_else(|| bad("The issue revision is required."))?
    {
        return Err(conflict(
            "Another edit changed this issue. Review the latest revision before applying your changes.",
        ));
    }
    if row.content.archived && !matches!(action, "archive" | "undo") {
        return Err(conflict("Restore the archived issue before editing it."));
    }
    let now: Time = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    let mut next = row.content.clone();
    let mut relation = None;
    let mut reverses = None;
    match action {
        "edit" => next = row.content.edited(form)?,
        "status" => {
            let new_status = form
                .status
                .as_deref()
                .ok_or_else(|| bad("Choose a status."))?;
            if !valid_status(new_status) {
                return Err(bad("Choose a listed issue status."));
            }
            next.status = new_status.to_owned();
        }
        "checkpoint" => {
            if matches!(next.status.as_str(), "done" | "canceled") {
                return Err(conflict("Reopen the issue before saving a checkpoint."));
            }
            let text = form.next_action.as_deref().unwrap_or_default().trim();
            if text.is_empty() || text.chars().count() > 2_000 {
                return Err(bad(
                    "Enter a checkpoint next action between 1 and 2,000 characters.",
                ));
            }
            next.next_action = text.to_owned();
            next.checkpoint_at = Some(now);
        }
        "archive" => {
            next.archived = form
                .archived
                .ok_or_else(|| bad("Choose whether to archive the issue."))?
        }
        "blocker" => {
            let blocker_id = form.blocker_id.ok_or_else(|| bad("Choose a blocker."))?;
            let present = match form.operation.as_deref() {
                Some("add") => true,
                Some("remove") => false,
                _ => return Err(bad("Choose add or remove.")),
            };
            relation = Some(Relation {
                blocker_id,
                present,
            });
        }
        "undo" => {
            let (event_id, prior): (Uuid, sqlx::types::Json<Snapshot>) = sqlx::query_as(
                "SELECT id,before_state FROM issue_events WHERE owner_id=$1 AND issue_id=$2
                 AND project_id=$3 AND revision_after=$4 AND before_state IS NOT NULL
                 AND action IN ('status','edit','archive','checkpoint','blocker')
                 ORDER BY occurred_at DESC,id DESC LIMIT 1",
            )
            .bind(owner)
            .bind(issue)
            .bind(project)
            .bind(row.revision)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| conflict("The latest issue revision has no change to undo."))?;
            next = restore(&next, &prior)?;
            relation = prior.relation;
            reverses = Some(event_id);
        }
        _ => return Err(AppError::NotFound),
    }
    let (mut before, mut after) = changes(&row.content, &next)?;
    if let Some(proposed) = relation
        && set_blocker(tx, owner, project, issue, proposed).await?
    {
        before.relation = Some(Relation {
            present: !proposed.present,
            ..proposed
        });
        after.relation = Some(proposed);
    }
    let changed = !after.fields.is_empty() || after.relation.is_some();
    after.command_hash = Some(hash);
    if changed {
        store::check_storage(tx, owner, next.billed_bytes() - row.content.billed_bytes()).await?;
        let (started, completed) = changed_times(
            Some(&row.content.status),
            &next.status,
            row.started_at,
            row.completed_at,
            now,
        );
        sqlx::query(
            "UPDATE issues SET title=$4, body=$5, status=$6, priority=$7, kind=$8, labels=$9,
             next_action=$10, manual_severity=$11, finding_closed=$12, security_critical=$13,
             archived=$14, checkpoint_at=$15, started_at=$16, completed_at=$17,
             updated_at=$18, revision=revision+1 WHERE owner_id=$1 AND project_id=$2 AND id=$3",
        )
        .bind(owner)
        .bind(project)
        .bind(issue)
        .bind(&next.title)
        .bind(&next.body)
        .bind(&next.status)
        .bind(&next.priority)
        .bind(&next.kind)
        .bind(&next.labels)
        .bind(&next.next_action)
        .bind(&next.manual_severity)
        .bind(next.finding_closed)
        .bind(next.open_critical())
        .bind(next.archived)
        .bind(next.checkpoint_at)
        .bind(started)
        .bind(completed)
        .bind(now)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO issue_events (owner_id,project_id,issue_id,old_status,new_status,action,
         command_id,before_state,after_state,revision_after,reverses,occurred_at,ingested_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$12)",
    )
    .bind(owner)
    .bind(project)
    .bind(issue)
    .bind(&row.content.status)
    .bind(&next.status)
    .bind(action)
    .bind(form.command_id)
    .bind(changed.then_some(sqlx::types::Json(before)))
    .bind(sqlx::types::Json(after))
    .bind(changed.then_some(row.revision + 1))
    .bind(reverses)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn draft() -> NewIssue {
        NewIssue {
            project: Uuid::new_v4(),
            title: " Fix index ".to_owned(),
            body: "é".to_owned(),
            status: "ready".to_owned(),
            priority: "normal".to_owned(),
            kind: "maintenance".to_owned(),
            labels: " data, index , data ".to_owned(),
            next_action: String::new(),
            severity: "none".to_owned(),
        }
    }

    #[test]
    fn first_start_and_latest_completion_follow_effective_transition() {
        let start = Utc::now();
        let first_end = start + Duration::hours(2);
        let reopened = first_end + Duration::hours(1);
        let latest_end = reopened + Duration::hours(1);
        let (began, done) = changed_times(None, "in_progress", None, None, start);
        assert_eq!((began, done), (Some(start), None));
        let (began, done) = changed_times(Some("in_progress"), "done", began, done, first_end);
        assert_eq!((began, done), (Some(start), Some(first_end)));
        let (began, done) = changed_times(Some("done"), "in_progress", began, done, reopened);
        assert_eq!((began, done), (Some(start), None));
        let (began, done) = changed_times(Some("in_progress"), "done", began, done, latest_end);
        assert_eq!((began, done), (Some(start), Some(latest_end)));
        assert_eq!(
            changed_times(None, "done", None, None, latest_end),
            (None, Some(latest_end))
        );
        assert_eq!(
            changed_times(
                Some("done"),
                "done",
                began,
                done,
                latest_end + Duration::days(1)
            ),
            (began, done)
        );
    }

    #[test]
    fn revision_only_retains_changed_fields_and_can_restore_them() {
        let old = Content::from_draft(draft(), false).unwrap();
        let mut new = old.clone();
        new.status = "in_progress".into();
        new.title = "Rewrite the index".into();
        let (before, after) = changes(&old, &new).unwrap();
        assert_eq!(before.fields.len(), 2);
        assert!(!after.fields.contains_key("body"));
        assert_eq!(restore(&new, &before).unwrap(), old);
        assert_eq!(restore(&old, &after).unwrap(), new);
    }

    #[test]
    fn manual_critical_can_close_independently_and_byte_count_is_utf8() {
        let mut data = draft();
        assert!(data.validate().is_ok());
        data.severity = "critical".into();
        assert!(data.validate().is_err());
        data.next_action = "Review dependency".into();
        data.status = "done".into();
        let open = Content::from_draft(data.clone(), false).unwrap();
        assert!(open.open_critical());
        assert_eq!(
            open.billed_bytes(),
            ("Fix index".len()
                + "é".len()
                + "Review dependency".len()
                + "data".len()
                + "index".len()) as i64
        );
        assert_eq!(open.labels, vec!["data", "index"]);
        assert!(!Content::from_draft(data, true).unwrap().open_critical());
    }

    #[test]
    fn partial_edit_preserves_work_and_independent_finding_closure() {
        let mut data = draft();
        data.severity = "critical".into();
        data.next_action = "Investigate".into();
        data.status = "done".into();
        let previous = Content::from_draft(data, true).unwrap();
        let form: WriteForm = serde_json::from_value(serde_json::json!({
            "csrf":"", "command_id":Uuid::new_v4(), "title":"Updated title"
        }))
        .unwrap();
        let current = previous.edited(&form).unwrap();
        assert_eq!(current.status, "done");
        assert!(current.finding_closed);
        assert_eq!(current.body, previous.body);
        assert_eq!(current.labels, previous.labels);
    }

    #[test]
    fn retry_fingerprint_ignores_csrf_but_distinguishes_submitted_changes() {
        let command = Uuid::new_v4();
        let form = |token: &str, status: &str| {
            serde_json::from_value::<WriteForm>(serde_json::json!({
                "csrf": token, "command_id": command, "revision": 2, "status": status
            }))
            .unwrap()
        };
        assert_eq!(
            fingerprint(&form("first", "ready")).unwrap(),
            fingerprint(&form("rotated", "ready")).unwrap()
        );
        assert_ne!(
            fingerprint(&form("first", "ready")).unwrap(),
            fingerprint(&form("first", "done")).unwrap()
        );
    }

    #[sqlx::test]
    async fn conflict_review_compares_owned_revision_and_rechecks_reviewed_retry(
        db: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('compare@example.invalid') RETURNING id",
        )
        .fetch_one(&db)
        .await?;
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,key,name) VALUES($1,'CMP','Comparison') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await?;
        let state = AppState::for_test(db.clone());
        let auth = Auth {
            user_id: owner,
            email: "compare@example.invalid".into(),
            csrf: "a".repeat(64),
        };
        let mut tx = db.begin().await?;
        let create = Uuid::new_v4();
        let issue = create_in(
            &mut tx,
            owner,
            create,
            NewIssue {
                project,
                title: "Saved title <ours>".into(),
                body: "Saved body".into(),
                ..draft()
            },
        )
        .await?;
        tx.commit().await?;
        let submitted = |command: Uuid, revision: i64| {
            serde_json::from_value::<WriteForm>(serde_json::json!({
            "csrf":auth.csrf, "command_id":command,"revision":revision,"title":"Draft <mine>", "body":"\nDraft body", "priority":"high"
        })).unwrap()
        };
        let edit = |id, form| {
            update(
                State(state.clone()),
                auth.clone(),
                Path((id, "edit".into())),
                Form(form),
            )
        };
        let body = |response: Response| async move {
            Ok::<_, anyhow::Error>(String::from_utf8(
                axum::body::to_bytes(response.into_body(), 200_000)
                    .await?
                    .to_vec(),
            )?)
        };
        let page = edit(issue, submitted(Uuid::new_v4(), 0)).await?;
        assert_eq!(page.status(), StatusCode::CONFLICT);
        let html = body(page).await?;
        let saved = html
            .split_once("Compare saved version")
            .context("comparison disclosure")?
            .1
            .split_once("</details>")
            .context("end of comparison")?
            .0;
        assert!(
            saved.contains("CMP-1")
                && saved.contains("Revision 1")
                && saved.contains("Saved title &lt;ours&gt;")
                && saved.contains("Saved body")
        );
        let draft_html = html.split_once("Your draft").context("preserved draft")?.1;
        assert!(draft_html.contains("Draft &lt;mine&gt;") && draft_html.contains("\n\nDraft body"));
        assert!(
            draft_html.contains("Save reviewed changes")
                && draft_html.contains("name=\"revision\" value=\"1\"")
        );
        let retry_id = draft_html
            .split_once("name=\"command_id\" value=\"")
            .context("review command")?
            .1
            .split('"')
            .next()
            .unwrap()
            .parse()?;

        // A new edit arriving during comparison must leave the reviewed draft intact again.
        let change = serde_json::from_value(
            serde_json::json!({"csrf":auth.csrf,"command_id":Uuid::new_v4(),"revision":1,"title":"Another saved title"}),
        )?;
        assert_eq!(edit(issue, change).await?.status(), StatusCode::SEE_OTHER);
        let second = edit(issue, submitted(retry_id, 1)).await?;
        assert_eq!(second.status(), StatusCode::CONFLICT);
        let second = body(second).await?;
        assert!(
            second.contains("Revision 2")
                && second.contains("Another saved title")
                && second.contains("Draft &lt;mine&gt;")
        );
        assert_eq!(
            edit(issue, submitted(Uuid::new_v4(), 2)).await?.status(),
            StatusCode::SEE_OTHER
        );
        let result: (String, i64) =
            sqlx::query_as("SELECT title,revision FROM issues WHERE owner_id=$1 AND id=$2")
                .bind(owner)
                .bind(issue)
                .fetch_one(&db)
                .await?;
        assert_eq!(result, ("Draft <mine>".into(), 3));

        sqlx::query("UPDATE projects SET archived=true WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(project)
            .execute(&db)
            .await?;
        let archived = body(edit(issue, submitted(Uuid::new_v4(), 3)).await?).await?;
        assert!(
            archived.contains("Archived · Read-only")
                && archived.contains("readonly")
                && !archived.contains("Save reviewed changes")
        );

        // An owned command receipt can conflict before the target issue is resolved.
        let other: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('outside@example.invalid') RETURNING id",
        )
        .fetch_one(&db)
        .await?;
        let their_project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,key,name) VALUES($1,'OTH','Other') RETURNING id",
        )
        .bind(other)
        .fetch_one(&db)
        .await?;
        let mut tx = db.begin().await?;
        let foreign = create_in(
            &mut tx,
            other,
            Uuid::new_v4(),
            NewIssue {
                project: their_project,
                title: "Other private title".into(),
                ..draft()
            },
        )
        .await?;
        tx.commit().await?;
        let unknown = body(edit(foreign, submitted(create, 3)).await?).await?;
        assert!(
            unknown.contains("saved version is unavailable")
                && unknown.contains("Draft &lt;mine&gt;")
        );
        assert!(
            !unknown.contains("Other private title") && !unknown.contains("Save reviewed changes")
        );
        Ok(())
    }

    #[sqlx::test]
    async fn first_checkpoint_records_only_explicit_open_issue_form_actions(
        db: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('resume@example.invalid') RETURNING id",
        )
        .fetch_one(&db)
        .await?;
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,key,name) VALUES($1,'RES','Resume') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await?;
        for (source, status, action, expected) in [
            (CreateSource::IssueForm, "backlog", " Plan upgrade ", true),
            (CreateSource::IssueForm, "ready", "Start upgrade", true),
            (CreateSource::IssueForm, "in_progress", "Test upgrade", true),
            (CreateSource::IssueForm, "ready", "  ", false),
            (CreateSource::IssueForm, "done", "Release notes", false),
            (
                CreateSource::IssueForm,
                "canceled",
                "Investigate later",
                false,
            ),
            (
                CreateSource::LinkedWorkflow,
                "in_progress",
                "Linked next action",
                false,
            ),
        ] {
            let draft = NewIssue {
                project,
                status: status.into(),
                next_action: action.into(),
                ..draft()
            };
            let command = Uuid::new_v4();
            let mut tx = db.begin().await?;
            let id = insert_issue(&mut tx, owner, command, draft.clone(), source).await?;
            tx.commit().await?;
            let mut retry = db.begin().await?;
            assert_eq!(
                insert_issue(&mut retry, owner, command, draft, source).await?,
                id
            );
            retry.commit().await?;
            let rows: Vec<(Option<Time>,Option<Time>,Time,i64)> = sqlx::query_as(
                "SELECT i.checkpoint_at,(e.after_state->'fields'->>'checkpoint_at')::timestamptz,
                 e.occurred_at,i.revision FROM issues i JOIN issue_events e ON i.id=e.issue_id WHERE i.id=$1"
            ).bind(id).fetch_all(&db).await?;
            assert_eq!(rows.len(), 1);
            let (checkpoint, snapshot, occurred, revision) = rows[0];
            assert_eq!(checkpoint, expected.then_some(occurred));
            assert_eq!(snapshot, checkpoint);
            assert_eq!(revision, 1);
        }
        Ok(())
    }

    #[sqlx::test]
    async fn undoing_a_removed_blocker_rechecks_new_dependencies(
        db: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('undo@example.invalid') RETURNING id",
        )
        .fetch_one(&db)
        .await?;
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,key,name) VALUES($1,'UNDO','Undo') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await?;
        let mut tx = db.begin().await?;
        let template = NewIssue { project, ..draft() };
        let a = create_in(&mut tx, owner, Uuid::new_v4(), template.clone()).await?;
        let b = create_in(&mut tx, owner, Uuid::new_v4(), template.clone()).await?;
        let c = create_in(&mut tx, owner, Uuid::new_v4(), template).await?;
        tx.commit().await?;
        for (issue, blocker, revision, operation) in [
            (a, b, 1, "add"),
            (a, b, 2, "remove"),
            (b, c, 1, "add"),
            (c, a, 1, "add"),
        ] {
            let form = serde_json::from_value(
                serde_json::json!({"csrf":"","command_id":Uuid::new_v4(),"revision":revision, "blocker_id":blocker,"operation":operation}),
            )?;
            let mut tx = db.begin().await?;
            apply_in(&mut tx, owner, issue, "blocker", &form).await?;
            tx.commit().await?;
        }
        let undo = serde_json::from_value(
            serde_json::json!({"csrf":"","command_id":Uuid::new_v4(),"revision":3}),
        )?;
        let mut tx = db.begin().await?;
        assert!(matches!(
            apply_in(&mut tx, owner, a, "undo", &undo).await,
            Err(AppError::Conflict(_))
        ));
        tx.rollback().await?;
        let edges: i64 =
            sqlx::query_scalar("SELECT count(*) FROM issue_blockers WHERE owner_id=$1")
                .bind(owner)
                .fetch_one(&db)
                .await?;
        let revision: i64 = sqlx::query_scalar("SELECT revision FROM issues WHERE id=$1")
            .bind(a)
            .fetch_one(&db)
            .await?;
        assert_eq!((edges, revision), (2, 3));
        Ok(())
    }
}
