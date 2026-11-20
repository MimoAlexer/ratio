use super::archive::{Entry, ManifestFile};
use super::{Action, Mode, bad, hash};
use crate::{AppError, AppResult};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres, Transaction};
use uuid::Uuid;

type Tx<'a> = Transaction<'a, Postgres>;
use futures_util::TryStreamExt;

#[derive(Serialize, Deserialize, FromRow)]
struct Note {
    id: Uuid,
    path: String,
    title: String,
    body: String,
    revision: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct NoteMeta {
    title: String,
    revision: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    base_state: String,
}

fn state(path: &str, title: &str, digest: &str) -> String {
    hash(
        serde_json::json!([path, title, digest])
            .to_string()
            .as_bytes(),
    )
}

fn source_key(kind: &str, path: &str) -> Uuid {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("vault/{kind}/{path}").as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

pub(super) async fn stream(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    out: &super::export::Sink,
) -> AppResult<()> {
    let mut notes=sqlx::query_as::<_,Note>("SELECT id,path,title,body,revision,created_at,updated_at FROM notes WHERE owner_id=$1 AND project_id=$2 ORDER BY path").bind(owner).bind(project).fetch(&mut **tx);
    while let Some(n) = notes.try_next().await? {
        let digest = hash(n.body.as_bytes());
        let data = NoteMeta {
            base_state: state(&n.path, &n.title, &digest),
            title: n.title,
            created_at: n.created_at,
            updated_at: n.updated_at,
            revision: n.revision,
        };
        out.file(
            ManifestFile {
                id: n.id,
                path: format!("Notes/{}", n.path),
                kind: "note".into(),
                sha256: digest,
                data: serde_json::to_value(data).map_err(|_| bad("Invalid note metadata"))?,
            },
            n.body.into_bytes(),
        )
        .await?;
    }
    drop(notes);
    let mut images=sqlx::query_as::<_,(Uuid,String,String,String,Vec<u8>)>("SELECT id,path,content_type,sha256,bytes FROM transfer_blobs WHERE owner_id=$1 AND project_id=$2 AND trashed_at IS NULL ORDER BY path").bind(owner).bind(project).fetch(&mut **tx);
    while let Some((id, path, mime, sha256, bytes)) = images.try_next().await? {
        let data = serde_json::json!({"content_type":mime,"base_state":state(&path,"",&sha256)});
        out.file(
            ManifestFile {
                id,
                path: format!("Notes/{path}"),
                kind: "blob".into(),
                sha256,
                data,
            },
            bytes,
        )
        .await?;
    }
    Ok(())
}

#[derive(FromRow)]
struct Existing {
    id: Uuid,
    path: String,
    title: String,
    digest: String,
}

async fn find_existing(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    kind: &str,
    id: Uuid,
    path: &str,
) -> AppResult<Option<Existing>> {
    // pgcrypto is optional; hash note text locally. Images are already digested on upload.
    let existing = if kind == "note" {
        sqlx::query_as::<_,(Uuid,String,String,String)>("SELECT id,path,title,body FROM notes WHERE owner_id=$1 AND project_id=$2 AND (id=$3 OR path=$4) ORDER BY (id=$3) DESC LIMIT 1").bind(owner).bind(project).bind(id).bind(path).fetch_optional(&mut **tx).await?.map(|(id,path,title,body)|Existing{id,path,title,digest:hash(body.as_bytes())})
    } else {
        let reserved: Option<String> = sqlx::query_scalar("SELECT path FROM transfer_blobs WHERE owner_id=$1 AND project_id=$2 AND trashed_at IS NOT NULL AND (id=$3 OR path=$4) LIMIT 1")
            .bind(owner).bind(project).bind(id).bind(path).fetch_optional(&mut **tx).await?;
        if let Some(reserved) = reserved {
            return Err(AppError::Conflict(format!(
                "The image {reserved} is in this project’s trash. Restore or permanently delete it in Image trash, then preview the import again."
            )));
        }
        sqlx::query_as::<_,Existing>("SELECT id,path,''::text AS title,sha256 AS digest FROM transfer_blobs WHERE owner_id=$1 AND project_id=$2 AND trashed_at IS NULL AND (id=$3 OR path=$4) ORDER BY (id=$3) DESC LIMIT 1").bind(owner).bind(project).bind(id).bind(path).fetch_optional(&mut **tx).await?
    };
    Ok(existing)
}

fn decide(current: Option<&str>, base: Option<&str>, incoming: &str) -> Action {
    match current {
        None => Action::Create,
        Some(c) if c == incoming || base == Some(incoming) => Action::Unchanged,
        Some(c) if Some(c) == base => Action::Update,
        Some(_) => Action::Conflict,
    }
}

pub(super) async fn apply(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    entry: &Entry,
    native: bool,
    meta: Option<&ManifestFile>,
    mode: Mode,
) -> AppResult<(Action, Uuid)> {
    let kind = entry.kind.as_str();
    let path = if native {
        entry.path.strip_prefix("Notes/").unwrap_or(&entry.path)
    } else {
        &entry.path
    };
    if kind == "note" && crate::notes::validate_path(path).as_deref() != Ok(path) {
        return Err(bad(format!("Invalid note path: {path}")));
    }
    if !super::archive::valid_path(path) {
        return Err(bad(format!("Invalid file path: {path}")));
    }
    let note_meta = if kind == "note" {
        meta.map(|f| {
            serde_json::from_value::<NoteMeta>(f.data.clone())
                .map_err(|_| bad(format!("Invalid note metadata for {path}")))
        })
        .transpose()?
    } else {
        None
    };
    let title = match (kind, &note_meta) {
        ("note", Some(m)) => m.title.as_str(),
        ("note", None) => std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(""),
        ("blob", _) => "",
        _ => return Err(bad("Unknown import file type")),
    };
    if kind == "note"
        && (title.trim().is_empty() || title.chars().count() > 200 || title.contains('\0'))
    {
        return Err(bad(format!("Note title must be 1–200 characters: {path}")));
    }
    if note_meta
        .as_ref()
        .is_some_and(|m| m.revision < 1 || m.revision == i64::MAX || m.created_at > m.updated_at)
    {
        return Err(bad(format!("Invalid note dates or revision: {path}")));
    }
    let source = meta.map(|m| m.id).unwrap_or_else(|| source_key(kind, path));
    let tracked=sqlx::query_as::<_,(Uuid,String)>("SELECT local_id,base_hash FROM transfer_records WHERE owner_id=$1 AND project_id=$2 AND kind=$3 AND source_id=$4").bind(owner).bind(project).bind(kind).bind(source).fetch_optional(&mut **tx).await?;
    let expected = tracked.as_ref().map(|r| r.0).unwrap_or(source);
    let existing = find_existing(tx, owner, project, kind, expected, path).await?;
    let current = existing
        .as_ref()
        .map(|e| state(&e.path, &e.title, &e.digest));
    let incoming = state(path, title, &hash(&entry.bytes));
    let archive_base = meta.and_then(|f| f.data.get("base_state")?.as_str());
    let base = archive_base
        .filter(|b| {
            existing.as_ref().is_some_and(|e| e.id == expected)
                && (Some(*b) == current.as_deref() || *b == incoming)
        })
        .or_else(|| tracked.as_ref().map(|r| r.1.as_str()))
        .or_else(|| archive_base.filter(|_| existing.as_ref().is_some_and(|e| e.id == expected)));
    let action = decide(current.as_deref(), base, &incoming);
    let local_id = existing.as_ref().map(|e| e.id).unwrap_or_else(Uuid::new_v4);
    if mode == Mode::Preview {
        return Ok((action, local_id));
    }
    if action == Action::Create
        || action == Action::Update
        || (action == Action::Conflict && mode == Mode::UseIncoming)
    {
        if existing.as_ref().is_some_and(|e| e.path != path)
            && find_existing(tx, owner, project, kind, Uuid::nil(), path)
                .await?
                .is_some_and(|e| e.id != local_id)
        {
            return Err(AppError::Conflict(format!(
                "The destination already contains {path}. Rename it and preview the import again."
            )));
        }
        if kind == "note" {
            let body = std::str::from_utf8(&entry.bytes).map_err(|_| bad("Note is not UTF-8"))?;
            if body.contains('\0') {
                return Err(bad("A note contains a NUL character"));
            }
            let dates = note_meta.as_ref();
            sqlx::query("INSERT INTO notes(id,owner_id,project_id,path,title,body,created_at,updated_at,revision) VALUES($1,$2,$3,$4,$5,$6,COALESCE($7,now()),COALESCE($8,now()),$9) ON CONFLICT(id) DO UPDATE SET path=EXCLUDED.path,title=EXCLUDED.title,body=EXCLUDED.body,updated_at=now(),revision=notes.revision+1 WHERE notes.owner_id=EXCLUDED.owner_id AND notes.project_id=EXCLUDED.project_id")
                .bind(local_id).bind(owner).bind(project).bind(path).bind(title).bind(body).bind(dates.map(|m|m.created_at)).bind(dates.map(|m|m.updated_at)).bind(dates.map(|m|m.revision).unwrap_or(1)).execute(&mut **tx).await?;
        } else {
            let mime = super::archive::image_type(path, &entry.bytes)
                .ok_or_else(|| bad(format!("Unsupported image: {path}")))?;
            sqlx::query("INSERT INTO transfer_blobs(id,owner_id,project_id,path,content_type,bytes,sha256) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(id) DO UPDATE SET path=EXCLUDED.path,content_type=EXCLUDED.content_type,bytes=EXCLUDED.bytes,sha256=EXCLUDED.sha256 WHERE transfer_blobs.owner_id=EXCLUDED.owner_id AND transfer_blobs.project_id=EXCLUDED.project_id AND transfer_blobs.trashed_at IS NULL")
                .bind(local_id).bind(owner).bind(project).bind(path).bind(mime).bind(&entry.bytes).bind(hash(&entry.bytes)).execute(&mut **tx).await?;
        }
    }
    sqlx::query("INSERT INTO transfer_records(owner_id,project_id,kind,source_id,local_id,base_hash,path) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(project_id,kind,source_id) DO UPDATE SET local_id=EXCLUDED.local_id,base_hash=EXCLUDED.base_hash,path=EXCLUDED.path WHERE (transfer_records.local_id,transfer_records.base_hash,transfer_records.path) IS DISTINCT FROM (EXCLUDED.local_id,EXCLUDED.base_hash,EXCLUDED.path)").bind(owner).bind(project).bind(kind).bind(source).bind(local_id).bind(incoming).bind(path).execute(&mut **tx).await?;
    Ok((action, local_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn three_way_merge_respects_untouched_inputs_and_local_changes() {
        assert_eq!(decide(Some("local"), Some("old"), "old"), Action::Unchanged);
        assert_eq!(decide(Some("old"), Some("old"), "new"), Action::Update);
        assert_eq!(decide(Some("local"), Some("old"), "new"), Action::Conflict);
        assert_eq!(decide(Some("same"), None, "same"), Action::Unchanged);
        assert_eq!(decide(None, None, "new"), Action::Create);
    }
}
