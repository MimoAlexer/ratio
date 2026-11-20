//! Native issue documents keep the editable heading and description in Markdown.
//! The manifest carries the structured issue fields and a common ancestor for edits.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres, Transaction};
use uuid::Uuid;

use super::{Action, Mode, archive::ManifestFile, hash};
use crate::{AppError, AppResult};

#[derive(Clone, Debug, Deserialize, FromRow, Serialize)]
struct Fields {
    number: i64,
    title: String,
    status: String,
    priority: String,
    kind: String,
    labels: Vec<String>,
    manual_severity: String,
    finding_closed: bool,
    archived: bool,
    next_action: String,
    checkpoint_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    revision: i64,
}

#[derive(FromRow)]
struct StoredIssue {
    id: Uuid,
    body: String,
    #[sqlx(flatten)]
    fields: Fields,
}

#[derive(Deserialize, Serialize)]
struct ManifestData {
    #[serde(flatten)]
    fields: Fields,
    base_state_sha256: String,
}

const SELECT_ISSUE: &str = "
    SELECT id, body, number, title, status, priority, kind, labels, manual_severity,
           finding_closed, archived, next_action, checkpoint_at, started_at, completed_at,
           created_at, updated_at, revision
      FROM issues WHERE owner_id=$1 AND project_id=$2";

fn bad(message: &str) -> AppError {
    AppError::BadRequest(format!("Invalid issue in the archive. {message}"))
}

fn to_json<T: Serialize>(value: &T) -> AppResult<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|_| AppError::Internal("Could not encode issue fields.".into()))
}

fn heading(title: &str) -> String {
    title.replace(['\r', '\n'], " ")
}

fn markdown(title: &str, body: &str) -> Vec<u8> {
    format!("# {}\n\n{body}", heading(title)).into_bytes()
}

// A new destination may assign a different issue number and revision. Local updates also
// advance updated_at. These are identities/versions, not edits to the portable content.
fn fingerprint(fields: &Fields, body: &str) -> AppResult<String> {
    let mut value = to_json(fields)?;
    let map = value
        .as_object_mut()
        .expect("Fields serializes as an object");
    for name in ["number", "revision", "created_at", "updated_at"] {
        map.remove(name);
    }
    map.insert(
        "body_sha256".into(),
        serde_json::Value::String(hash(body.as_bytes())),
    );
    Ok(hash(value.to_string().as_bytes()))
}

fn export_issue(issue: StoredIssue) -> AppResult<(ManifestFile, Vec<u8>)> {
    let bytes = markdown(&issue.fields.title, &issue.body);
    let metadata = ManifestData {
        base_state_sha256: fingerprint(&issue.fields, &issue.body)?,
        fields: issue.fields,
    };
    Ok((
        ManifestFile {
            id: issue.id,
            path: format!("Issues/{}.md", metadata.fields.number),
            kind: "issue".to_owned(),
            sha256: hash(&bytes),
            data: to_json(&metadata)?,
        },
        bytes,
    ))
}

fn checkpoint(project_key: &str, issue: &StoredIssue) -> Option<(String, Vec<u8>)> {
    let f = &issue.fields;
    let saved = f.checkpoint_at?;
    let number = f.number;
    let text = format!(
        "# {project_key}-{number} · {title}\n\nSaved {date}.\n\n[Open issue {project_key}-{number}](../Issues/{number}.md)\n\n## Next action\n\n{action}\n",
        title = heading(&f.title),
        date = saved.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        action = f.next_action,
    );
    Some((format!("Checkpoints/{number}.md"), text.into_bytes()))
}

