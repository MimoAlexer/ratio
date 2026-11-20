mod archive;
mod export;
mod history;
mod linear;
mod native_issues;
mod trash;
pub use trash::purge_expired;
mod vault;
mod views;

use crate::{AppError, AppResult, AppState, auth::Auth, store};
use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Acquire, FromRow, Postgres, Transaction};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

type Tx<'a> = Transaction<'a, Postgres>;
static TRANSFERS: Semaphore = Semaphore::const_new(2);
static OWNER_SLOTS: LazyLock<Mutex<HashMap<Uuid, Weak<Semaphore>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
struct TransferSlot {
    _account: OwnedSemaphorePermit,
    _process: SemaphorePermit<'static>,
}
async fn reserve(owner: Uuid) -> AppResult<TransferSlot> {
    let limit = {
        let mut owners = OWNER_SLOTS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        owners.retain(|_, slot| slot.strong_count() > 0);
        if let Some(existing) = owners.get(&owner).and_then(Weak::upgrade) {
            existing
        } else {
            let semaphore = Arc::new(Semaphore::new(1));
            owners.insert(owner, Arc::downgrade(&semaphore));
            semaphore
        }
    };
    let account = timeout(Duration::from_secs(5), limit.acquire_owned())
        .await
        .map_err(|_| {
            AppError::Unavailable(
                "A transfer is already running in your workspace. Try again shortly.".into(),
            )
        })?
        .map_err(|_| AppError::Unavailable("Transfers are unavailable".into()))?;
    let process = TRANSFERS.try_acquire().map_err(|_| {
        AppError::Unavailable("Two transfers are running. Try again shortly.".into())
    })?;
    Ok(TransferSlot {
        _account: account,
        _process: process,
    })
}
fn bad(s: impl Into<String>) -> AppError {
    AppError::BadRequest(s.into())
}
fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Preview,
    KeepLocal,
    UseIncoming,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Create,
    Update,
    Conflict,
    Unchanged,
}
impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::Create => "Create",
            Self::Update => "Update",
            Self::Conflict => "Conflict",
            Self::Unchanged => "Unchanged",
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Report {
    rows: Vec<(String, String)>,
    warnings: Vec<String>,
    fields: Vec<String>,
    conflicts: usize,
    created: usize,
    updated: usize,
    unchanged: usize,
    growth: i64,
}
impl Report {
    fn row(&mut self, name: impl Into<String>, action: Action, count: usize) {
        match action {
            Action::Create => self.created += count,
            Action::Update => self.updated += count,
            Action::Conflict => self.conflicts += count,
            Action::Unchanged => self.unchanged += count,
        }
        self.rows.push((name.into(), action.label().into()));
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/transfer", get(views::index))
        .route("/transfer/export/{project}", get(export::download))
        .route(
            "/transfer/preview",
            post(preview).layer(DefaultBodyLimit::max(archive::MAX_UPLOAD + 32 * 1024)),
        )
        .route("/transfer/review/{id}", get(views::review))
        .route("/transfer/commit/{id}", post(commit))
        .route("/transfer/assets/{id}", get(asset))
        .route("/transfer/trash", get(trash::index))
        .route("/transfer/assets/{id}/trash", post(trash::move_to_trash))
        .route("/transfer/assets/{id}/restore", post(trash::restore))
        .route(
            "/transfer/assets/{id}/delete",
            get(trash::delete_confirmation).post(trash::delete),
        )
        .route("/transfer/media/{project}/{*path}", get(media))
}

struct Upload {
    project: Uuid,
    format: String,
    folder: String,
    file: axum::body::Bytes,
}
async fn upload(auth: &Auth, mut multipart: Multipart) -> AppResult<Upload> {
    let mut text = HashMap::new();
    let mut file = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad("Unreadable upload or request exceeds 64 MiB"))?
    {
        let name = field.name().unwrap_or("").to_owned();
        if name == "file" {
            if file.is_some() {
                return Err(bad("Select one file"));
            }
            let b = field
                .bytes()
                .await
                .map_err(|_| bad("Unreadable file or upload exceeds 64 MiB"))?;
            if b.is_empty() || b.len() > archive::MAX_UPLOAD {
                return Err(bad("Choose a nonempty file of at most 64 MiB"));
            }
            file = Some(b);
        } else {
            if !["csrf", "project", "format", "folder"].contains(&name.as_str())
                || text.contains_key(&name)
            {
                return Err(bad("Unexpected or repeated upload field"));
            }
            let mut field = field;
            let mut v = Vec::new();
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|_| bad("Unreadable upload field"))?
            {
                if v.len() + chunk.len() > 1024 {
                    return Err(bad("Upload field too long"));
                }
                v.extend_from_slice(&chunk);
            }
            text.insert(
                name,
                String::from_utf8(v).map_err(|_| bad("Invalid upload field"))?,
            );
        }
    }
    auth.check_csrf(text.get("csrf").map(String::as_str).unwrap_or(""))?;
    let project = text
        .remove("project")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("Choose a project"))?;
    let format = text
        .remove("format")
        .filter(|v| v == "vault" || v == "linear")
        .ok_or_else(|| bad("Choose a file format"))?;
    let folder = text.remove("folder").unwrap_or_default();
    if format == "linear" && !folder.is_empty() {
        return Err(bad("Folder selection applies to ZIP files"));
    }
    Ok(Upload {
        format,
        folder,
        project,
        file: file.ok_or_else(|| bad("Choose a file to import"))?,
    })
}

