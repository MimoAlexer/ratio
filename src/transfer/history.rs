//! The ZIP contains only native project data. All table and column names below
//! are an explicit allowlist. Tenant IDs and provider credentials never cross it.
use super::{Action, Mode, archive::ManifestFile, hash};
use crate::{AppError, AppResult};
use serde_json::{Map, Value, json};
use sqlx::{Postgres, Transaction};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

type Tx<'a> = Transaction<'a, Postgres>;
type Document = Map<String, Value>;

struct Table {
    name: &'static str,
    columns: &'static str,
    keys: &'static str,
    references: &'static [(&'static str, &'static str)],
}

// Tables appear in FK order. Publication is finalized after its immutable
// entries and carryover snapshot have been inserted into the draft.
const TABLES: &[Table] = &[
    Table {
        name: "releases",
        columns: "id,name,version,source_url,target_on,published_on,published_at,publish_command,revision,created_at",
        keys: "id",
        references: &[("id", "releases"), ("publish_command", "commands")],
    },
    Table {
        name: "milestones",
        columns: "id,title,details,status,start_on,end_on,target_on,revision,created_at,updated_at",
        keys: "id",
        references: &[("id", "milestones")],
    },
    Table {
        name: "issue_events",
        columns: "id,issue_id,old_status,new_status,occurred_at,command_id,action,before_state,after_state,revision_after,reverses,ingested_at",
        keys: "id",
        references: &[
            ("id", "issue_events"),
            ("issue_id", "issues"),
            ("command_id", "commands"),
            ("reverses", "issue_events"),
        ],
    },
    Table {
        name: "issue_blockers",
        columns: "issue_id,blocker_id",
        keys: "issue_id,blocker_id",
        references: &[("issue_id", "issues"), ("blocker_id", "issues")],
    },
    Table {
        name: "milestone_dependencies",
        columns: "prerequisite_id,dependent_id",
        keys: "prerequisite_id,dependent_id",
        references: &[
            ("prerequisite_id", "milestones"),
            ("dependent_id", "milestones"),
        ],
    },
    Table {
        name: "milestone_issues",
        columns: "milestone_id,issue_id",
        keys: "milestone_id,issue_id",
        references: &[("milestone_id", "milestones"), ("issue_id", "issues")],
    },
    Table {
        name: "roadmap_weeks_off",
        columns: "week_of,note",
        keys: "week_of",
        references: &[],
    },
    Table {
        name: "milestone_releases",
        columns: "milestone_id,release_id",
        keys: "milestone_id",
        references: &[("milestone_id", "milestones"), ("release_id", "releases")],
    },
    Table {
        name: "roadmap_release_spans",
        columns: "release_id,start_on,end_on,revision",
        keys: "release_id",
        references: &[("release_id", "releases")],
    },
    Table {
        name: "release_members",
        columns: "release_id,issue_id",
        keys: "issue_id",
        references: &[("issue_id", "issues"), ("release_id", "releases")],
    },
    Table {
        name: "release_membership_events",
        columns: "id,release_id,issue_id,in_scope,status,occurred_at,source",
        keys: "id",
        references: &[
            ("id", "release_membership_events"),
            ("release_id", "releases"),
            ("issue_id", "issues"),
        ],
    },
    Table {
        name: "release_entries",
        columns: "release_id,issue_id,number,title,issue_revision",
        keys: "release_id,issue_id",
        references: &[("release_id", "releases"), ("issue_id", "issues")],
    },
    Table {
        name: "release_carryovers",
        columns: "release_id,issue_id,number,title,status,issue_revision,next_release_id",
        keys: "release_id,issue_id",
        references: &[
            ("release_id", "releases"),
            ("issue_id", "issues"),
            ("next_release_id", "releases"),
        ],
    },
    Table {
        name: "release_amendments",
        columns: "id,release_id,issue_id,reason,replacement,created_at",
        keys: "id",
        references: &[
            ("id", "release_amendments"),
            ("release_id", "releases"),
            ("issue_id", "issues"),
        ],
    },
    Table {
        name: "note_issue_sources",
        columns: "note_id,issue_id,source_anchor,excerpt,note_revision",
        keys: "issue_id",
        references: &[("note_id", "notes"), ("issue_id", "issues")],
    },
    Table {
        name: "note_links",
        columns: "source_id,ordinal,kind,raw,target,fragment,label,dest_note,dest_issue,manual",
        keys: "source_id,ordinal",
        references: &[
            ("source_id", "notes"),
            ("dest_note", "notes"),
            ("dest_issue", "issues"),
        ],
    },
    Table {
        name: "transfer_linear",
        columns: "issue_id,source_id,base,parent_source,parent_id",
        keys: "source_id",
        references: &[("issue_id", "issues"), ("parent_id", "issues")],
    },
];

fn bad(message: impl Into<String>) -> AppError {
    AppError::BadRequest(message.into())
}
fn as_id(v: &Value) -> AppResult<Uuid> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("Native history contains an invalid identifier."))
}
fn object(value: &Value) -> AppResult<&Document> {
    value
        .as_object()
        .ok_or_else(|| bad("Native history row must be an object."))
}
fn digest(row: &Value) -> String {
    hash(row.to_string().as_bytes())
}

fn qualified(columns: &str, alias: &str) -> String {
    columns
        .split(',')
        .map(|c| format!("{alias}.{c}"))
        .collect::<Vec<_>>()
        .join(",")
}
#[derive(Default)]
struct ReleaseText {
    shipped: Vec<(i64, String)>,
    carryover: Vec<(i64, String)>,
    amendments: Vec<(String, String, String)>,
}

fn release_text(history: &[Value]) -> AppResult<HashMap<Uuid, ReleaseText>> {
    let mut releases: HashMap<Uuid, ReleaseText> = HashMap::new();
    for line in history {
        let Some(kind @ ("release_entries" | "release_carryovers" | "release_amendments")) =
            line["type"].as_str()
        else {
            continue;
        };
        let row = &line["data"];
        let text = releases.entry(as_id(&row["release_id"])?).or_default();
        if kind == "release_amendments" {
            text.amendments.push((
                row["created_at"].as_str().unwrap_or_default().into(),
                row["reason"].as_str().unwrap_or_default().into(),
                row["replacement"].as_str().unwrap_or_default().into(),
            ));
        } else {
            let list = if kind == "release_entries" {
                &mut text.shipped
            } else {
                &mut text.carryover
            };
            list.push((
                row["number"]
                    .as_i64()
                    .ok_or_else(|| bad("Invalid release issue number."))?,
                row["title"]
                    .as_str()
                    .ok_or_else(|| bad("Invalid snapshot title."))?
                    .to_owned(),
            ));
        }
    }
    for text in releases.values_mut() {
        text.shipped.sort();
        text.carryover.sort();
        text.amendments.sort();
    }
    Ok(releases)
}

