use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use csv::StringRecord;
use serde_json::{Map, Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{AppError, AppResult};

const MAX_CSV: usize = 8 * 1024 * 1024;
const MAX_ROWS: usize = 2_000;

pub struct Csv {
    pub rows: Vec<LinearRow>,
    pub fields: Vec<String>,
    pub warnings: Vec<String>,
}

pub struct LinearRow {
    pub source_id: String,
    pub title: String,
    parent: Option<String>,
    // Only nonempty, understood cells participate in the merge.
    values: Map<String, Value>,
    original: Map<String, Value>,
}

fn invalid(reason: impl Into<String>) -> AppError {
    AppError::BadRequest(reason.into())
}

fn heading(value: &str) -> String {
    value
        .trim_start_matches('\u{feff}')
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-'], " ")
}

fn column(name: &str) -> Option<&'static str> {
    Some(match name {
        "id" | "issue id" => "id",
        "identifier" => "identifier",
        "title" => "title",
        "description" | "body" => "body",
        "status" | "state" => "status",
        "priority" => "priority",
        "labels" => "labels",
        "parent" | "parent issue" | "parent id" => "parent",
        "project" | "project name" => "project",
        "project id" => "project_id",
        "created" | "created at" => "created_at",
        "updated" | "updated at" => "updated_at",
        "started" | "started at" => "started_at",
        "completed" | "completed at" => "completed_at",
        "canceled" | "cancelled" | "canceled at" => "canceled_at",
        "archived" | "archived at" => "archived_at",
        "triaged" | "triaged at" => "triaged_at",
        "due date" => "due_on",
        _ => return None,
    })
}

fn date(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f%:z"))
        .or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f %z"))
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
                .iter()
                .find_map(|f| NaiveDateTime::parse_from_str(value.trim_end_matches(" UTC"), f).ok())
                .map(|dt| dt.and_utc())
        })
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
        })
}

fn status(value: &str) -> Option<&'static str> {
    Some(match heading(value).as_str() {
        "backlog" | "icebox" | "triage" => "backlog",
        "todo" | "to do" | "planned" | "ready" | "unstarted" | "open" => "ready",
        "in progress" | "in review" | "in development" | "started" | "review" | "blocked" => {
            "in_progress"
        }
        "done" | "completed" | "complete" | "closed" | "shipped" | "released" => "done",
        "canceled" | "cancelled" | "duplicate" | "won't do" | "wontfix" => "canceled",
        _ => return None,
    })
}

fn priority(value: &str) -> Option<&'static str> {
    Some(match heading(value).as_str() {
        "1" | "urgent" | "p0" => "urgent",
        "2" | "high" | "p1" => "high",
        "0" | "3" | "normal" | "medium" | "no priority" | "none" | "p2" => "normal",
        "4" | "low" | "p3" => "low",
        _ => return None,
    })
}

fn compact(value: &str, max: usize) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control) && value.chars().count() <= max
}

fn cell<'a>(
    record: &'a StringRecord,
    columns: &HashMap<&str, usize>,
    key: &str,
) -> Option<&'a str> {
    let value = record.get(*columns.get(key)?)?;
    if key == "body" {
        return (!value.is_empty()).then_some(value);
    }
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