enum Source {
    Vault(archive::Package),
    Linear(linear::Csv),
}
async fn decode(
    format: &str,
    folder: &str,
    bytes: axum::body::Bytes,
    image_cap: i64,
) -> AppResult<Source> {
    let (format, folder) = (format.to_owned(), folder.to_owned());
    tokio::task::spawn_blocking(move || match format.as_str() {
        "vault" => archive::parse(&bytes, &folder, image_cap).map(Source::Vault),
        "linear" => linear::parse(&bytes).map(Source::Linear),
        _ => Err(bad("Unknown file format")),
    })
    .await
    .map_err(|_| AppError::Internal("Could not read upload".into()))?
}

// Missing files alone never remove a note or issue. If this destination already
// owns the matching record, restored history may still refer to it safely.
async fn map_missing(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    manifest: &archive::Manifest,
    ids: &mut HashMap<Uuid, Uuid>,
) -> AppResult<()> {
    for (kind, table) in [("issue", "issues"), ("note", "notes")] {
        let missing: Vec<_> = manifest
            .files
            .iter()
            .filter(|f| f.kind == kind && !ids.contains_key(&f.id))
            .map(|f| f.id)
            .collect();
        if missing.is_empty() {
            continue;
        }
        let sql = format!(
            "SELECT r.source_id,t.id FROM transfer_records r JOIN {table} t ON t.owner_id=r.owner_id AND t.project_id=r.project_id AND t.id=r.local_id WHERE r.owner_id=$1 AND r.project_id=$2 AND r.kind=$3 AND r.source_id=ANY($4)"
        );
        let mapped: Vec<(Uuid, Uuid)> = sqlx::query_as(&sql)
            .bind(owner)
            .bind(project)
            .bind(kind)
            .bind(&missing)
            .fetch_all(&mut **tx)
            .await?;
        ids.extend(mapped);
        if project == manifest.project {
            let sql = format!(
                "SELECT id FROM {table} WHERE owner_id=$1 AND project_id=$2 AND id=ANY($3)"
            );
            for id in sqlx::query_scalar::<_, Uuid>(&sql)
                .bind(owner)
                .bind(project)
                .bind(&missing)
                .fetch_all(&mut **tx)
                .await?
            {
                ids.entry(id).or_insert(id);
            }
        }
    }
    Ok(())
}

