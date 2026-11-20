//! The snapshot feeds a bounded queue. The ZIP, manifest and event log live in
//! anonymous temporary files, including while the HTTP client reads the result.

use super::{Tx, archive, history, native_issues, reserve, vault};
use crate::{AppError, AppResult, AppState, auth::Auth};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, header},
    response::Response,
};
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    fs::File,
    io::{self, BufWriter, Seek, Write},
};
use tokio::sync::mpsc;
use tokio_util::io::ReaderStream;
use uuid::Uuid;
use zip::{ZipWriter, write::SimpleFileOptions};

enum Record {
    File(archive::ManifestFile, Vec<u8>),
    ReadingCopy(String, Vec<u8>),
    Event(Value),
    Finish,
}

pub(super) struct Sink(mpsc::Sender<Record>);

impl Sink {
    async fn send(&self, record: Record) -> AppResult<()> {
        self.0.send(record).await.map_err(|_| {
            AppError::Unavailable("Could not write the export to temporary storage.".into())
        })
    }

    pub(super) async fn file(
        &self,
        metadata: archive::ManifestFile,
        bytes: Vec<u8>,
    ) -> AppResult<()> {
        self.send(Record::File(metadata, bytes)).await
    }

    pub(super) async fn reading_copy(&self, path: String, bytes: Vec<u8>) -> AppResult<()> {
        self.send(Record::ReadingCopy(path, bytes)).await
    }

    pub(super) async fn event(&self, event: Value) -> AppResult<()> {
        self.send(Record::Event(event)).await
    }
}

fn storage_error(_: impl std::fmt::Display) -> AppError {
    AppError::Unavailable("Temporary storage for this export is unavailable.".into())
}

fn start_entry(zip: &mut ZipWriter<File>, path: &str, length: u64) -> AppResult<()> {
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600)
        .large_file(length >= u64::from(u32::MAX));
    if !archive::valid_path(path) {
        return Err(AppError::Internal(
            "Project contains an invalid export path.".into(),
        ));
    }
    zip.start_file(path, options).map_err(storage_error)
}

fn append_spool(zip: &mut ZipWriter<File>, path: &str, spool: BufWriter<File>) -> AppResult<()> {
    let mut file = spool.into_inner().map_err(storage_error)?;
    let length = file.metadata().map_err(storage_error)?.len();
    file.rewind().map_err(storage_error)?;
    start_entry(zip, path, length)?;
    io::copy(&mut file, zip).map_err(storage_error)?;
    Ok(())
}

fn write_zip(project: Uuid, mut records: mpsc::Receiver<Record>) -> AppResult<Option<File>> {
    let mut zip = ZipWriter::new(tempfile::tempfile().map_err(storage_error)?);
    let mut manifest = BufWriter::new(tempfile::tempfile().map_err(storage_error)?);
    let mut history = BufWriter::new(tempfile::tempfile().map_err(storage_error)?);
    write!(
        manifest,
        "{{\"version\":1,\"project\":\"{project}\",\"files\":["
    )
    .map_err(storage_error)?;
    let mut first = true;
    while let Some(record) = records.blocking_recv() {
        match record {
            Record::File(metadata, bytes) => {
                start_entry(&mut zip, &metadata.path, bytes.len() as u64)?;
                zip.write_all(&bytes).map_err(storage_error)?;
                if !first {
                    manifest.write_all(b",").map_err(storage_error)?;
                }
                first = false;
                serde_json::to_writer(&mut manifest, &metadata).map_err(storage_error)?;
            }
            Record::ReadingCopy(path, bytes) => {
                start_entry(&mut zip, &path, bytes.len() as u64)?;
                zip.write_all(&bytes).map_err(storage_error)?;
            }
            Record::Event(row) => {
                serde_json::to_writer(&mut history, &row).map_err(storage_error)?;
                history.write_all(b"\n").map_err(storage_error)?;
            }
            Record::Finish => {
                manifest.write_all(b"]}").map_err(storage_error)?;
                append_spool(&mut zip, "history.jsonl", history)?;
                append_spool(&mut zip, "manifest.json", manifest)?;
                let mut file = zip.finish().map_err(storage_error)?;
                file.rewind().map_err(storage_error)?;
                return Ok(Some(file));
            }
        }
    }
    // The snapshot failed or the request was cancelled. Drop all partial files.
    Ok(None)
}

