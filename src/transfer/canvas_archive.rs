//! The manifest records board identities. Each board or checkpoint carries its
//! own document so exporting a project's history only buffers one drawing at a time.
use super::{Tx, archive::ManifestFile, export::Sink, hash};
use crate::{AppError, AppResult};
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Serialize, Deserialize, FromRow)]
pub(super) struct BoardFile {
    pub title: String,
    pub document: Value,
}

impl BoardFile {
    pub fn fingerprint(&self) -> String {
        hash(json!([self.title, self.document]).to_string().as_bytes())
    }
}

#[derive(Serialize, Deserialize, FromRow)]
pub(super) struct CheckpointFile {
    pub name: String,
    pub title: String,
    pub document: Value,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub automatic: bool,
}

impl CheckpointFile {
    pub fn fingerprint(&self) -> String {
        hash(
            json!([
                self.name,
                self.title,
                self.document,
                self.created_at,
                self.automatic
            ])
            .to_string()
            .as_bytes(),
        )
    }
}

#[derive(FromRow)]
struct ExportBoard {
    id: Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[sqlx(flatten)]
    file: BoardFile,
}

#[derive(FromRow)]
struct ExportCheckpoint {
    id: Uuid,
    board_id: Uuid,
    #[sqlx(flatten)]
    file: CheckpointFile,
}

fn entry(
    id: Uuid,
    path: String,
    kind: &str,
    data: Value,
    file: &impl Serialize,
) -> AppResult<(ManifestFile, Vec<u8>)> {
    let bytes = serde_json::to_vec(file)
        .map_err(|_| AppError::Internal("Could not encode a canvas document.".into()))?;
    Ok((
        ManifestFile {
            id,
            path,
            kind: kind.into(),
            sha256: hash(&bytes),
            data,
        },
        bytes,
    ))
}

pub(super) async fn stream(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    sink: &Sink,
) -> AppResult<()> {
    {
        let mut boards = sqlx::query_as::<_, ExportBoard>(
            "SELECT id,created_at,updated_at,title,document FROM canvas_boards
             WHERE owner_id=$1 AND project_id=$2 ORDER BY id",
        )
        .bind(owner)
        .bind(project)
        .fetch(&mut **tx);
        while let Some(board) = boards.try_next().await? {
            let (file, bytes) = entry(
                board.id,
                format!("Canvases/{}.json", board.id),
                "canvas",
                json!({"created_at":board.created_at,"updated_at":board.updated_at,
                       "base_state":board.file.fingerprint()}),
                &board.file,
            )?;
            sink.file(file, bytes).await?;
        }
    }
    let mut checkpoints = sqlx::query_as::<_, ExportCheckpoint>(
        "SELECT id,board_id,name,title,document,created_at,automatic FROM canvas_checkpoints
         WHERE owner_id=$1 AND project_id=$2 ORDER BY board_id,created_at,id",
    )
    .bind(owner)
    .bind(project)
    .fetch(&mut **tx);
    while let Some(checkpoint) = checkpoints.try_next().await? {
        let (file, bytes) = entry(
            checkpoint.id,
            format!(
                "Canvases/{}/Checkpoints/{}.json",
                checkpoint.board_id, checkpoint.id
            ),
            "canvas_checkpoint",
            json!({"board_id":checkpoint.board_id, "base_state":checkpoint.file.fingerprint()}),
            &checkpoint.file,
        )?;
        sink.file(file, bytes).await?;
    }
    Ok(())
}