async fn apply_vault(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    pack: archive::Package,
    mode: Mode,
) -> AppResult<Report> {
    let mut report = Report {
        warnings: pack.warnings,
        ..Report::default()
    };
    report
        .fields
        .push("Markdown source, note paths and supported images".into());
    if pack.manifest.is_some() {
        report
            .fields
            .push("Native issue, roadmap and release records, with recorded timestamps".into());
    }
    let manifest = pack.manifest.as_ref();
    let files: HashMap<_, _> = manifest
        .into_iter()
        .flat_map(|m| &m.files)
        .map(|f| (f.path.as_str(), f))
        .collect();
    let mut ids = HashMap::new();
    let mut assigned = HashSet::new();
    let mut releases = Vec::new();
    let mut note_ids = Vec::new();
    for entry in pack.entries {
        let meta = files.get(entry.path.as_str()).copied();
        let (action, id) = match entry.kind.as_str() {
            "note" | "blob" => {
                vault::apply(tx, owner, project, &entry, manifest.is_some(), meta, mode).await?
            }
            "issue" => {
                native_issues::apply(
                    tx,
                    owner,
                    project,
                    meta.ok_or_else(|| bad("Missing issue metadata"))?,
                    &entry.bytes,
                    mode,
                )
                .await?
            }
            "release" => {
                releases.push((
                    meta.ok_or_else(|| bad("Missing release metadata"))?.clone(),
                    entry.bytes,
                ));
                continue;
            }
            _ => return Err(bad("Unexpected archive record")),
        };
        if !assigned.insert((entry.kind.clone(), id)) {
            return Err(bad(format!(
                "Multiple files resolve to the same record: {}",
                entry.path
            )));
        }
        if let Some(meta) = meta {
            ids.insert(meta.id, id);
        }
        if entry.kind == "note" {
            note_ids.push(id);
        }
        report.row(entry.path, action, 1);
    }
    crate::notes::reindex(tx, owner, project).await?;
    if let Some(m) = manifest {
        map_missing(tx, owner, project, m, &mut ids).await?;
        for (name, action, count) in history::restore(
            tx,
            owner,
            project,
            m.project,
            &releases,
            &pack.history,
            &mut ids,
            mode,
        )
        .await?
        {
            report.row(name, action, count);
        }
    }
    if !note_ids.is_empty() {
        report
            .warnings
            .extend(crate::notes::unresolved_warnings(tx, owner, project, &note_ids).await?);
    }
    Ok(report)
}

async fn apply(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    source: Source,
    mode: Mode,
    cap: i64,
) -> AppResult<Report> {
    let before = store::storage_used_tx(tx, owner).await?;
    let mut r = match source {
        Source::Vault(pack) => apply_vault(tx, owner, project, pack, mode).await?,
        Source::Linear(csv) => {
            let m = if mode == Mode::UseIncoming {
                linear::Mode::UseIncoming
            } else {
                linear::Mode::KeepLocal
            };
            let r = linear::process(tx, owner, project, &csv, m).await?;
            crate::notes::reindex(tx, owner, project).await?;
            let mut warnings = csv.warnings;
            warnings.extend(r.warnings);
            Report {
                rows: r.rows,
                warnings,
                fields: csv.fields,
                conflicts: r.conflicts,
                created: r.created,
                updated: r.updated,
                unchanged: r.unchanged,
                growth: 0,
            }
        }
    };
    let after = store::storage_used_tx(tx, owner).await?;
    r.growth = after - before;
    if r.growth > 0 && after > cap {
        return Err(AppError::Conflict(format!(
            "This import would use {after} bytes. Your storage limit is {cap} bytes."
        )));
    }
    Ok(r)
}