pub(super) async fn stream(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    project: Uuid,
    sink: &super::export::Sink,
) -> AppResult<()> {
    use futures_util::TryStreamExt;
    let project_key: String =
        sqlx::query_scalar("SELECT key FROM projects WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(project)
            .fetch_one(&mut **tx)
            .await?;
    let query = format!("{SELECT_ISSUE} ORDER BY number");
    let mut rows = sqlx::query_as::<_, StoredIssue>(&query)
        .bind(owner)
        .bind(project)
        .fetch(&mut **tx);
    while let Some(issue) = rows.try_next().await? {
        if let Some((path, bytes)) = checkpoint(&project_key, &issue) {
            sink.reading_copy(path, bytes).await?;
        }
        let (file, bytes) = export_issue(issue)?;
        sink.file(file, bytes).await?;
    }
    Ok(())
}

#[cfg(test)]
pub async fn export(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    project: Uuid,
) -> AppResult<Vec<(ManifestFile, Vec<u8>)>> {
    let query = format!("{SELECT_ISSUE} ORDER BY number");
    sqlx::query_as::<_, StoredIssue>(&query)
        .bind(owner)
        .bind(project)
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(export_issue)
        .collect()
}

fn decode<'a>(file: &ManifestFile, bytes: &'a [u8]) -> AppResult<(ManifestData, &'a str)> {
    let mut data: ManifestData = serde_json::from_value(file.data.clone())
        .map_err(|_| bad("Required manifest fields are missing or invalid."))?;
    if data.base_state_sha256.len() != 64
        || !data
            .base_state_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(bad("Invalid base checksum."));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| bad("Markdown must be UTF-8."))?;
    let (first, rest) = source
        .split_once('\n')
        .ok_or_else(|| bad("Missing Markdown title."))?;
    let title = first
        .trim_end_matches('\r')
        .strip_prefix("# ")
        .ok_or_else(|| bad("Start the Markdown file with a level-one title."))?;
    let body = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .ok_or_else(|| bad("Put a blank line after the Markdown title."))?;
    // Editing the heading updates the title. For unmodified Markdown, the manifest's
    // title can itself be edited, and can losslessly retain an original multiline title.
    if hash(bytes) != file.sha256 && title != heading(&data.fields.title) {
        data.fields.title = title.to_owned();
    }
    validate(&data.fields, body)?;
    Ok((data, body))
}

fn validate(f: &Fields, body: &str) -> AppResult<()> {
    if f.number <= 0 || f.revision <= 0 || f.number >= i64::MAX / 2 || f.revision >= i64::MAX / 2 {
        return Err(bad(
            "Issue number and revision must be positive supported integers.",
        ));
    }
    if !["backlog", "ready", "in_progress", "done", "canceled"].contains(&f.status.as_str())
        || !["low", "normal", "high", "urgent"].contains(&f.priority.as_str())
        || !["feature", "bug", "maintenance", "security"].contains(&f.kind.as_str())
        || !["none", "low", "medium", "high", "critical"].contains(&f.manual_severity.as_str())
    {
        return Err(bad(
            "Unrecognized status, priority, kind or manual severity.",
        ));
    }
    if f.title.trim().is_empty()
        || f.title.chars().count() > 240
        || f.next_action.chars().count() > 2_000
        || body.len() > 2 * 1024 * 1024
        || (f.manual_severity != "none" && f.next_action.trim().is_empty())
    {
        return Err(bad(
            "Check title, description and next-action limits. A manual finding requires a next action.",
        ));
    }
    if f.labels.len() > 24
        || f.labels
            .iter()
            .any(|label| label.chars().count() > 48 || label.trim().is_empty())
        || f.labels.iter().collect::<HashSet<_>>().len() != f.labels.len()
    {
        return Err(bad("Use at most 24 distinct labels of 1–48 characters."));
    }
    if std::iter::once(f.title.as_str())
        .chain([body, &f.next_action])
        .chain(f.labels.iter().map(String::as_str))
        .any(|s| s.contains('\0'))
    {
        return Err(bad("Issue text contains a NUL character."));
    }
    Ok(())
}

fn classify(current: Option<&str>, incoming: &str, base: &str) -> Action {
    match current {
        None => Action::Create,
        Some(current) if current == incoming || incoming == base => Action::Unchanged,
        Some(current) if current == base => Action::Update,
        Some(_) => Action::Conflict,
    }
}