fn portable_release(data: &Value, details: Option<&ReleaseText>) -> Vec<u8> {
    let mut out = format!("# {}\n\n", data["name"].as_str().unwrap_or("Release"));
    for (key, label) in [
        ("version", "Version"),
        ("target_on", "Target date"),
        ("published_on", "Published"),
        ("source_url", "Source"),
    ] {
        if let Some(v) = data[key].as_str().filter(|s| !s.is_empty()) {
            out.push_str(&format!("- {label}: {v}\n"));
        }
    }
    if data["published_at"].is_null() {
        out.push_str("- State: Draft\n");
    }
    if let Some(text) = details {
        for (title, rows) in [("Shipped", &text.shipped), ("Carryover", &text.carryover)] {
            if !rows.is_empty() {
                out.push_str(&format!("\n## {title}\n\n"));
                for (number, title) in rows {
                    out.push_str(&format!("- #{number} {title}\n"));
                }
            }
        }
        if !text.amendments.is_empty() {
            out.push_str("\n## Amendments\n\n");
            for (date, reason, replacement) in &text.amendments {
                out.push_str(&format!("- {date} — {reason}\n  {replacement}\n"));
            }
        }
    }
    out.into_bytes()
}
// Production forwards each row to the ZIP writer. The in-memory target exists
// only in tests and exercises the identical ordering, projection and rendering.
enum Target<'a> {
    Zip(&'a super::export::Sink),
    #[cfg(test)]
    Test(Vec<(ManifestFile, Vec<u8>)>, Vec<Value>),
}
impl Target<'_> {
    async fn file(&mut self, meta: ManifestFile, bytes: Vec<u8>) -> AppResult<()> {
        match self {
            Self::Zip(sink) => sink.file(meta, bytes).await,
            #[cfg(test)]
            Self::Test(files, _) => {
                files.push((meta, bytes));
                Ok(())
            }
        }
    }
    async fn event(&mut self, row: Value) -> AppResult<()> {
        match self {
            Self::Zip(sink) => sink.event(row).await,
            #[cfg(test)]
            Self::Test(_, events) => {
                events.push(row);
                Ok(())
            }
        }
    }
}

pub(super) async fn stream(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    out: &super::export::Sink,
) -> AppResult<()> {
    export_to(tx, owner, project, &mut Target::Zip(out)).await
}

#[cfg(test)]
pub async fn export(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
) -> AppResult<(Vec<(ManifestFile, Vec<u8>)>, Vec<Value>)> {
    let mut target = Target::Test(Vec::new(), Vec::new());
    export_to(tx, owner, project, &mut target).await?;
    match target {
        Target::Test(files, events) => Ok((files, events)),
        Target::Zip(_) => unreachable!(),
    }
}

async fn export_to(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    out: &mut Target<'_>,
) -> AppResult<()> {
    use futures_util::TryStreamExt;
    let headers = &TABLES[0];
    let header_sql = format!(
        "SELECT to_jsonb(r) FROM (SELECT {} FROM releases WHERE owner_id=$1 AND project_id=$2 AND ($3::uuid IS NULL OR id>$3) ORDER BY id LIMIT 32) r",
        headers.columns
    );
    // A release document contains only its own shipment, carryover and amendment
    // text. Paging headers allows issuing those queries on the same snapshot.
    let mut cursor = None;
    loop {
        let page = sqlx::query_scalar::<_, Value>(&header_sql)
            .bind(owner)
            .bind(project)
            .bind(cursor)
            .fetch_all(&mut **tx)
            .await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            let id = as_id(&row["id"])?;
            let details=sqlx::query_scalar::<_,Value>("SELECT jsonb_build_object('type','release_entries','data',to_jsonb(e)) FROM (SELECT release_id,number,title FROM release_entries WHERE owner_id=$1 AND project_id=$2 AND release_id=$3) e
                UNION ALL SELECT jsonb_build_object('type','release_carryovers','data',to_jsonb(c)) FROM (SELECT release_id,number,title FROM release_carryovers WHERE owner_id=$1 AND project_id=$2 AND release_id=$3) c
                UNION ALL SELECT jsonb_build_object('type','release_amendments','data',to_jsonb(a)) FROM (SELECT release_id,created_at,reason,replacement FROM release_amendments WHERE owner_id=$1 AND project_id=$2 AND release_id=$3) a")
                .bind(owner).bind(project).bind(id).fetch_all(&mut **tx).await?;
            let details = release_text(&details)?;
            let bytes = portable_release(&row, details.get(&id));
            let file = ManifestFile {
                id,
                path: format!("Releases/{id}.md"),
                kind: "release".to_owned(),
                sha256: hash(&bytes),
                data: row,
            };
            out.file(file, bytes).await?;
            cursor = Some(id);
        }
    }
    for table in &TABLES[1..] {
        let extra = if table.name == "note_links" {
            "AND (dest_note IS NOT NULL OR dest_issue IS NOT NULL)"
        } else {
            ""
        };
        let sql = format!(
            "SELECT to_jsonb(r) FROM (SELECT {} FROM {} WHERE owner_id=$1 AND project_id=$2 {} ORDER BY {}) r",
            table.columns, table.name, extra, table.keys
        );
        let mut rows = sqlx::query_scalar::<_, Value>(&sql)
            .bind(owner)
            .bind(project)
            .fetch(&mut **tx);
        while let Some(row) = rows.try_next().await? {
            out.event(json!({"type":table.name,"data":row})).await?;
        }
    }
    Ok(())
}

struct Record<'a> {
    table: &'a Table,
    data: &'a Value,
    key: String,
}

fn records<'a>(
    releases: &'a [(ManifestFile, Vec<u8>)],
    history: &'a [Value],
) -> AppResult<Vec<Record<'a>>> {
    let mut grouped: HashMap<&str, Vec<&Value>> = HashMap::new();
    let details = release_text(history)?;
    for (file, bytes) in releases {
        if file.kind != "release"
            || as_id(&file.data["id"])? != file.id
            || file.sha256 != hash(bytes)
            || *bytes != portable_release(&file.data, details.get(&file.id))
        {
            return Err(bad(
                "A release file differs from its archived snapshot. Restore the original release Markdown before importing.",
            ));
        }
        grouped.entry("releases").or_default().push(&file.data);
    }
    for line in history {
        let name = line["type"]
            .as_str()
            .ok_or_else(|| bad("Missing native history record type."))?;
        if name == "releases" || !TABLES.iter().any(|t| t.name == name) {
            return Err(bad("Unknown native history record type."));
        }
        grouped.entry(name).or_default().push(&line["data"]);
    }
    let mut out = Vec::new();
    for table in TABLES {
        let mut seen = HashSet::new();
        for data in grouped.remove(table.name).unwrap_or_default() {
            let row = object(data)?;
            let expected: Vec<_> = table.columns.split(',').collect();
            // Earlier version-one archives stored only manually selected note targets.
            let legacy = table.name == "note_links"
                && !row.contains_key("manual")
                && !row.contains_key("dest_issue")
                && row.len() + 2 == expected.len()
                && expected
                    .iter()
                    .all(|c| matches!(*c, "manual" | "dest_issue") || row.contains_key(*c));
            if !legacy
                && (row.len() != expected.len()
                    || !expected.iter().all(|col| row.contains_key(*col)))
            {
                return Err(bad(format!(
                    "Invalid columns in native {} record.",
                    table.name
                )));
            }
            let key = Value::Array(table.keys.split(',').map(|k| data[k].clone()).collect());
            if key.as_array().unwrap().iter().any(Value::is_null) || !seen.insert(key.to_string()) {
                return Err(bad(format!(
                    "Missing or duplicate key in native {} records.",
                    table.name
                )));
            }
            validate_row(table.name, data)?;
            out.push(Record {
                table,
                data,
                key: digest(&key),
            });
        }
    }
    check_relationships(&out)?;
    Ok(out)
}