async fn lock(tx: &mut Tx<'_>, owner: Uuid, project: Uuid) -> AppResult<store::Limits> {
    let plan: String = sqlx::query_scalar("SELECT plan FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?;
    store::lock_project(tx, owner, project).await?;
    Ok(store::limits(&plan))
}

async fn preview(
    auth: Auth,
    State(s): State<AppState>,
    multipart: Multipart,
) -> AppResult<Redirect> {
    let _slot = reserve(auth.user_id).await?;
    let input = timeout(Duration::from_secs(120), upload(&auth, multipart))
        .await
        .map_err(|_| bad("Upload exceeded the two-minute deadline. Try again."))??;
    let mut tx = s.db.begin().await?;
    let limits = lock(&mut tx, auth.user_id, input.project).await?;
    let source = decode(
        &input.format,
        &input.folder,
        input.file.clone(),
        limits.image_bytes,
    )
    .await?;
    // Validate against the real destination, including quotas, links and history dependencies.
    // Discard all simulated data before retaining the review and its original upload.
    let mut simulation = tx.begin().await?;
    let report = apply(
        &mut simulation,
        auth.user_id,
        input.project,
        source,
        Mode::KeepLocal,
        limits.storage_bytes,
    )
    .await?;
    simulation.rollback().await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO transfer_previews(id,owner_id,project_id,format,folder,payload,report) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(owner_id) DO UPDATE SET id=EXCLUDED.id,project_id=EXCLUDED.project_id,format=EXCLUDED.format,folder=EXCLUDED.folder,payload=EXCLUDED.payload,report=EXCLUDED.report,stale=false,expires_at=EXCLUDED.expires_at")
        .bind(id).bind(auth.user_id).bind(input.project).bind(input.format).bind(input.folder).bind(input.file.as_ref()).bind(sqlx::types::Json(report)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/transfer/review/{id}")))
}

#[derive(Deserialize)]
struct Decision {
    csrf: String,
    decision: String,
}
#[derive(FromRow)]
struct Pending {
    project_id: Uuid,
    format: String,
    folder: String,
    payload: Option<Vec<u8>>,
    stale: bool,
    expires_at: DateTime<Utc>,
}
async fn commit(
    auth: Auth,
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Form(d): Form<Decision>,
) -> AppResult<Redirect> {
    auth.check_csrf(&d.csrf)?;
    let mode = match d.decision.as_str() {
        "local" => Mode::KeepLocal,
        "incoming" => Mode::UseIncoming,
        _ => return Err(bad("Choose a conflict resolution")),
    };
    let _slot = reserve(auth.user_id).await?;
    let mut tx = s.db.begin().await?;
    let project: Uuid =
        sqlx::query_scalar("SELECT project_id FROM transfer_previews WHERE owner_id=$1 AND id=$2")
            .bind(auth.user_id)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
    let limits = lock(&mut tx, auth.user_id, project).await?;
    let p=sqlx::query_as::<_,Pending>("SELECT project_id,format,folder,payload,stale,expires_at FROM transfer_previews WHERE owner_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE").bind(auth.user_id).bind(project).bind(id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
    let redirect = Redirect::to(&format!("/transfer/review/{id}"));
    let Some(bytes) = p.payload else {
        return Ok(redirect);
    };
    if p.stale || p.expires_at <= Utc::now() {
        return Err(AppError::Conflict("The project changed or this preview expired. Upload again to review the current changes.".into()));
    }
    let source = decode(&p.format, &p.folder, bytes.into(), limits.image_bytes).await?;
    let report = apply(
        &mut tx,
        auth.user_id,
        p.project_id,
        source,
        mode,
        limits.storage_bytes,
    )
    .await?;
    sqlx::query("UPDATE transfer_previews SET payload=NULL,report=$1,stale=false,expires_at=now()+interval '1 day' WHERE id=$2 AND owner_id=$3").bind(sqlx::types::Json(report)).bind(id).bind(auth.user_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(redirect)
}

fn image_response(record: Option<(String, Vec<u8>)>) -> AppResult<Response> {
    let (mime, bytes) = record.ok_or(AppError::NotFound)?;
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).map_err(|_| bad("Invalid image MIME"))?,
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok((h, bytes).into_response())
}
async fn asset(auth: Auth, State(s): State<AppState>, Path(id): Path<Uuid>) -> AppResult<Response> {
    image_response(sqlx::query_as("SELECT content_type,bytes FROM transfer_blobs WHERE owner_id=$1 AND id=$2 AND trashed_at IS NULL").bind(auth.user_id).bind(id).fetch_optional(&s.db).await?)
}
async fn media(
    auth: Auth,
    State(s): State<AppState>,
    Path((project, path)): Path<(Uuid, String)>,
) -> AppResult<Response> {
    if !archive::valid_path(&path) {
        return Err(AppError::NotFound);
    }
    image_response(sqlx::query_as("SELECT content_type,bytes FROM transfer_blobs WHERE owner_id=$1 AND project_id=$2 AND path=$3 AND trashed_at IS NULL").bind(auth.user_id).bind(project).bind(path).fetch_optional(&s.db).await?)
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    #[tokio::test]
    async fn a_slow_transfer_leaves_capacity_for_another_owner() {
        let first = Uuid::new_v4();
        let active = reserve(first).await.unwrap();
        assert!(
            timeout(Duration::from_millis(10), reserve(first))
                .await
                .is_err()
        );
        let other = reserve(Uuid::new_v4()).await.unwrap();
        drop((active, other));
        assert!(reserve(first).await.is_ok());
    }
}