pub async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    project: Uuid,
    file: &ManifestFile,
    bytes: &[u8],
    mode: Mode,
) -> AppResult<(Action, Uuid)> {
    let (data, body) = decode(file, bytes)?;
    let mapping: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT local_id, base_hash FROM transfer_records
           WHERE owner_id=$1 AND project_id=$2 AND kind='issue' AND source_id=$3",
    )
    .bind(owner)
    .bind(project)
    .bind(file.id)
    .fetch_optional(&mut **tx)
    .await?;
    let candidate = mapping.as_ref().map_or(file.id, |(id, _)| *id);
    let query = format!("{SELECT_ISSUE} AND id=$3");
    let current = sqlx::query_as::<_, StoredIssue>(&query)
        .bind(owner)
        .bind(project)
        .bind(candidate)
        .fetch_optional(&mut **tx)
        .await?;
    let current_hash = current
        .as_ref()
        .map(|r| fingerprint(&r.fields, &r.body))
        .transpose()?;
    let incoming_hash = fingerprint(&data.fields, body)?;
    let remote_base = &data.base_state_sha256;
    let base_hash = match &mapping {
        // This is a fresh export of the local record. A previous export may have had
        // a different base. If either copy still matches this archive's base,
        // leave edits to the other copy intact.
        _ if candidate == file.id
            && (current_hash.as_ref() == Some(remote_base) || incoming_hash == *remote_base) =>
        {
            remote_base
        }
        Some((_, accepted)) => accepted,
        None => remote_base,
    };
    let action = classify(current_hash.as_deref(), &incoming_hash, base_hash);
    let local_id = current.as_ref().map_or_else(Uuid::new_v4, |r| r.id);
    if matches!(mode, Mode::Preview) {
        return Ok((action, local_id));
    }
    let fields = to_json(&data.fields)?;
    match action {
        Action::Create => {
            create(tx, owner, project, local_id, &fields, body).await?;
        }
        Action::Update => {
            update(tx, owner, project, local_id, &fields, body).await?;
        }
        Action::Conflict if matches!(mode, Mode::UseIncoming) => {
            update(tx, owner, project, local_id, &fields, body).await?;
        }
        _ => {}
    }
    sqlx::query(
        "INSERT INTO transfer_records(owner_id,project_id,kind,source_id,local_id,base_hash,path)
         VALUES($1,$2,'issue',$3,$4,$5,$6)
         ON CONFLICT(project_id,kind,source_id) DO UPDATE
         SET local_id=EXCLUDED.local_id,base_hash=EXCLUDED.base_hash,path=EXCLUDED.path
         WHERE transfer_records.owner_id=EXCLUDED.owner_id
           AND (transfer_records.local_id,transfer_records.base_hash,transfer_records.path)
               IS DISTINCT FROM (EXCLUDED.local_id,EXCLUDED.base_hash,EXCLUDED.path)",
    )
    .bind(owner)
    .bind(project)
    .bind(file.id)
    .bind(local_id)
    .bind(incoming_hash)
    .bind(&file.path)
    .execute(&mut **tx)
    .await?;
    Ok((action, local_id))
}