fn check_relationships(records: &[Record<'_>]) -> AppResult<()> {
    let mut reversals = HashMap::new();
    let mut published = HashSet::new();
    let mut snapshots = HashSet::new();
    for rec in records {
        match rec.table.name {
            "releases" if !rec.data["published_at"].is_null() => {
                published.insert(as_id(&rec.data["id"])?);
            }
            "issue_events" => {
                reversals.insert(as_id(&rec.data["id"])?, as_id(&rec.data["issue_id"])?);
            }
            _ => {}
        }
    }
    for rec in records {
        let row = rec.data;
        match rec.table.name {
            "release_entries" | "release_carryovers" => {
                let pair = (as_id(&row["release_id"])?, as_id(&row["issue_id"])?);
                if !published.contains(&pair.0) || !snapshots.insert(pair) {
                    return Err(bad(
                        "Each published release issue must occur in exactly one snapshot section.",
                    ));
                }
            }
            "issue_events" if !row["reverses"].is_null() => {
                let original = as_id(&row["reverses"])?;
                if original == as_id(&row["id"])?
                    || reversals.get(&original) != Some(&as_id(&row["issue_id"])?)
                {
                    return Err(bad(
                        "A reversal refers to an unrelated or missing issue event.",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn valid_snapshot(snapshot: &Value) -> bool {
    if snapshot.is_null() {
        return true;
    }
    let Some(object) = snapshot.as_object() else {
        return false;
    };
    let Some(fields) = snapshot["fields"].as_object() else {
        return false;
    };
    if !object
        .keys()
        .all(|key| matches!(key.as_str(), "fields" | "command_hash" | "relation"))
    {
        return false;
    }
    if !snapshot["command_hash"].is_null()
        && snapshot["command_hash"]
            .as_str()
            .is_none_or(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return false;
    }
    let relation = &snapshot["relation"];
    if !relation.is_null()
        && (as_id(&relation["blocker_id"]).is_err()
            || !relation["present"].is_boolean()
            || relation.as_object().is_none_or(|r| r.len() != 2))
    {
        return false;
    }
    fields.iter().all(|(key, v)| match key.as_str() {
        "title" | "body" | "next_action" => v.is_string(),
        "status" => matches!(
            v.as_str(),
            Some("backlog" | "ready" | "in_progress" | "done" | "canceled")
        ),
        "priority" => matches!(v.as_str(), Some("low" | "normal" | "high" | "urgent")),
        "kind" => matches!(
            v.as_str(),
            Some("feature" | "bug" | "maintenance" | "security")
        ),
        "manual_severity" => matches!(
            v.as_str(),
            Some("none" | "low" | "medium" | "high" | "critical")
        ),
        "archived" | "finding_closed" => v.is_boolean(),
        "labels" => v
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
        "checkpoint_at" => {
            v.is_null()
                || v.as_str()
                    .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
        }
        _ => false,
    })
}

fn validate_row(table: &str, row: &Value) -> AppResult<()> {
    let valid_status = |v: &Value| {
        matches!(
            v.as_str(),
            Some("backlog" | "ready" | "in_progress" | "done" | "canceled")
        )
    };
    match table {
        "releases" => {
            let published = !row["published_at"].is_null();
            if published == row["published_on"].is_null()
                || published == row["publish_command"].is_null()
                || row["name"].as_str().is_none_or(|v| v.trim().is_empty())
            {
                return Err(bad("Invalid release name or publication date."));
            }
            let url = row["source_url"]
                .as_str()
                .ok_or_else(|| bad("Invalid release source link."))?;
            if !url.is_empty()
                && reqwest::Url::parse(url).ok().is_none_or(|u| {
                    !matches!(u.scheme(), "http" | "https")
                        || u.host_str().is_none()
                        || !u.username().is_empty()
                        || u.password().is_some()
                })
            {
                return Err(bad("Invalid release source link."));
            }
        }
        "issue_events" => {
            if !valid_status(&row["new_status"])
                || (!row["old_status"].is_null() && !valid_status(&row["old_status"]))
            {
                return Err(bad("Invalid workflow status in native history."));
            }
            if !valid_snapshot(&row["before_state"])
                || !valid_snapshot(&row["after_state"])
                || !matches!(
                    row["action"].as_str(),
                    Some(
                        "create"
                            | "edit"
                            | "status"
                            | "checkpoint"
                            | "archive"
                            | "undo"
                            | "blocker"
                            | "import"
                    )
                )
            {
                return Err(bad(
                    "Invalid change or revision data in native issue history.",
                ));
            }
        }
        "release_membership_events" | "release_carryovers" if !valid_status(&row["status"]) => {
            return Err(bad("Invalid release scope status."));
        }
        "note_links"
            if row["dest_note"].is_null() == row["dest_issue"].is_null()
                || row.get("manual").is_some_and(|v| !v.is_boolean())
                || !matches!(row["kind"].as_str(), Some("wiki" | "markdown")) =>
        {
            return Err(bad("Invalid explicit note link target."));
        }
        _ => {}
    }
    Ok(())
}

// Deterministic local IDs keep earlier references stable when a later archive
// includes new events that refer back to a previous restore.
pub(super) fn target_id(owner: Uuid, project: Uuid, source: Uuid, kind: &str, id: Uuid) -> Uuid {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in [owner, project, source, id] {
        h.update(part.as_bytes());
    }
    h.update(kind.as_bytes());
    let mut bytes: [u8; 16] = h.finalize()[..16].try_into().expect("digest length");
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

struct Ids {
    explicit: HashMap<(&'static str, Uuid), Uuid>,
    owner: Uuid,
    project: Uuid,
    source: Uuid,
}

impl Ids {
    fn resolve(&self, kind: &'static str, id: Uuid) -> AppResult<Uuid> {
        if kind == "commands" {
            return Ok(if self.project == self.source {
                id
            } else {
                target_id(self.owner, self.project, self.source, kind, id)
            });
        }
        self.explicit.get(&(kind,id)).copied().ok_or_else(||bad(format!("Native history refers to a missing {kind} record. Import the complete project archive.")))
    }
    fn map_value(&self, row: &mut Value, key: &str, kind: &'static str) -> AppResult<()> {
        if !row[key].is_null() {
            row[key] = json!(self.resolve(kind, as_id(&row[key])?)?);
        }
        Ok(())
    }
    fn remap(&self, record: &Record<'_>) -> AppResult<Value> {
        let mut row = record.data.clone();
        if record.table.name == "note_links" && row.get("manual").is_none() {
            row["manual"] = Value::Bool(true);
            row["dest_issue"] = Value::Null;
        }
        for &(column, kind) in record.table.references {
            self.map_value(&mut row, column, kind)?;
        }
        if record.table.name == "issue_events" {
            for state in ["before_state", "after_state"] {
                if let Some(obj) = row[state].as_object_mut() {
                    // Idempotency hashes belong to the original mutation payload.
                    if self.source != self.project {
                        obj.remove("command_hash");
                    }
                    if let Some(relation) = obj.get_mut("relation")
                        && !relation.is_null()
                    {
                        self.map_value(relation, "blocker_id", "issues")?;
                    }
                }
            }
        }
        Ok(row)
    }
}

async fn id_maps(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    source: Uuid,
    records: &[Record<'_>],
    native: &HashMap<Uuid, Uuid>,
) -> AppResult<Ids> {
    let mut map = Ids {
        owner,
        project,
        source,
        explicit: HashMap::new(),
    };
    let destinations: Vec<Uuid> = native.values().copied().collect();
    for kind in ["issues", "notes"] {
        let sql =
            format!("SELECT id FROM {kind} WHERE owner_id=$1 AND project_id=$2 AND id=ANY($3)");
        let permitted: HashSet<Uuid> = sqlx::query_scalar(&sql)
            .bind(owner)
            .bind(project)
            .bind(&destinations)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .collect();
        for (&from, &to) in native {
            if permitted.contains(&to) {
                map.explicit.insert((kind, from), to);
            }
        }
    }
    for kind in [
        "releases",
        "milestones",
        "issue_events",
        "release_membership_events",
        "release_amendments",
    ] {
        let sources = records
            .iter()
            .filter(|r| r.table.name == kind)
            .map(|r| as_id(&r.data["id"]))
            .collect::<AppResult<Vec<_>>>()?;
        let same_project: HashSet<Uuid> = if project == source {
            let sql =
                format!("SELECT id FROM {kind} WHERE owner_id=$1 AND project_id=$2 AND id=ANY($3)");
            sqlx::query_scalar(&sql)
                .bind(owner)
                .bind(project)
                .bind(&sources)
                .fetch_all(&mut **tx)
                .await?
                .into_iter()
                .collect()
        } else {
            HashSet::new()
        };
        for id in sources {
            let local = if same_project.contains(&id) {
                id
            } else {
                target_id(owner, project, source, kind, id)
            };
            map.explicit.insert((kind, id), local);
        }
    }
    Ok(map)
}

fn row_key(table: &Table, row: &Value) -> String {
    let key = Value::Array(table.keys.split(',').map(|k| row[k].clone()).collect());
    digest(&key)
}

async fn local_rows(
    tx: &mut Tx<'_>,
    table: &Table,
    owner: Uuid,
    project: Uuid,
    incoming: &[Value],
) -> AppResult<HashMap<String, Value>> {
    // A small import should never read the destination project's entire event log.
    let keys = Value::Array(
        incoming
            .iter()
            .map(|row| {
                Value::Object(
                    table
                        .keys
                        .split(',')
                        .map(|k| (k.to_owned(), row[k].clone()))
                        .collect(),
                )
            })
            .collect(),
    );
    let sql = format!(
        "SELECT to_jsonb(found) FROM (SELECT {} FROM {} t JOIN jsonb_populate_recordset(NULL::{},$3::jsonb) r ON {} WHERE t.owner_id=$1 AND t.project_id=$2) found",
        qualified(table.columns, "t"),
        table.name,
        table.name,
        predicate(table)
    );
    Ok(sqlx::query_scalar::<_, Value>(&sql)
        .bind(owner)
        .bind(project)
        .bind(keys)
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|r| (row_key(table, &r), r))
        .collect())
}

fn db_error(err: sqlx::Error) -> AppError {
    if let Some(e) = err.as_database_error()
        && e.code()
            .as_deref()
            .is_some_and(|s| s.starts_with("22") || s.starts_with("23"))
    {
        return bad(
            "A native history value violates a data constraint. Check the original export and its references.",
        );
    }
    err.into()
}

fn predicate(table: &Table) -> String {
    table
        .keys
        .split(',')
        .map(|k| format!("t.{k}=r.{k}"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

async fn write(
    tx: &mut Tx<'_>,
    table: &Table,
    owner: Uuid,
    project: Uuid,
    row: &Value,
    existing: bool,
) -> AppResult<()> {
    let name = table.name;
    let mut content = row.clone();
    let obj = content.as_object_mut().expect("validated");
    obj.insert("owner_id".into(), json!(owner));
    obj.insert("project_id".into(), json!(project));
    let mut columns = format!("owner_id,project_id,{}", table.columns);
    if name == "release_amendments" {
        obj.insert("actor_id".into(), json!(owner));
        columns.push_str(",actor_id");
    }
    let input = format!("jsonb_populate_record(NULL::{name},$1::jsonb) r");
    let guard = format!(
        "t.owner_id=r.owner_id AND t.project_id=r.project_id AND {}",
        predicate(table)
    );
    if name == "note_links" {
        let sql = format!(
            "UPDATE {name} AS t SET dest_note=r.dest_note,dest_issue=r.dest_issue,manual=r.manual FROM {input} WHERE {guard} AND t.raw=r.raw AND t.kind=r.kind AND t.target=r.target AND t.fragment IS NOT DISTINCT FROM r.fragment AND t.label=r.label"
        );
        let changed = sqlx::query(&sql)
            .bind(content)
            .execute(&mut **tx)
            .await
            .map_err(db_error)?
            .rows_affected();
        if changed != 1 {
            return Err(AppError::Conflict("The original note link has changed. Keep the local link or restore the original note first.".into()));
        }
        return Ok(());
    }
    // Current release assignment is deliberately replaced with a fresh row:
    // the domain append-only trigger prohibits in-place updates.
    let existing = if name == "release_members" && existing {
        let sql = format!("DELETE FROM {name} AS t USING {input} WHERE {guard}");
        sqlx::query(&sql)
            .bind(&content)
            .execute(&mut **tx)
            .await
            .map_err(db_error)?;
        false
    } else {
        existing
    };
    let sql = if existing {
        let keys: HashSet<_> = table.keys.split(',').collect();
        let assignments = table
            .columns
            .split(',')
            .filter(|c| !keys.contains(c))
            .map(|c| format!("{c}=r.{c}"))
            .collect::<Vec<_>>()
            .join(",");
        if assignments.is_empty() {
            return Ok(());
        }
        format!("UPDATE {name} AS t SET {assignments} FROM {input} WHERE {guard}")
    } else {
        format!(
            "INSERT INTO {name} ({columns}) SELECT {} FROM {input}",
            qualified(&columns, "r")
        )
    };
    sqlx::query(&sql)
        .bind(content)
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    Ok(())
}

async fn finish_publication(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    rows: &mut Vec<Value>,
) -> AppResult<()> {
    for row in rows.drain(..) {
        write(tx, &TABLES[0], owner, project, &row, true).await?;
    }
    Ok(())
}

fn changed_both(
    local: Option<&Value>,
    incoming: &Value,
    previous: Option<&(String, String)>,
    source_hash: &str,
) -> Action {
    let incoming_hash = digest(incoming);
    let local_hash = local.map(digest);
    if local_hash.as_deref() == Some(&incoming_hash) {
        return Action::Unchanged;
    }
    if let Some((original, applied)) = previous {
        if original == source_hash {
            return Action::Unchanged;
        }
        if local_hash.as_deref() == Some(applied) {
            return Action::Update;
        }
        return Action::Conflict;
    }
    if local.is_none() {
        Action::Create
    } else {
        Action::Conflict
    }
}

fn acyclic(edges: Vec<(Uuid, Uuid)>) -> bool {
    let mut incoming: HashMap<Uuid, usize> = HashMap::new();
    let mut next: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (a, b) in edges {
        incoming.entry(a).or_default();
        *incoming.entry(b).or_default() += 1;
        next.entry(a).or_default().push(b);
    }
    let mut ready: Vec<Uuid> = incoming
        .iter()
        .filter_map(|(id, n)| (*n == 0).then_some(*id))
        .collect();
    let mut visited = 0;
    while let Some(a) = ready.pop() {
        visited += 1;
        for b in next.remove(&a).unwrap_or_default() {
            let left = incoming.get_mut(&b).expect("edge target counted");
            *left -= 1;
            if *left == 0 {
                ready.push(b);
            }
        }
    }
    visited == incoming.len()
}

async fn validate_dependencies(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    table: &str,
) -> AppResult<()> {
    let sql = match table {
        "issue_blockers" => {
            "SELECT issue_id,blocker_id FROM issue_blockers WHERE owner_id=$1 AND project_id=$2"
        }
        "transfer_linear" => {
            "SELECT issue_id,parent_id FROM transfer_linear WHERE owner_id=$1 AND project_id=$2 AND parent_id IS NOT NULL"
        }
        _ => return Ok(()),
    };
    let edges = sqlx::query_as(sql)
        .bind(owner)
        .bind(project)
        .fetch_all(&mut **tx)
        .await?;
    if !acyclic(edges) {
        return Err(bad("Imported dependencies would introduce a cycle."));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn restore(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    source_project: Uuid,
    releases: &[(ManifestFile, Vec<u8>)],
    history: &[Value],
    ids: &mut HashMap<Uuid, Uuid>,
    mode: Mode,
) -> AppResult<Vec<(String, Action, usize)>> {
    let records = records(releases, history)?;
    let mapped = id_maps(tx, owner, project, source_project, &records, ids).await?;
    let kinds: Vec<_> = records.iter().map(|r| r.table.name).collect();
    let keys: Vec<_> = records.iter().map(|r| r.key.as_str()).collect();
    let receipts:Vec<(String,String,String,String)>=sqlx::query_as("SELECT t.record_type,t.source_key,t.source_hash,t.local_hash
         FROM transfer_history_receipts t JOIN unnest($4::text[],$5::text[]) AS k(kind,key) ON (t.record_type,t.source_key)=(k.kind,k.key)
         WHERE t.owner_id=$1 AND t.project_id=$2 AND t.source_project=$3")
        .bind(owner).bind(project).bind(source_project).bind(kinds).bind(keys).fetch_all(&mut **tx).await?;
    let receipts: HashMap<(String, String), (String, String)> = receipts
        .into_iter()
        .map(|(t, k, s, l)| ((t, k), (s, l)))
        .collect();
    let mut updates = Vec::new();
    let mut publications = Vec::new();
    let mut divergent_releases = HashSet::new();
    let mut summary = Vec::new();
    for table in TABLES {
        // Amendments require a sealed header. Other release records require a draft.
        if table.name == "release_amendments" {
            finish_publication(tx, owner, project, &mut publications).await?;
        }
        let rows = records
            .iter()
            .filter(|r| r.table.name == table.name)
            .collect::<Vec<_>>();
        if rows.is_empty() {
            continue;
        }
        let incoming = rows
            .iter()
            .map(|rec| mapped.remap(rec))
            .collect::<AppResult<Vec<_>>>()?;
        let mut local = local_rows(tx, table, owner, project, &incoming).await?;
        let mut counts = [0usize; 4];
        for (rec, incoming) in rows.into_iter().zip(incoming) {
            let local_key = row_key(table, &incoming);
            let current = local.remove(&local_key);
            let source_hash = digest(rec.data);
            let prev = receipts.get(&(table.name.into(), rec.key.clone()));
            // Reindex initially guesses from the destination key and paths. An original
            // binding survives a rename or move even when the guess points elsewhere.
            let parsed_link = table.name == "note_links"
                && current.as_ref().is_some_and(|value| {
                    value["manual"] == Value::Bool(false)
                        && [
                            "source_id",
                            "ordinal",
                            "raw",
                            "target",
                            "fragment",
                            "kind",
                            "label",
                        ]
                        .iter()
                        .all(|c| value[c] == incoming[c])
                });
            let mut action = changed_both(current.as_ref(), &incoming, prev, &source_hash);
            if table.name == "note_links"
                && current.is_none()
                && !matches!(action, Action::Unchanged)
            {
                action = Action::Conflict;
            } else if parsed_link && prev.is_none() && !matches!(action, Action::Unchanged) {
                action = Action::Create;
            }
            let immutable = current.as_ref().is_some_and(|c| {
                (table.name == "releases" && !c["published_at"].is_null())
                    || matches!(
                        table.name,
                        "issue_events"
                            | "release_membership_events"
                            | "release_entries"
                            | "release_carryovers"
                            | "release_amendments"
                    )
            });
            if immutable && matches!(action, Action::Update) {
                action = Action::Conflict;
            }
            // A source draft may have been published after the destination edited
            // its own draft. Keeping the local header must also retain its local
            // membership and snapshots, otherwise the draft would appear shipped.
            if table.name == "releases"
                && matches!(action, Action::Conflict)
                && !matches!(mode, Mode::UseIncoming)
            {
                divergent_releases.insert(as_id(&incoming["id"])?);
            } else if table.name != "releases"
                && !matches!(action, Action::Unchanged)
                && table
                    .references
                    .iter()
                    .filter(|(_, kind)| *kind == "releases")
                    .any(|(field, _)| {
                        incoming[*field]
                            .as_str()
                            .and_then(|v| v.parse::<Uuid>().ok())
                            .is_some_and(|id| divergent_releases.contains(&id))
                    })
            {
                action = Action::Conflict;
            }
            counts[match action {
                Action::Create => 0,
                Action::Update => 1,
                Action::Conflict => 2,
                Action::Unchanged => 3,
            }] += 1;
            if matches!(action, Action::Conflict) {
                let identifier = table
                    .keys
                    .split(',')
                    .map(|k| {
                        rec.data[k]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| rec.data[k].to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(" / ");
                let name = rec.data["name"]
                    .as_str()
                    .or_else(|| rec.data["title"].as_str())
                    .unwrap_or("");
                summary.push((
                    format!("{} {} ({identifier})", table.name.replace('_', " "), name)
                        .trim()
                        .to_owned(),
                    action,
                    1,
                ));
            }
            let apply = matches!(action, Action::Create | Action::Update)
                || (matches!(action, Action::Conflict) && matches!(mode, Mode::UseIncoming));
            if apply {
                if immutable {
                    return Err(AppError::Conflict("A previously sealed release or recorded event differs. Keep the local history or import into a new project.".into()));
                }
                let mut row = incoming.clone();
                if table.name == "releases" && !row["published_at"].is_null() {
                    publications.push(row.clone());
                    for key in ["published_at", "published_on", "publish_command"] {
                        row[key] = Value::Null;
                    }
                }
                write(tx, table, owner, project, &row, current.is_some()).await?;
            }
            // Leaving a conflicting record local preserves its divergence. A repeat
            // identical import skips it until the owner chooses the incoming version.
            if apply || (prev.is_none() && matches!(action, Action::Unchanged)) {
                updates.push((table.name, rec.key.clone(), source_hash, digest(&incoming)));
            }
        }
        if counts[0] + counts[1] > 0 || (counts[2] > 0 && matches!(mode, Mode::UseIncoming)) {
            validate_dependencies(tx, owner, project, table.name).await?;
        }
        for (count, action) in counts.into_iter().zip([
            Action::Create,
            Action::Update,
            Action::Conflict,
            Action::Unchanged,
        ]) {
            if count > 0 && !matches!(action, Action::Conflict) {
                summary.push((
                    format!("{count} {}", table.name.replace('_', " ")),
                    action,
                    count,
                ));
            }
        }
    }
    finish_publication(tx, owner, project, &mut publications).await?;
    if !updates.is_empty() {
        let mut kinds = Vec::with_capacity(updates.len());
        let mut keys = Vec::with_capacity(updates.len());
        let mut source_hashes = Vec::with_capacity(updates.len());
        let mut local_hashes = Vec::with_capacity(updates.len());
        for (name, key, source_hash, local_hash) in updates {
            kinds.push(name);
            keys.push(key);
            source_hashes.push(source_hash);
            local_hashes.push(local_hash);
        }
        sqlx::query("INSERT INTO transfer_history_receipts(owner_id,project_id,source_project,record_type,source_key,source_hash,local_hash)
            SELECT $1,$2,$3,kind,key,source_digest,local_digest FROM unnest($4::text[],$5::text[],$6::text[],$7::text[]) AS r(kind,key,source_digest,local_digest)
            ON CONFLICT(owner_id,project_id,source_project,record_type,source_key)
            DO UPDATE SET source_hash=EXCLUDED.source_hash,local_hash=EXCLUDED.local_hash")
            .bind(owner).bind(project).bind(source_project).bind(kinds).bind(keys).bind(source_hashes).bind(local_hashes).execute(&mut **tx).await?;
    }
    Ok(summary)
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    #[test]
    fn reject_unknown_change_fields_and_cross_issue_reversal() {
        assert!(valid_snapshot(&json!({"fields":{"status":"done"}})));
        assert!(!valid_snapshot(
            &json!({"fields":{"owner_id":Uuid::new_v4()}})
        ));
        assert!(!valid_snapshot(
            &json!({"fields":{},"relation":{"blocker_id":"missing","present":true}})
        ));
        let source = Uuid::new_v4();
        let event = Uuid::new_v4();
        let other = Uuid::new_v4();
        let table = TABLES.iter().find(|t| t.name == "issue_events").unwrap();
        let first = json!({"id":event,"issue_id":source});
        let second = json!({"id":Uuid::new_v4(),"issue_id":other,"reverses":event});
        let rows = [
            Record {
                table,
                data: &first,
                key: "a".into(),
            },
            Record {
                table,
                data: &second,
                key: "b".into(),
            },
        ];
        assert!(check_relationships(&rows).is_err());
    }

    #[test]
    fn detect_dependency_cycles_without_recursing() {
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        assert!(acyclic(vec![
            (ids[0], ids[1]),
            (ids[1], ids[2]),
            (ids[0], ids[2])
        ]));
        assert!(!acyclic(vec![
            (ids[0], ids[1]),
            (ids[1], ids[2]),
            (ids[2], ids[0])
        ]));
    }
}
#[cfg(test)]
mod postgres_roundtrip {
    use super::{Action, Mode, export, restore};
    use serde_json::json;
    use sqlx::{PgPool, Postgres, Transaction};
    use std::collections::HashMap;
    use uuid::Uuid;

    struct Workspace {
        owner: Uuid,
        project: Uuid,
        shipped: Uuid,
        carry: Uuid,
    }
    async fn workspace(db: &PgPool) -> Result<Workspace, sqlx::Error> {
        let owner: Uuid = sqlx::query_scalar("INSERT INTO users(email) VALUES($1) RETURNING id")
            .bind(format!("{}@example.test", Uuid::new_v4()))
            .fetch_one(db)
            .await?;
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES($1,'Portability','PORT') RETURNING id",
        )
        .bind(owner)
        .fetch_one(db)
        .await?;
        let mut issue_ids = vec![];
        for (number, title, status) in [(1_i64, "Ship", "done"), (2, "Carry", "in_progress")] {
            let id:Uuid=sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title,status) VALUES($1,$2,$3,$4,$5) RETURNING id")
                .bind(owner).bind(project).bind(number).bind(title).bind(status).fetch_one(db).await?;
            issue_ids.push(id);
        }
        Ok(Workspace {
            owner,
            project,
            shipped: issue_ids[0],
            carry: issue_ids[1],
        })
    }

    async fn draft(
        tx: &mut Transaction<'_, Postgres>,
        ws: &Workspace,
        name: &str,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO releases(owner_id,project_id,name) VALUES($1,$2,$3) RETURNING id",
        )
        .bind(ws.owner)
        .bind(ws.project)
        .bind(name)
        .fetch_one(&mut **tx)
        .await
    }
    async fn seed_history(db: &PgPool, ws: &Workspace) -> Result<(), sqlx::Error> {
        let mut tx = db.begin().await?;
        let current = draft(&mut tx, ws, "Beta").await?;
        let next = draft(&mut tx, ws, "Next").await?;
        let milestone:Uuid=sqlx::query_scalar("INSERT INTO milestones(owner_id,project_id,title,target_on) VALUES($1,$2,'Delivery gate','2025-05-04') RETURNING id")
            .bind(ws.owner).bind(ws.project).fetch_one(&mut *tx).await?;
        let prerequisite: Uuid = sqlx::query_scalar(
            "INSERT INTO milestones(owner_id,project_id,title) VALUES($1,$2,'Verification') RETURNING id"
        ).bind(ws.owner).bind(ws.project).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO milestone_dependencies(owner_id,project_id,prerequisite_id,dependent_id) VALUES($1,$2,$3,$4)")
            .bind(ws.owner).bind(ws.project).bind(prerequisite).bind(milestone).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO transfer_linear(owner_id,project_id,source_id,issue_id,base,parent_source,parent_id)
            VALUES($1,$2,'PORT-1',$3,'{}'::jsonb,NULL,NULL),($1,$2,'PORT-2',$4,'{}'::jsonb,'PORT-1',$3)")
            .bind(ws.owner).bind(ws.project).bind(ws.shipped).bind(ws.carry).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO milestone_issues(owner_id,project_id,milestone_id,issue_id) VALUES($1,$2,$3,$4)")
            .bind(ws.owner).bind(ws.project).bind(milestone).bind(ws.shipped).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO milestone_releases(owner_id,project_id,milestone_id,release_id) VALUES($1,$2,$3,$4)")
            .bind(ws.owner).bind(ws.project).bind(milestone).bind(current).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,occurred_at,command_id,action) VALUES($1,$2,$3,'in_progress','done','2025-05-03 08:00Z',gen_random_uuid(),'status')")
            .bind(ws.owner).bind(ws.project).bind(ws.shipped).execute(&mut *tx).await?;
        for (release, issue, status) in [
            (current, ws.shipped, "done"),
            (next, ws.carry, "in_progress"),
        ] {
            sqlx::query("INSERT INTO release_members(owner_id,project_id,release_id,issue_id) VALUES($1,$2,$3,$4)")
                .bind(ws.owner).bind(ws.project).bind(release).bind(issue).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO release_membership_events(owner_id,project_id,release_id,issue_id,in_scope,status,occurred_at) VALUES($1,$2,$3,$4,true,$5,'2025-05-03 09:00Z')")
                .bind(ws.owner).bind(ws.project).bind(release).bind(issue).bind(status).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO release_entries(owner_id,project_id,release_id,issue_id,number,title,issue_revision) VALUES($1,$2,$3,$4,1,'Original manifest',1)")
            .bind(ws.owner).bind(ws.project).bind(current).bind(ws.shipped).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO release_carryovers(owner_id,project_id,release_id,issue_id,number,title,status,issue_revision,next_release_id) VALUES($1,$2,$3,$4,2,'Carry at publish','in_progress',1,$5)")
            .bind(ws.owner).bind(ws.project).bind(current).bind(ws.carry).bind(next).execute(&mut *tx).await?;
        sqlx::query("UPDATE releases SET published_on='2025-05-04',published_at='2025-05-04 10:00Z',publish_command=gen_random_uuid() WHERE id=$1")
            .bind(current).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO release_amendments(owner_id,project_id,release_id,issue_id,actor_id,reason,replacement,created_at) VALUES($1,$2,$3,$4,$1,'Clarify scope','Amended manifest','2025-05-05 09:00Z')")
            .bind(ws.owner).bind(ws.project).bind(current).bind(ws.shipped).execute(&mut *tx).await?;
        tx.commit().await
    }

    #[sqlx::test]
    async fn history_remaps_committed_publications_and_rejects_foreign_references(
        db: PgPool,
    ) -> Result<(), sqlx::Error> {
        let source = workspace(&db).await?;
        let dest = workspace(&db).await?;
        seed_history(&db, &source).await?;
        let mut tx = db.begin().await?;
        let (releases, history) = export(&mut tx, source.owner, source.project).await.unwrap();
        tx.commit().await?;
        let mut ids = HashMap::from([(source.shipped, dest.shipped), (source.carry, dest.carry)]);

        // A forged mapping to the source owner's real issue must fail even after
        // the importer began staging destination releases in this transaction.
        let mut forged = history.clone();
        let spoofed = Uuid::new_v4();
        forged
            .iter_mut()
            .find(|row| row["type"] == "release_amendments")
            .unwrap()["data"]["issue_id"] = json!(spoofed);
        let mut forged_ids = ids.clone();
        forged_ids.insert(spoofed, source.shipped);
        let mut tx = db.begin().await?;
        assert!(
            restore(
                &mut tx,
                dest.owner,
                dest.project,
                source.project,
                &releases,
                &forged,
                &mut forged_ids,
                Mode::KeepLocal
            )
            .await
            .is_err()
        );
        tx.rollback().await?;
        let residual: i64 = sqlx::query_scalar("SELECT count(*) FROM releases WHERE project_id=$1")
            .bind(dest.project)
            .fetch_one(&db)
            .await?;
        assert_eq!(residual, 0);

        let mut tx = db.begin().await?;
        let first = restore(
            &mut tx,
            dest.owner,
            dest.project,
            source.project,
            &releases,
            &history,
            &mut ids,
            Mode::KeepLocal,
        )
        .await
        .unwrap();
        assert!(first.iter().any(|row| matches!(row.1, Action::Create)));
        tx.commit().await?;
        let mut tx = db.begin().await?;
        let again = restore(
            &mut tx,
            dest.owner,
            dest.project,
            source.project,
            &releases,
            &history,
            &mut ids,
            Mode::KeepLocal,
        )
        .await
        .unwrap();
        assert!(again.iter().all(|row| matches!(row.1, Action::Unchanged)));
        let (files, events) = export(&mut tx, dest.owner, dest.project).await.unwrap();
        tx.commit().await?;
        assert_eq!(files.len(), 2);
        assert_eq!(events.len(), history.len());
        let published = files
            .iter()
            .find(|(f, _)| f.data["name"] == "Beta")
            .unwrap();
        assert_eq!(published.0.data["published_on"], "2025-05-04");
        assert!(String::from_utf8_lossy(&published.1).contains("2025-05-04"));
        let next = files
            .iter()
            .find(|(f, _)| f.data["name"] == "Next")
            .unwrap()
            .0
            .id;
        assert!(
            !releases
                .iter()
                .any(|(r, _)| r.id == published.0.id || r.id == next)
        );
        let event = |kind: &str| &events.iter().find(|row| row["type"] == kind).unwrap()["data"];
        assert_eq!(event("issue_events")["issue_id"], json!(dest.shipped));
        assert_eq!(
            event("issue_events")["occurred_at"],
            "2025-05-03T08:00:00+00:00"
        );
        assert_eq!(event("release_carryovers")["next_release_id"], json!(next));
        assert_eq!(event("release_carryovers")["issue_id"], json!(dest.carry));
        assert_eq!(event("release_entries")["title"], "Original manifest");
        assert_eq!(
            event("release_amendments")["replacement"],
            "Amended manifest"
        );
        assert_eq!(
            event("milestone_releases")["release_id"],
            json!(published.0.id)
        );
        let find = |kind: &str, field: &str, value: &str| {
            &events
                .iter()
                .find(|row| row["type"] == kind && row["data"][field] == value)
                .unwrap()["data"]
        };
        let milestone_id = &find("milestones", "title", "Delivery gate")["id"];
        let prerequisite_id = &find("milestones", "title", "Verification")["id"];
        assert_eq!(&event("milestone_issues")["milestone_id"], milestone_id);
        assert_eq!(
            &event("milestone_dependencies")["dependent_id"],
            milestone_id
        );
        assert_eq!(
            &event("milestone_dependencies")["prerequisite_id"],
            prerequisite_id
        );
        let child = find("transfer_linear", "source_id", "PORT-2");
        assert_eq!(child["parent_source"], "PORT-1");
        assert_eq!(child["parent_id"], json!(dest.shipped));
        assert_eq!(child["issue_id"], json!(dest.carry));
        let actor: Uuid =
            sqlx::query_scalar("SELECT actor_id FROM release_amendments WHERE project_id=$1")
                .bind(dest.project)
                .fetch_one(&db)
                .await?;
        assert_eq!(actor, dest.owner);

        // Both owners edited the milestone since the initial restore.
        for (project, title) in [
            (dest.project, "Local schedule"),
            (source.project, "Revised source schedule"),
        ] {
            sqlx::query("UPDATE milestones SET title=$2,revision=revision+1 WHERE project_id=$1 AND title='Delivery gate'")
                .bind(project)
                .bind(title)
                .execute(&db)
                .await?;
        }
        let mut tx = db.begin().await?;
        let (files, updated) = export(&mut tx, source.owner, source.project).await.unwrap();
        let conflicts = restore(
            &mut tx,
            dest.owner,
            dest.project,
            source.project,
            &files,
            &updated,
            &mut ids,
            Mode::KeepLocal,
        )
        .await
        .unwrap();
        assert!(
            conflicts
                .iter()
                .any(|row| row.0.contains("milestone") && matches!(row.1, Action::Conflict))
        );
        tx.commit().await?;
        let local_title: String = sqlx::query_scalar(
            "SELECT title FROM milestones WHERE project_id=$1 AND target_on IS NOT NULL",
        )
        .bind(dest.project)
        .fetch_one(&db)
        .await?;
        assert_eq!(local_title, "Local schedule");
        Ok(())
    }

    async fn note(
        tx: &mut Transaction<'_, Postgres>,
        ws: &Workspace,
        path: &str,
        body: &str,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar("INSERT INTO notes(owner_id,project_id,title,path,body) VALUES($1,$2,$3,$3,$4) RETURNING id")
            .bind(ws.owner).bind(ws.project).bind(path).bind(body).fetch_one(&mut **tx).await
    }

    #[sqlx::test]
    async fn native_links_keep_authored_markdown_and_stable_destinations_after_reindex(
        db: PgPool,
    ) -> Result<(), sqlx::Error> {
        let source = workspace(&db).await?;
        let dest = workspace(&db).await?;
        let body = format!(
            "[[OLD-1]] [ticket](/issues/{}) [[Journal/Decision]] [[Decision]]",
            source.shipped
        );
        let mut tx = db.begin().await?;
        sqlx::query("UPDATE projects SET key='OLD' WHERE id=$1")
            .bind(source.project)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE projects SET key='NEW' WHERE id=$1")
            .bind(dest.project)
            .execute(&mut *tx)
            .await?;
        let from = note(&mut tx, &source, "Home.md", &body).await?;
        let decision = note(&mut tx, &source, "Journal/Decision.md", "original").await?;
        let other = note(&mut tx, &source, "Spare/Decision.md", "other").await?;
        crate::notes::reindex(&mut tx, source.owner, source.project)
            .await
            .unwrap();
        sqlx::query("UPDATE note_links SET dest_note=$1,manual=true WHERE source_id=$2 AND target='Decision'")
            .bind(decision).bind(from).execute(&mut *tx).await?;
        sqlx::query("UPDATE notes SET path='Archive/Moved.md',title='Moved' WHERE id=$1")
            .bind(decision)
            .execute(&mut *tx)
            .await?;
        crate::notes::reindex(&mut tx, source.owner, source.project)
            .await
            .unwrap();
        let (files, history) = export(&mut tx, source.owner, source.project).await.unwrap();

        let to = note(&mut tx, &dest, "Home.md", &body).await?;
        let moved = note(&mut tx, &dest, "Archive/Moved.md", "original").await?;
        let alternate = note(&mut tx, &dest, "Spare/Decision.md", "other").await?;
        let mut ids = HashMap::from([
            (source.shipped, dest.shipped),
            (from, to),
            (decision, moved),
            (other, alternate),
        ]);
        let old_lookups: Vec<String> =
            sqlx::query_scalar("SELECT raw FROM note_links WHERE source_id=$1 ORDER BY ordinal")
                .bind(from)
                .fetch_all(&mut *tx)
                .await?;
        assert_eq!(old_lookups.len(), 4);
        for iteration in 0..2 {
            crate::notes::reindex(&mut tx, dest.owner, dest.project)
                .await
                .unwrap();
            let actions = restore(
                &mut tx,
                dest.owner,
                dest.project,
                source.project,
                &files,
                &history,
                &mut ids,
                Mode::KeepLocal,
            )
            .await
            .unwrap();
            if iteration == 1 {
                assert!(actions.iter().all(|row| matches!(row.1, Action::Unchanged)));
            }
            let rows=sqlx::query_as::<_,(String,Option<Uuid>,Option<Uuid>,bool)>(
                "SELECT raw,dest_note,dest_issue,manual FROM note_links WHERE source_id=$1 ORDER BY ordinal"
            ).bind(to).fetch_all(&mut *tx).await?;
            assert_eq!(
                rows,
                vec![
                    (old_lookups[0].clone(), None, Some(dest.shipped), false),
                    (old_lookups[1].clone(), None, Some(dest.shipped), false),
                    (old_lookups[2].clone(), Some(moved), None, false),
                    (old_lookups[3].clone(), Some(moved), None, true),
                ]
            );
        }
        let stored: String = sqlx::query_scalar("SELECT body FROM notes WHERE id=$1")
            .bind(to)
            .fetch_one(&mut *tx)
            .await?;
        assert_eq!(stored, body);
        tx.commit().await
    }
    #[sqlx::test]
    async fn keeping_a_locally_edited_draft_keeps_its_snapshot_unpublished(
        db: PgPool,
    ) -> Result<(), sqlx::Error> {
        let source = workspace(&db).await?;
        let dest = workspace(&db).await?;
        let mut ids = HashMap::from([(source.shipped, dest.shipped)]);
        let mut tx = db.begin().await?;
        let release = draft(&mut tx, &source, "Beta").await?;
        let (files, history) = export(&mut tx, source.owner, source.project).await.unwrap();
        restore(
            &mut tx,
            dest.owner,
            dest.project,
            source.project,
            &files,
            &history,
            &mut ids,
            Mode::KeepLocal,
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE releases SET name='Edited draft',revision=revision+1 WHERE project_id=$1",
        )
        .bind(dest.project)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO release_entries(owner_id,project_id,release_id,issue_id,number,title,issue_revision) VALUES($1,$2,$3,$4,1,'Ship',1)")
            .bind(source.owner).bind(source.project).bind(release).bind(source.shipped).execute(&mut *tx).await?;
        sqlx::query("UPDATE releases SET published_on=current_date,published_at=now(),publish_command=gen_random_uuid(),revision=revision+1 WHERE id=$1").bind(release).execute(&mut *tx).await?;
        let (files, history) = export(&mut tx, source.owner, source.project).await.unwrap();
        let result = restore(
            &mut tx,
            dest.owner,
            dest.project,
            source.project,
            &files,
            &history,
            &mut ids,
            Mode::KeepLocal,
        )
        .await
        .unwrap();
        assert_eq!(
            result
                .iter()
                .filter(|item| matches!(item.1, Action::Conflict))
                .count(),
            2
        );
        let local:(String,bool,i64)=sqlx::query_as("SELECT name,published_at IS NULL,(SELECT count(*) FROM release_entries WHERE project_id=$1) FROM releases WHERE project_id=$1").bind(dest.project).fetch_one(&mut *tx).await?;
        assert_eq!(local, ("Edited draft".into(), true, 0));
        restore(
            &mut tx,
            dest.owner,
            dest.project,
            source.project,
            &files,
            &history,
            &mut ids,
            Mode::UseIncoming,
        )
        .await
        .unwrap();
        let restored:(String,bool,i64)=sqlx::query_as("SELECT name,published_at IS NOT NULL,(SELECT count(*) FROM release_entries WHERE project_id=$1) FROM releases WHERE project_id=$1").bind(dest.project).fetch_one(&mut *tx).await?;
        assert_eq!(restored, ("Beta".into(), true, 1));
        tx.commit().await
    }
}
