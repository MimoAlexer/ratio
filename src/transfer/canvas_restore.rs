//! Restore board files after their issue and note destinations have been assigned.
use super::archive::{Manifest, ManifestFile};
use super::canvas_archive::{BoardFile, CheckpointFile};
use super::{Action, Mode, Report, Tx, bad, history};
use crate::canvas::{
    CHECKPOINT_LIMIT, RECOVERY_CHECKPOINT_LIMIT,
    model::{Document, SourceKind},
};
use crate::{AppError, AppResult};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub(super) struct Destination {
    owner: Uuid,
    project: Uuid,
    source_project: Uuid,
    sources: HashMap<(SourceKind, Uuid), Uuid>,
}

impl Destination {
    pub(super) async fn new(
        tx: &mut Tx<'_>,
        owner: Uuid,
        project: Uuid,
        manifest: &Manifest,
        ids: &HashMap<Uuid, Uuid>,
    ) -> AppResult<Self> {
        let mut sources = HashMap::new();
        // A UUID mapping is usable only for the correct type and destination project.
        for (kind, table) in [(SourceKind::Issue, "issues"), (SourceKind::Note, "notes")] {
            let pairs: Vec<_> = manifest
                .files
                .iter()
                .filter(|f| f.kind == kind.as_str())
                .filter_map(|f| ids.get(&f.id).map(|&id| (f.id, id)))
                .collect();
            let candidate_ids: Vec<_> = pairs.iter().map(|&(_, dest)| dest).collect();
            let permitted: HashSet<Uuid> = sqlx::query_scalar(&format!(
                "SELECT id FROM {table} WHERE owner_id=$1 AND project_id=$2 AND id=ANY($3)"
            ))
            .bind(owner)
            .bind(project)
            .bind(candidate_ids)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .collect();
            for (from, to) in pairs {
                if permitted.contains(&to) {
                    sources.insert((kind, from), to);
                }
            }
        }
        Ok(Self {
            owner,
            project,
            source_project: manifest.project,
            sources,
        })
    }

    fn id(&self, kind: &str, source_id: Uuid) -> Uuid {
        if self.source_project == self.project {
            source_id
        } else {
            history::target_id(
                self.owner,
                self.project,
                self.source_project,
                kind,
                source_id,
            )
        }
    }

    fn remap_document(&self, raw: &mut Value) -> AppResult<usize> {
        let document =
            Document::deserialize(&*raw).map_err(|_| bad("Invalid planning canvas document."))?;
        document.validate()?;
        let nodes = raw["nodes"]
            .as_array_mut()
            .ok_or_else(|| bad("Missing canvas nodes."))?;
        let mut detached = 0;
        for (node, source) in nodes
            .iter_mut()
            .zip(document.nodes.into_iter().map(|n| n.source))
        {
            let Some(source) = source else { continue };
            if let Some(id) = self.sources.get(&(source.kind, source.id)) {
                node["source"]["id"] = Value::String(id.to_string());
            } else {
                node.as_object_mut()
                    .expect("deserialized canvas node")
                    .remove("source");
                detached += 1;
            }
        }
        Ok(detached)
    }