pub fn parse(bytes: &[u8]) -> AppResult<Csv> {
    if bytes.is_empty() || bytes.len() > MAX_CSV || bytes.contains(&0) {
        return Err(invalid(
            "Choose a UTF-8 Linear CSV up to 8 MiB without NUL bytes.",
        ));
    }
    let mut reader = csv::ReaderBuilder::new().from_reader(bytes);
    let header = reader
        .headers()
        .map_err(|_| invalid("Invalid UTF-8 or CSV header."))?
        .clone();
    if header.len() > 100
        || header
            .iter()
            .any(|h| !compact(h.trim_start_matches('\u{feff}').trim(), 100))
    {
        return Err(invalid(
            "The CSV contains too many columns or an invalid header.",
        ));
    }
    let mut columns = HashMap::new();
    let mut fields = Vec::new();
    let mut omitted = Vec::new();
    for (index, h) in header.iter().enumerate() {
        if let Some(key) = column(&heading(h)) {
            if columns.insert(key, index).is_some() {
                return Err(invalid(format!("Duplicate CSV column for {h}.")));
            }
            fields.push(h.trim_start_matches('\u{feff}').trim().to_owned());
        } else {
            omitted.push(h);
        }
    }
    if !columns.contains_key("title")
        || !(columns.contains_key("id") || columns.contains_key("identifier"))
    {
        return Err(invalid(
            "Linear CSV requires an ID (or Identifier) and a Title column.",
        ));
    }

    let mut rows = Vec::new();
    let mut ids = HashSet::new();
    let mut project_ids = HashSet::new();
    let mut projects = HashSet::new();
    let mut mappings: BTreeMap<String, usize> = BTreeMap::new();
    for (index, result) in reader.records().enumerate() {
        if index >= MAX_ROWS {
            return Err(invalid(
                "Split this project CSV into batches of at most 2,000 issues.",
            ));
        }
        let record = result.map_err(|_| {
            invalid(format!(
                "Invalid UTF-8 or CSV columns at issue row {}.",
                index + 1
            ))
        })?;
        let get = |k: &str| cell(&record, &columns, k);
        let id = get("id").or_else(|| get("identifier")).unwrap_or("");
        let title = get("title").unwrap_or("");
        if !compact(id, 200) || !ids.insert(id.to_string()) {
            return Err(invalid(format!(
                "Missing, duplicated or invalid Linear ID at issue row {}.",
                index + 1
            )));
        }
        if !compact(title, 240) {
            return Err(invalid(format!("{id} needs a title of 1–240 characters.")));
        }
        let mut values = Map::new();
        let mut original = Map::new();
        values.insert("title".into(), json!(title));
        if let Some(body) = get("body") {
            if body.len() > 2 * 1024 * 1024 {
                return Err(invalid(format!("The description of {id} exceeds 2 MiB.")));
            }
            values.insert("body".into(), json!(body));
        }
        type Mapper = fn(&str) -> Option<&'static str>;
        for (name, map, fallback) in [
            ("status", status as Mapper, "backlog"),
            ("priority", priority as Mapper, "normal"),
        ] {
            if let Some(raw) = get(name) {
                if !compact(raw, 120) {
                    return Err(invalid(format!("{id} has an invalid {name}.")));
                }
                let mapped = map(raw).unwrap_or_else(|| {
                    *mappings
                        .entry(format!("Unrecognized {name} “{raw}” maps to {fallback}"))
                        .or_default() += 1;
                    fallback
                });
                values.insert(name.into(), json!(mapped));
                original.insert(name.into(), json!(raw));
            }
        }
        if let Some(raw) = get("labels") {
            let mut labels = Vec::new();
            for label in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
            if labels.len() > 24 || labels.iter().any(|s| !compact(s, 48)) {
                return Err(invalid(format!(
                    "{id} exceeds the limit of 24 labels of up to 48 characters each."
                )));
            }
            if !labels.is_empty() {
                values.insert("labels".into(), json!(labels));
            }
        }

        for key in [
            "created_at",
            "updated_at",
            "started_at",
            "completed_at",
            "canceled_at",
            "archived_at",
            "triaged_at",
            "due_on",
        ] {
            if let Some(raw) = get(key) {
                let stamp =
                    date(raw).ok_or_else(|| invalid(format!("Invalid {key} date on {id}.")))?;
                // PostgreSQL timestamps have microsecond precision.
                let canonical = stamp.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
                original.insert(key.into(), json!(raw));
                match key {
                    "created_at" | "updated_at" | "started_at" | "completed_at" => {
                        values.insert(key.into(), json!(canonical));
                    }
                    "archived_at" => {
                        values.insert("archived".into(), Value::Bool(true));
                    }
                    _ => (),
                }
            }
        }
        let parent = get("parent").map(str::to_string);
        if parent
            .as_deref()
            .is_some_and(|p| !compact(p, 200) || p == id)
        {
            return Err(invalid(format!(
                "{id} contains an invalid parent reference."
            )));
        }
        for (key, distinct) in [("project_id", &mut project_ids), ("project", &mut projects)] {
            if columns.contains_key(key) {
                let value = get(key).unwrap_or("");
                if !value.is_empty() && !compact(value, 240) {
                    return Err(invalid(format!("{id} has an invalid source project.")));
                }
                distinct.insert(value.to_owned());
                if !value.is_empty() {
                    original.insert(key.into(), json!(value));
                }
            }
        }
        if let Some(alias) = get("identifier") {
            if !compact(alias, 200) {
                return Err(invalid(format!("{id} has an invalid issue identifier.")));
            }
            original.insert("identifier".into(), json!(alias));
        }
        rows.push(LinearRow {
            source_id: id.into(),
            title: title.into(),
            parent,
            values,
            original,
        });
    }
    if rows.is_empty() {
        return Err(invalid("The Linear CSV has no issue rows."));
    }
    if project_ids.len() > 1 || projects.len() > 1 {
        return Err(invalid(
            "This CSV includes multiple Linear projects. Export one project at a time.",
        ));
    }
    // Some exporters include both a UUID and a human-readable identifier. Resolve
    // either spelling to the persisted source ID; reject ambiguous aliases.
    let mut aliases = HashMap::new();
    for row in &rows {
        if let Some(alias) = row.original.get("identifier").and_then(Value::as_str)
            && alias != row.source_id
            && (ids.contains(alias)
                || aliases
                    .insert(alias.to_owned(), row.source_id.clone())
                    .is_some())
        {
            return Err(invalid("The CSV contains ambiguous issue identifiers."));
        }
    }
    for row in &mut rows {
        if let Some(mapped) = row.parent.as_ref().and_then(|p| aliases.get(p)) {
            row.parent = Some(mapped.clone());
        }
    }
    let mut warnings = vec!["Comments, documents and attachment files are omitted. Links in descriptions are preserved. Import records the current state at confirmation.".into()];
    if !omitted.is_empty() {
        warnings.push(format!("Unmapped CSV columns: {}.", omitted.join(", ")));
    }
    warnings.extend(
        mappings
            .into_iter()
            .map(|(msg, n)| format!("{msg} ({n} issues).")),
    );
    Ok(Csv {
        rows,
        fields,
        warnings,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Preview,
    KeepLocal,
    UseIncoming,
}

#[derive(Default)]
pub struct Preview {
    pub rows: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub conflicts: usize,
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
}

struct Merge {
    values: Map<String, Value>,
    base: Value,
    changed: Vec<String>,
    conflicts: Vec<String>,
}

fn value_hash(value: &Value) -> String {
    super::hash(value.to_string().as_bytes())
}

fn merge(base: &Value, current: Map<String, Value>, row: &LinearRow, mode: Mode) -> Merge {
    let mut hashes = base["hashes"].as_object().cloned().unwrap_or_default();
    let mut original = base["original"].as_object().cloned().unwrap_or_default();
    original.extend(row.original.clone());
    let mut values = current;
    let mut changed = Vec::new();
    let mut conflicts = Vec::new();
    for (key, incoming) in &row.values {
        let current = values.get(key).unwrap_or(&Value::Null);
        let source_hash = value_hash(incoming);
        let previous = hashes.get(key).and_then(Value::as_str);
        let source_changed = previous != Some(&source_hash);
        let local_changed = previous != Some(&value_hash(current));
        if current != incoming && source_changed {
            if local_changed {
                conflicts.push(key.clone());
            }
            if !local_changed || mode == Mode::UseIncoming {
                changed.push(key.clone());
                values.insert(key.clone(), incoming.clone());
            }
        }
        hashes.insert(key.clone(), json!(source_hash));
    }
    Merge {
        values,
        base: json!({"hashes":hashes,"original":original}),
        changed,
        conflicts,
    }
}

// Use the same UTC microsecond representation as PostgreSQL, independent of the
// database session timezone and the original CSV timestamp spelling.
fn canonical_state(raw: Value) -> AppResult<Map<String, Value>> {
    let mut object = raw
        .as_object()
        .cloned()
        .ok_or_else(|| AppError::Internal("Invalid Linear issue projection.".into()))?;
    for key in ["created_at", "updated_at", "started_at", "completed_at"] {
        if let Some(raw) = object.get(key).and_then(Value::as_str) {
            let time = date(raw)
                .ok_or_else(|| AppError::Internal("Invalid issue timestamp in storage.".into()))?;
            object.insert(
                key.into(),
                json!(time.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)),
            );
        }
    }
    Ok(object)
}

fn initial_base(current: &Map<String, Value>, row: &LinearRow) -> Value {
    let hashes: BTreeMap<_, _> = current.iter().map(|(k, v)| (k, value_hash(v))).collect();
    json!({"hashes":hashes,"original":row.original})
}

#[derive(Clone)]
struct Relation {
    issue: Uuid,
    parent_source: Option<String>,
    parent_id: Option<Uuid>,
    alias: Option<String>,
}

fn check_cycles(parents: &HashMap<Uuid, Uuid>) -> AppResult<()> {
    let mut finished = HashSet::new();
    for start in parents.keys() {
        let mut cursor = *start;
        let mut path = HashSet::new();
        while !finished.contains(&cursor) {
            let Some(next) = parents.get(&cursor) else {
                break;
            };
            if !path.insert(cursor) {
                return Err(invalid("The Linear parent links contain a cycle."));
            }
            cursor = *next;
        }
        finished.extend(path);
    }
    Ok(())
}

struct ParentPlan {
    by_source: HashMap<String, Relation>,
    changed: HashMap<String, Option<Uuid>>,
    warnings: Vec<String>,
}

type Tx<'a> = Transaction<'a, Postgres>;

async fn parents(tx: &mut Tx<'_>, owner: Uuid, project: Uuid, csv: &Csv) -> AppResult<ParentPlan> {
    type StoredRelation = (String, Uuid, Option<String>, Option<Uuid>, Option<String>);
    let stored: Vec<StoredRelation> = sqlx::query_as(
        "SELECT source_id,issue_id,parent_source,parent_id,base->'original'->>'identifier'
         FROM transfer_linear WHERE owner_id=$1 AND project_id=$2",
    )
    .bind(owner)
    .bind(project)
    .fetch_all(&mut **tx)
    .await?;
    let mut by_source = HashMap::new();
    for (source, issue, parent_source, parent_id, alias) in stored {
        by_source.insert(
            source,
            Relation {
                issue,
                parent_source,
                parent_id,
                alias,
            },
        );
    }
    let mut changed = HashMap::new();
    for row in &csv.rows {
        let rel = by_source
            .entry(row.source_id.clone())
            .or_insert_with(|| Relation {
                issue: Uuid::new_v4(),
                parent_source: None,
                parent_id: None,
                alias: None,
            });
        if let Some(alias) = row.original.get("identifier").and_then(Value::as_str) {
            rel.alias = Some(alias.into());
        }
        if let Some(parent) = &row.parent
            && rel.parent_source.as_ref() != Some(parent)
        {
            rel.parent_source = Some(parent.clone());
            changed.insert(row.source_id.clone(), None);
        }
    }
    let mut all_ids = HashMap::new();
    for (source, rel) in &by_source {
        for alias in std::iter::once(source).chain(rel.alias.iter()) {
            if all_ids
                .insert(alias, rel.issue)
                .is_some_and(|existing| existing != rel.issue)
            {
                return Err(invalid(
                    "A Linear identifier matches two different issues in this project.",
                ));
            }
        }
    }
    let included: HashSet<_> = csv.rows.iter().map(|r| &r.source_id).collect();
    let mut warnings = Vec::new();
    let mut edges = HashMap::new();
    for (source, rel) in &by_source {
        if let Some(parent) = &rel.parent_source {
            let id = all_ids.get(parent).copied();
            if id != rel.parent_id || changed.contains_key(source) {
                changed.insert(source.clone(), id);
            }
            if let Some(id) = id {
                edges.insert(rel.issue, id);
            } else if included.contains(source) {
                warnings.push(format!("{source} references missing Linear parent {parent}. The reference will be resolved when that parent is imported."));
            }
        }
    }
    check_cycles(&edges)?;
    warnings.sort();
    Ok(ParentPlan {
        by_source,
        changed,
        warnings,
    })
}

const PROJECTION: &str =
    "SELECT jsonb_build_object('title',i.title,'body',i.body,'status',i.status,'priority',i.priority,
         'labels',i.labels,'created_at',i.created_at,'updated_at',i.updated_at,'started_at',i.started_at,
         'completed_at',i.completed_at,'archived',i.archived), l.base
     FROM issues i JOIN transfer_linear l ON i.owner_id=l.owner_id AND i.project_id=l.project_id AND i.id=l.issue_id
     WHERE l.owner_id=$1 AND l.project_id=$2 AND l.source_id=$3";