async fn snapshot(tx: &mut Tx<'_>, owner: Uuid, project: Uuid, sink: &Sink) -> AppResult<()> {
    vault::stream(tx, owner, project, sink).await?;
    native_issues::stream(tx, owner, project, sink).await?;
    history::stream(tx, owner, project, sink).await
}

pub(super) async fn download(
    auth: Auth,
    State(state): State<AppState>,
    Path(project): Path<Uuid>,
) -> AppResult<Response> {
    let permit = reserve(auth.user_id).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects WHERE owner_id=$1 AND id=$2)")
            .bind(auth.user_id)
            .bind(project)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Err(AppError::NotFound);
    }

    let (sender, receiver) = mpsc::channel(1);
    let sink = Sink(sender);
    let writer = tokio::task::spawn_blocking(move || write_zip(project, receiver));
    let produced = async {
        snapshot(&mut tx, auth.user_id, project, &sink).await?;
        tx.commit().await?;
        sink.send(Record::Finish).await
    }
    .await;
    drop(sink);
    let file = writer.await.map_err(storage_error)??;
    produced?;
    let file = tokio::fs::File::from_std(file.ok_or_else(|| storage_error("incomplete"))?);
    let length = file.metadata().await.map_err(storage_error)?.len();
    // Keep a transfer slot until the client consumes or drops the body, bounding
    // both work in progress and retained disk space for slow downloads.
    let stream = ReaderStream::with_capacity(file, 64 * 1024);
    let body = Body::from_stream(stream.map(move |chunk| {
        let _keep_slot = &permit;
        chunk
    }));
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"ratio-project-{project}.zip\""
        ))
        .map_err(|_| AppError::Internal("Invalid export filename.".into()))?,
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[tokio::test]
    async fn export_spools_a_project_larger_than_the_upload_limit() {
        let project = Uuid::new_v4();
        let (sender, receiver) = mpsc::channel(1);
        let sink = Sink(sender);
        let writer = tokio::task::spawn_blocking(move || write_zip(project, receiver));
        let payload = vec![b'x'; 1024 * 1024];
        let checksum = super::super::hash(&payload);
        let count = archive::MAX_UPLOAD / payload.len() + 1;
        for n in 0..count {
            let metadata = archive::ManifestFile {
                id: Uuid::new_v4(),
                path: format!("Notes/note-{n}.md"),
                kind: "note".into(),
                sha256: checksum.clone(),
                data: Value::Null,
            };
            sink.file(metadata, payload.clone()).await.unwrap();
        }
        sink.event(serde_json::json!({"type":"event","data":{}}))
            .await
            .unwrap();
        sink.send(Record::Finish).await.unwrap();
        let file = writer.await.unwrap().unwrap().unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        assert_eq!(zip.len(), count + 2);
        let manifest: archive::Manifest =
            serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
        assert_eq!(manifest.project, project);
        assert_eq!(manifest.files.len(), count);
        for file in manifest.files {
            assert_eq!(file.sha256, checksum);
            let mut member = zip.by_name(&file.path).unwrap();
            assert_eq!(
                io::copy(&mut member, &mut io::sink()).unwrap(),
                payload.len() as u64
            );
        }
        let mut line = String::new();
        zip.by_name("history.jsonl")
            .unwrap()
            .read_to_string(&mut line)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["type"],
            "event"
        );
    }

    #[tokio::test]
    async fn interrupted_producer_discards_partial_archive() {
        let (sender, receiver) = mpsc::channel(1);
        let task = tokio::task::spawn_blocking(move || write_zip(Uuid::new_v4(), receiver));
        drop(sender);
        assert!(task.await.unwrap().unwrap().is_none());
    }
}