    pub(super) async fn restore(
        &self,
        tx: &mut Tx<'_>,
        mut files: Vec<(&ManifestFile, Vec<u8>)>,
        mode: Mode,
        report: &mut Report,
    ) -> AppResult<()> {
        files.sort_by_key(|(metadata, _)| metadata.kind == "canvas_checkpoint");
        let mut affected_boards = HashSet::new();
        for (metadata, bytes) in files {
            let id = self.id(&metadata.kind, metadata.id);
            let mut record = Record::decode(metadata, &bytes, self)?;
            let original = record.fingerprint();
            let detached = self.remap_document(record.document_mut())?;
            if detached > 0 {
                let pins = if detached == 1 {
                    "source pin"
                } else {
                    "source pins"
                };
                report.warnings.push(format!(
                    "The incoming file {} has {detached} unavailable {pins}. Importing it keeps the shapes and text without those links.",
                    metadata.path
                ));
            }
            let incoming = record.fingerprint();
            let local = record.current(tx, self, id).await?;
            let key = metadata.id.to_string();
            let receipt: Option<(String,String)> = sqlx::query_as(
                "SELECT source_hash, local_hash FROM transfer_history_receipts
                 WHERE owner_id=$1 AND project_id=$2 AND source_project=$3 AND record_type=$4 AND source_key=$5"
            ).bind(self.owner).bind(self.project).bind(self.source_project).bind(&metadata.kind).bind(&key)
             .fetch_optional(&mut **tx).await?;
            let base = metadata.data.get("base_state").and_then(Value::as_str);
            let action = classify(
                local.as_deref(),
                &incoming,
                &original,
                receipt.as_ref(),
                base.filter(|_| self.source_project == self.project),
            );
            let write = mode != Mode::Preview
                && (matches!(action, Action::Create | Action::Update)
                    || action == Action::Conflict && mode == Mode::UseIncoming);
            if write {
                record.write(tx, self, id, action == Action::Create).await?;
                if let Record::Checkpoint { board, .. } = &record {
                    affected_boards.insert(*board);
                }
            }
            if mode != Mode::Preview && (write || local.as_deref() == Some(&incoming)) {
                sqlx::query("INSERT INTO transfer_history_receipts(owner_id,project_id,source_project,record_type,source_key,source_hash,local_hash)
                    VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(owner_id,project_id,source_project,record_type,source_key)
                    DO UPDATE SET source_hash=excluded.source_hash,local_hash=excluded.local_hash
                    WHERE (transfer_history_receipts.source_hash,transfer_history_receipts.local_hash)
                        IS DISTINCT FROM (excluded.source_hash,excluded.local_hash)")
                    .bind(self.owner).bind(self.project).bind(self.source_project).bind(&metadata.kind)
                    .bind(key).bind(original).bind(incoming).execute(&mut **tx).await?;
            }
            report.row(metadata.path.clone(), action, 1);
        }
        if !affected_boards.is_empty() {
            let over: bool = sqlx::query_scalar("SELECT EXISTS(
                SELECT board_id FROM canvas_checkpoints WHERE owner_id=$1 AND project_id=$2 AND board_id=ANY($3)
                GROUP BY board_id HAVING count(*) FILTER (WHERE NOT automatic)>$4 OR count(*) FILTER (WHERE automatic)>$5)")
                .bind(self.owner).bind(self.project).bind(affected_boards.into_iter().collect::<Vec<_>>())
                .bind(CHECKPOINT_LIMIT).bind(RECOVERY_CHECKPOINT_LIMIT)
                .fetch_one(&mut **tx).await?;
            if over {
                return Err(AppError::Conflict("Restoring these canvases would exceed a board’s checkpoint history. Import into a new project to keep both histories.".into()));
            }
        }
        Ok(())
    }
}

fn classify(
    local: Option<&str>,
    incoming: &str,
    original: &str,
    receipt: Option<&(String, String)>,
    archive_base: Option<&str>,
) -> Action {
    let Some(current) = local else {
        return Action::Create;
    };
    if current == incoming {
        return Action::Unchanged;
    }
    if archive_base == Some(current) {
        return Action::Update;
    }
    if let Some((source, applied)) = receipt {
        if current == applied {
            return Action::Update;
        }
        if source == original {
            return Action::Unchanged;
        }
        return Action::Conflict;
    }
    if archive_base == Some(original) {
        Action::Unchanged
    } else {
        Action::Conflict
    }
}

#[derive(Deserialize)]
struct BoardTimes {
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

enum Record {
    Board { data: BoardFile, dates: BoardTimes },
    Checkpoint { data: CheckpointFile, board: Uuid },
}

impl Record {
    fn decode(meta: &ManifestFile, bytes: &[u8], destination: &Destination) -> AppResult<Self> {
        fn label(value: &str, max: usize) -> bool {
            !value.is_empty()
                && value.trim() == value
                && value.chars().count() <= max
                && !value.chars().any(char::is_control)
        }
        if meta.kind == "canvas" {
            let data: BoardFile =
                serde_json::from_slice(bytes).map_err(|_| bad("Invalid canvas file."))?;
            let dates: BoardTimes = serde_json::from_value(meta.data.clone())
                .map_err(|_| bad("Invalid canvas dates."))?;
            if !label(&data.title, 120)
                || matches!((dates.created_at, dates.updated_at),(Some(a),Some(b)) if a>b)
            {
                return Err(bad("Invalid canvas title or dates."));
            }
            Ok(Self::Board { data, dates })
        } else {
            let data: CheckpointFile =
                serde_json::from_slice(bytes).map_err(|_| bad("Invalid canvas checkpoint."))?;
            let board: Uuid = serde_json::from_value(meta.data["board_id"].clone())
                .map_err(|_| bad("Missing canvas checkpoint board."))?;
            if !label(&data.name, 80) || !label(&data.title, 120) {
                return Err(bad("Invalid canvas checkpoint name or title."));
            }
            Ok(Self::Checkpoint {
                data,
                board: destination.id("canvas", board),
            })
        }
    }
    fn fingerprint(&self) -> String {
        match self {
            Self::Board { data, .. } => data.fingerprint(),
            Self::Checkpoint { data, .. } => data.fingerprint(),
        }
    }
    fn document_mut(&mut self) -> &mut Value {
        match self {
            Self::Board { data, .. } => &mut data.document,
            Self::Checkpoint { data, .. } => &mut data.document,
        }
    }
    async fn current(
        &self,
        tx: &mut Tx<'_>,
        dest: &Destination,
        id: Uuid,
    ) -> AppResult<Option<String>> {
        let hash = match self {
            Self::Board { .. } => {
                let local = sqlx::query_as::<_, BoardFile>(
                    "SELECT title,document FROM canvas_boards
                     WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE",
                )
                .bind(dest.owner)
                .bind(dest.project)
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
                local.map(|board| board.fingerprint())
            }
            Self::Checkpoint { board, .. } => {
                let parent = sqlx::query_scalar::<_,Uuid>("SELECT id FROM canvas_boards WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE")
                    .bind(dest.owner).bind(dest.project).bind(board).fetch_optional(&mut **tx).await?;
                if parent.is_none() {
                    return Err(bad(
                        "A canvas checkpoint has no destination board. Import its board file together with the checkpoint.",
                    ));
                }
                #[derive(sqlx::FromRow)]
                struct StoredCheckpoint {
                    board_id: Uuid,
                    #[sqlx(flatten)]
                    file: CheckpointFile,
                }
                let local = sqlx::query_as::<_, StoredCheckpoint>(
                    "SELECT board_id,name,title,document,created_at,automatic FROM canvas_checkpoints
                     WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE"
                ).bind(dest.owner).bind(dest.project).bind(id).fetch_optional(&mut **tx).await?;
                if local.as_ref().is_some_and(|saved| saved.board_id != *board) {
                    return Err(bad("A saved checkpoint belongs to another board."));
                }
                local.map(|saved| saved.file.fingerprint())
            }
        };
        Ok(hash)
    }
    async fn write(
        &self,
        tx: &mut Tx<'_>,
        d: &Destination,
        id: Uuid,
        create: bool,
    ) -> AppResult<()> {
        match self {
            Self::Board { data, dates } if create => {
                sqlx::query("INSERT INTO canvas_boards(id,owner_id,project_id,title,document,created_at,updated_at)
                    VALUES($1,$2,$3,$4,$5,coalesce($6,now()),coalesce($7,$6,now()))")
                    .bind(id).bind(d.owner).bind(d.project).bind(&data.title).bind(&data.document)
                    .bind(dates.created_at).bind(dates.updated_at).execute(&mut **tx).await?;
            }
            Self::Board { data, .. } => {
                sqlx::query("UPDATE canvas_boards SET title=$4,document=$5,revision=revision+1,updated_at=now()
                    WHERE owner_id=$1 AND project_id=$2 AND id=$3")
                    .bind(d.owner).bind(d.project).bind(id).bind(&data.title).bind(&data.document).execute(&mut **tx).await?;
            }
            Self::Checkpoint { data, board } if create => {
                sqlx::query("INSERT INTO canvas_checkpoints(id,owner_id,project_id,board_id,name,title,document,created_at,automatic)
                    VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                    .bind(id).bind(d.owner).bind(d.project).bind(board).bind(&data.name).bind(&data.title)
                    .bind(&data.document).bind(data.created_at).bind(data.automatic).execute(&mut **tx).await?;
            }
            Self::Checkpoint { data, board } => {
                sqlx::query("UPDATE canvas_checkpoints SET name=$5,title=$6,document=$7,created_at=$8,automatic=$9
                    WHERE owner_id=$1 AND project_id=$2 AND board_id=$3 AND id=$4")
                    .bind(d.owner).bind(d.project).bind(board).bind(id).bind(&data.name).bind(&data.title)
                    .bind(&data.document).bind(data.created_at).bind(data.automatic).execute(&mut **tx).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