async fn insert_issue(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    id: Uuid,
    number: i64,
    row: &LinearRow,
) -> AppResult<()> {
    let raw: Value = sqlx::query_scalar(
        "INSERT INTO issues AS i (id,owner_id,project_id,number,title,body,status,priority,labels,created_at,updated_at,started_at,completed_at,archived)
         SELECT $1,$2,$3,$4,s.title,COALESCE(s.body,''),COALESCE(s.status,'ready'),COALESCE(s.priority,'normal'),
             COALESCE(s.labels,ARRAY[]::text[]),COALESCE(s.created_at,s.updated_at,now()),COALESCE(s.updated_at,s.created_at,now()),
             s.started_at,s.completed_at,COALESCE(s.archived,false) FROM jsonb_populate_record(NULL::issues,$5) AS s
         RETURNING jsonb_build_object('title',i.title,'body',i.body,'status',i.status,'priority',i.priority,'labels',i.labels,
             'created_at',i.created_at,'updated_at',i.updated_at,'started_at',i.started_at,'completed_at',i.completed_at,'archived',i.archived)")
        .bind(id).bind(owner).bind(project).bind(number).bind(json!(row.values)).fetch_one(&mut **tx).await?;
    let stored = canonical_state(raw)?;
    sqlx::query("INSERT INTO transfer_linear(owner_id,project_id,source_id,issue_id,base) VALUES ($1,$2,$3,$4,$5)")
        .bind(owner).bind(project).bind(&row.source_id).bind(id).bind(initial_base(&stored,row)).execute(&mut **tx).await?;
    event(tx, owner, project, id, None).await
}

async fn event(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    issue: Uuid,
    old: Option<&str>,
) -> AppResult<()> {
    sqlx::query("INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,action,revision_after)
        SELECT owner_id,project_id,id,$4,status,'import',revision FROM issues WHERE owner_id=$1 AND project_id=$2 AND id=$3")
        .bind(owner).bind(project).bind(issue).bind(old).execute(&mut **tx).await?;
    Ok(())
}

async fn apply_merge(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    issue: Uuid,
    m: &Merge,
    old: &Value,
    same_base: bool,
) -> AppResult<()> {
    if !m.changed.is_empty() {
        sqlx::query("UPDATE issues i SET title=s.title,body=s.body,status=s.status,priority=s.priority,labels=s.labels,
             created_at=s.created_at,updated_at=s.updated_at,started_at=s.started_at,completed_at=s.completed_at,
             archived=s.archived,revision=i.revision+1 FROM jsonb_populate_record(NULL::issues,$4) s
             WHERE i.owner_id=$1 AND i.project_id=$2 AND i.id=$3")
            .bind(owner).bind(project).bind(issue).bind(json!(m.values)).execute(&mut **tx).await?;
        event(tx, owner, project, issue, old["status"].as_str()).await?;
    }
    if !same_base {
        sqlx::query("UPDATE transfer_linear SET base=$4 WHERE owner_id=$1 AND project_id=$2 AND issue_id=$3")
            .bind(owner).bind(project).bind(issue).bind(&m.base).execute(&mut **tx).await?;
    }
    Ok(())
}

pub async fn process(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    csv: &Csv,
    mode: Mode,
) -> AppResult<Preview> {
    let plan = parents(tx, owner, project, csv).await?;
    let mut result = Preview {
        warnings: plan.warnings,
        ..Default::default()
    };
    let mut number: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(number),0)::bigint FROM issues WHERE owner_id=$1 AND project_id=$2",
    )
    .bind(owner)
    .bind(project)
    .fetch_one(&mut **tx)
    .await?;
    for row in &csv.rows {
        let issue = plan.by_source[&row.source_id].issue;
        let stored: Option<(Value, Value)> = sqlx::query_as(PROJECTION)
            .bind(owner)
            .bind(project)
            .bind(&row.source_id)
            .fetch_optional(&mut **tx)
            .await?;
        let action = match stored {
            None => {
                result.created += 1;
                number = number
                    .checked_add(1)
                    .ok_or_else(|| invalid("No issue numbers remain in this project."))?;
                if mode != Mode::Preview {
                    insert_issue(tx, owner, project, issue, number, row).await?;
                }
                format!(
                    "Create · {} · {}",
                    row.values
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("ready"),
                    row.values
                        .get("priority")
                        .and_then(Value::as_str)
                        .unwrap_or("normal")
                )
            }
            Some((raw, base)) => {
                if let (Some(previous), Some(incoming)) = (
                    base["original"]["project_id"].as_str(),
                    row.original.get("project_id").and_then(Value::as_str),
                ) && previous != incoming
                {
                    return Err(invalid(format!(
                        "{} already belongs to another Linear project.",
                        row.source_id
                    )));
                }
                let m = merge(&base, canonical_state(raw.clone())?, row, mode);
                let parent_change = plan.changed.contains_key(&row.source_id);
                let same_base = m.base == base;
                let mut detail = Vec::new();
                if !m.changed.is_empty() {
                    detail.push(format!("Update {}", m.changed.join(", ")));
                }
                if parent_change {
                    detail.push("Update parent reference".into());
                }
                if !m.conflicts.is_empty() {
                    result.conflicts += 1;
                    detail.push(format!("Local and CSV edits to {}", m.conflicts.join(", ")));
                }
                if !same_base && detail.is_empty() {
                    detail.push("Update source metadata".into());
                }
                if detail.is_empty() {
                    result.unchanged += 1;
                    detail.push("Unchanged".into());
                } else {
                    result.updated += 1;
                }
                if mode != Mode::Preview {
                    apply_merge(tx, owner, project, issue, &m, &raw, same_base).await?;
                }
                detail.join(". ")
            }
        };
        result
            .rows
            .push((format!("{} · {}", row.source_id, row.title), action));
    }
    if mode != Mode::Preview {
        // Run after inserts so child-before-parent CSV order and older unresolved
        // children both resolve in the same project transaction.
        for (source, parent_id) in plan.changed {
            sqlx::query("UPDATE transfer_linear SET parent_id=$4,parent_source=$5 WHERE owner_id=$1 AND project_id=$2 AND source_id=$3")
                .bind(owner).bind(project).bind(&source).bind(parent_id).bind(&plan.by_source[&source].parent_source)
                .execute(&mut **tx).await?;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linear_project_export_including_multiline_markdown_and_source_dates() {
        let csv = parse(b"ID,Title,Description,Status,Priority,Project ID,Parent issue,Created,Labels,Due Date,Estimate\r\nL-2,Child,\"first, line\nsecond\",In Review,High,p,L-1,2025-01-01T10:00:00+02:00,\"api, ui\",2025-12-31,2\r\nL-1,Parent,,Custom lane,4,p,,,,,3\r\n").unwrap();
        assert_eq!(csv.rows[0].values["body"], "first, line\nsecond");
        assert_eq!(csv.rows[0].values["status"], "in_progress");
        assert_eq!(csv.rows[0].values["priority"], "high");
        assert_eq!(
            csv.rows[0].values["created_at"],
            "2025-01-01T08:00:00.000000Z"
        );
        assert_eq!(csv.rows[0].values["labels"], json!(["api", "ui"]));
        assert_eq!(csv.rows[0].original["due_on"], "2025-12-31");
        assert_eq!(csv.rows[0].parent.as_deref(), Some("L-1"));
        assert_eq!(csv.rows[1].values["status"], "backlog");
        assert!(!csv.rows[1].values.contains_key("body"));
        assert!(csv.warnings.iter().any(|s| s.contains("Custom lane")));
        assert!(csv.warnings.iter().any(|s| s.contains("Estimate")));
    }

    #[test]
    fn rejects_invalid_dates_duplicate_ids_mixed_projects_and_excessive_input() {
        for invalid_csv in [
            "ID,Title,Started\n1,1,yesterday",
            "ID,Title\n1,x\n1,y",
            "ID,Title,Project\n1,x,\n2,y,p",
            "ID,Title,Project ID\n1,x,a\n2,y,b",
            "ID,Status\n1,x",
            "ID,Title,Title\n1,x,x",
            "ID,Title,Parent issue\nx,x,x",
        ] {
            assert!(parse(invalid_csv.as_bytes()).is_err(), "{invalid_csv}");
        }
        assert!(parse(&vec![b'a'; MAX_CSV + 1]).is_err());
        let many: String = (0..=MAX_ROWS).map(|k| format!("{k},title\n")).collect();
        assert!(parse(format!("ID,Title\n{many}").as_bytes()).is_err());
    }

    #[test]
    fn three_way_merge_preserves_empty_fields_and_resolves_independent_edits() {
        let mut parsed = parse(b"ID,Title,Description,Priority\nL-1,Old,Keep,Normal").unwrap();
        let old = parsed.rows.pop().unwrap();
        let base = initial_base(&old.values, &old);
        let mut local = old.values.clone();
        local.insert("title".into(), json!("Local"));
        let mut new_csv = parse(b"ID,Title,Description,Priority\nL-1,Upstream,,Urgent").unwrap();
        let incoming = new_csv.rows.pop().unwrap();
        let kept = merge(&base, local.clone(), &incoming, Mode::KeepLocal);
        assert_eq!(kept.conflicts, ["title"]);
        assert_eq!(kept.changed, ["priority"]);
        assert_eq!(kept.values["title"], "Local");
        assert_eq!(kept.values["body"], "Keep");
        assert!(
            merge(&kept.base, kept.values.clone(), &incoming, Mode::KeepLocal)
                .changed
                .is_empty()
        );
        let replaced = merge(&base, local, &incoming, Mode::UseIncoming);
        assert_eq!(replaced.values["title"], "Upstream");
        assert_eq!(replaced.values["body"], "Keep");
    }

    #[test]
    fn aliases_and_retained_parent_cycles_are_checked() {
        let csv = parse(
            b"ID,Identifier,Title,Parent issue\nuuid-2,KEY-2,Child,KEY-1\nuuid-1,KEY-1,Parent,",
        )
        .unwrap();
        assert_eq!(csv.rows[0].parent.as_deref(), Some("uuid-1"));
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        assert!(check_cycles(&HashMap::from([(a, b), (b, c)])).is_ok());
        assert!(check_cycles(&HashMap::from([(a, b), (b, c), (c, a)])).is_err());
    }
}