async fn create(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    project: Uuid,
    id: Uuid,
    fields: &serde_json::Value,
    body: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO issues(id, owner_id, project_id, number, title, body, status, priority, kind,
             labels, manual_severity, finding_closed, security_critical, archived, next_action,
             checkpoint_at, started_at, completed_at, created_at, updated_at, revision)
         SELECT $3, $1, $2,
             CASE WHEN EXISTS (SELECT 1 FROM issues WHERE project_id=$2 AND number=f.number)
                  THEN (SELECT COALESCE(MAX(number),0)+1 FROM issues WHERE project_id=$2)
                  ELSE f.number END,
             f.title, $5, f.status, f.priority, f.kind, f.labels, f.manual_severity,
             f.finding_closed, f.manual_severity='critical' AND NOT f.finding_closed,
             f.archived, f.next_action, f.checkpoint_at, f.started_at, f.completed_at,
             f.created_at, f.updated_at, f.revision
         FROM jsonb_populate_record(NULL::issues,$4::jsonb) f",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .bind(fields)
    .bind(body)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn update(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    project: Uuid,
    id: Uuid,
    fields: &serde_json::Value,
    body: &str,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE issues i SET (title, body, status, priority, kind, labels, manual_severity,
             finding_closed, security_critical, archived, next_action, checkpoint_at,
             started_at, completed_at, updated_at, revision) =
             (f.title, $5, f.status, f.priority, f.kind, f.labels, f.manual_severity,
              f.finding_closed, f.manual_severity='critical' AND NOT f.finding_closed,
              f.archived, f.next_action, f.checkpoint_at, f.started_at, f.completed_at,
              GREATEST(f.updated_at, i.updated_at, now()), i.revision+1)
         FROM jsonb_populate_record(NULL::issues,$4::jsonb) f
         WHERE i.owner_id=$1 AND i.project_id=$2 AND i.id=$3",
    )
    .bind(owner)
    .bind(project)
    .bind(id)
    .bind(fields)
    .bind(body)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> StoredIssue {
        let date = "2026-11-14T09:20:13Z".parse().unwrap();
        StoredIssue {
            id: Uuid::new_v4(),
            fields: Fields {
                number: 44,
                title: "Investigate indexing".into(),
                status: "in_progress".into(),
                priority: "high".into(),
                kind: "bug".into(),
                labels: vec!["vault".into(), "β".into()],
                manual_severity: "none".into(),
                finding_closed: false,
                archived: false,
                next_action: "Try the second vault".into(),
                checkpoint_at: Some(date),
                started_at: Some(date),
                completed_at: None,
                created_at: date,
                updated_at: date,
                revision: 3,
            },
            body: "See [[Decisions/索引]].\r\n\r\n```rust\r\nfn foo() {}\r\n```\r\n".into(),
        }
    }

    #[test]
    fn saved_checkpoint_has_a_readable_dated_issue_link() {
        let mut issue = source();
        let (path, bytes) = checkpoint("NATIVE", &issue).unwrap();
        assert_eq!(path, "Checkpoints/44.md");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("# NATIVE-44 · Investigate indexing"));
        assert!(text.contains("2026-11-14T09:20:13Z"));
        assert!(text.contains("(../Issues/44.md)"));
        assert!(text.contains("## Next action\n\nTry the second vault"));
        issue.fields.checkpoint_at = None;
        assert!(checkpoint("NATIVE", &issue).is_none());
    }

    #[test]
    fn native_markdown_preserves_description_bytes_and_structured_fields() {
        let issue = source();
        let body = issue.body.clone();
        let expected = fingerprint(&issue.fields, &body).unwrap();
        let (file, bytes) = export_issue(issue).unwrap();
        assert_eq!(file.path, "Issues/44.md");
        assert_eq!(file.sha256, hash(&bytes));
        let (data, restored) = decode(&file, &bytes).unwrap();
        assert_eq!(data.base_state_sha256, expected);
        assert_eq!(restored, body);
        assert_eq!(data.fields.revision, 3);
        let mut local = data.fields.clone();
        local.number += 1;
        local.revision += 1;
        local.updated_at = Utc::now();
        assert_eq!(fingerprint(&local, restored).unwrap(), expected);
        local.labels.clear();
        assert_ne!(fingerprint(&local, restored).unwrap(), expected);
    }

    #[test]
    fn edits_and_conflicts_distinguish_local_remote_and_shared_changes() {
        let issue = source();
        let (mut file, bytes) = export_issue(issue).unwrap();
        let edited = b"# Revised heading\r\n\r\nDifferent text\r\n";
        let (data, body) = decode(&file, edited).unwrap();
        assert_eq!(data.fields.title, "Revised heading");
        assert_eq!(body, "Different text\r\n");
        let changed = fingerprint(&data.fields, body).unwrap();
        let base = &data.base_state_sha256;
        assert!(matches!(classify(None, &changed, base), Action::Create));
        assert!(matches!(
            classify(Some(base), &changed, base),
            Action::Update
        ));
        assert!(matches!(
            classify(Some("local change"), base, base),
            Action::Unchanged
        ));
        assert!(matches!(
            classify(Some("local change"), &changed, base),
            Action::Conflict
        ));
        assert!(matches!(
            classify(Some(&changed), &changed, base),
            Action::Unchanged
        ));
        file.data["title"] = "Title in metadata".into();
        assert_eq!(
            decode(&file, &bytes).unwrap().0.fields.title,
            "Title in metadata"
        );
        file.data["priority"] = "unknown".into();
        assert!(decode(&file, &bytes).is_err());
    }

    #[sqlx::test]
    async fn native_import_remaps_identity_deduplicates_and_preserves_local_edits(
        db: sqlx::PgPool,
    ) {
        let mut tx = db.begin().await.unwrap();
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('native-issue-test@example.test') RETURNING id",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,key,name) VALUES($1,'TEST','Restore') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let (file, bytes) = export_issue(source()).unwrap();
        let (first, id) = apply(&mut tx, owner, project, &file, &bytes, Mode::KeepLocal)
            .await
            .unwrap();
        assert!(matches!(first, Action::Create));
        assert_ne!(id, file.id);
        let (again, id2) = apply(&mut tx, owner, project, &file, &bytes, Mode::KeepLocal)
            .await
            .unwrap();
        assert!(matches!(again, Action::Unchanged));
        assert_eq!(id, id2);
        let exports = export(&mut tx, owner, project).await.unwrap();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].1, bytes);
        assert_eq!(exports[0].0.data, file.data);
        sqlx::query("UPDATE issues SET body='Locally changed' WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let edited = b"# Investigate indexing\n\nRemotely changed";
        assert!(matches!(
            apply(&mut tx, owner, project, &file, edited, Mode::Preview)
                .await
                .unwrap()
                .0,
            Action::Conflict
        ));
        assert!(matches!(
            apply(&mut tx, owner, project, &file, edited, Mode::KeepLocal)
                .await
                .unwrap()
                .0,
            Action::Conflict
        ));
        let saved: String = sqlx::query_scalar("SELECT body FROM issues WHERE id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(saved, "Locally changed");
        assert!(matches!(
            apply(&mut tx, owner, project, &file, edited, Mode::KeepLocal)
                .await
                .unwrap()
                .0,
            Action::Unchanged
        ));

        let new_edit = b"# Reconciled issue\n\nSecond remote edit";
        let (decision, updated_id) =
            apply(&mut tx, owner, project, &file, new_edit, Mode::UseIncoming)
                .await
                .unwrap();
        assert!(matches!(decision, Action::Conflict));
        assert_eq!(updated_id, id);
        let (title, body, revision): (String, String, i64) =
            sqlx::query_as("SELECT title,body,revision FROM issues WHERE owner_id=$1 AND id=$2")
                .bind(owner)
                .bind(id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(
            (title.as_str(), body.as_str(), revision),
            ("Reconciled issue", "Second remote edit", 4)
        );

        // A second source can carry the same issue number. Both imported identities must survive.
        let (other, other_bytes) = export_issue(source()).unwrap();
        let (created, other_id) = apply(
            &mut tx,
            owner,
            project,
            &other,
            &other_bytes,
            Mode::KeepLocal,
        )
        .await
        .unwrap();
        assert!(matches!(created, Action::Create));
        assert_ne!(id, other_id);
        let allocated: i64 = sqlx::query_scalar("SELECT number FROM issues WHERE id=$1")
            .bind(other_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(allocated, 45);
    }
}
