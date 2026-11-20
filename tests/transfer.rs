//! Exercise the public import and export routes against independently migrated databases.
use std::{
    collections::BTreeMap,
    io::{Cursor, Read, Write},
};

use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use ratio_server::{AppState, app};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

// The application has two process-wide transfer slots. Reserve this test binary's
// slots per scenario so the retry race exercises the owner lock deterministically.
static TRANSFER_SCENARIO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Tenant {
    id: Uuid,
    project: Uuid,
    cookie: String,
    csrf: String,
}

impl Tenant {
    async fn new(db: &PgPool) -> Result<Self> {
        let id = Uuid::new_v4();
        let address = format!("{id}@example.invalid");
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(id)
            .bind(address)
            .execute(db)
            .await?;
        let project = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,key,name) VALUES($1,'DOC','Documents') RETURNING id",
        )
        .bind(id)
        .fetch_one(db)
        .await?;
        let raw = hex::encode([*Uuid::new_v4().as_bytes(), *Uuid::new_v4().as_bytes()].concat());
        sqlx::query("INSERT INTO user_sessions(user_id,token_hash) VALUES($1,$2)")
            .bind(id)
            .bind(Sha256::digest(raw.as_bytes()).to_vec())
            .execute(db)
            .await?;
        let mut mac = Hmac::<Sha256>::new_from_slice(raw.as_bytes())?;
        mac.update(b"workspace/form-csrf/v1");
        Ok(Self {
            id,
            project,
            cookie: format!("workspace_session={raw}"),
            csrf: hex::encode(mac.finalize().into_bytes()),
        })
    }

    async fn send(
        &self,
        app: &Router,
        method: Method,
        path: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Response> {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, &self.cookie);
        if let Some(kind) = content_type {
            req = req.header(header::CONTENT_TYPE, kind);
        }
        Ok(app.clone().oneshot(req.body(Body::from(body))?).await?)
    }

    async fn get(&self, app: &Router, path: &str) -> Result<Response> {
        self.send(app, Method::GET, path, None, Vec::new()).await
    }

    async fn upload(
        &self,
        app: &Router,
        project: Uuid,
        format: &str,
        folder: &str,
        file: &[u8],
    ) -> Result<Response> {
        let boundary = format!("test-{}", Uuid::new_v4());
        let filename = if format == "linear" {
            "issues.csv"
        } else {
            "vault.zip"
        };
        let mut body = Vec::new();
        for (key, value) in [
            ("csrf", self.csrf.as_str()),
            ("project", &project.to_string()),
            ("format", format),
            ("folder", folder),
        ] {
            write!(
                body,
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n"
            )?;
        }
        write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )?;
        body.extend_from_slice(file);
        write!(body, "\r\n--{boundary}--\r\n")?;
        self.send(
            app,
            Method::POST,
            "/transfer/preview",
            Some(&format!("multipart/form-data; boundary={boundary}")),
            body,
        )
        .await
    }

    async fn preview(&self, app: &Router, format: &str, folder: &str, file: &[u8]) -> Result<Uuid> {
        let response = self.upload(app, self.project, format, folder, file).await?;
        let url = redirect(response).await?;
        Ok(url
            .strip_prefix("/transfer/review/")
            .context("preview URL")?
            .parse()?)
    }

    async fn commit(&self, app: &Router, preview: Uuid, choice: &str) -> Result<Response> {
        self.send(
            app,
            Method::POST,
            &format!("/transfer/commit/{preview}"),
            Some("application/x-www-form-urlencoded"),
            format!("csrf={}&decision={choice}", self.csrf).into_bytes(),
        )
        .await
    }

    async fn import(&self, app: &Router, format: &str, folder: &str, file: &[u8]) -> Result<()> {
        let id = self.preview(app, format, folder, file).await?;
        assert_eq!(
            redirect(self.commit(app, id, "local").await?).await?,
            format!("/transfer/review/{id}")
        );
        Ok(())
    }

    async fn delete_asset(&self, app: &Router, id: Uuid) -> Result<Response> {
        self.send(
            app,
            Method::POST,
            &format!("/transfer/assets/{id}/delete"),
            Some("application/x-www-form-urlencoded"),
            format!("csrf={}&confirm=delete", self.csrf).into_bytes(),
        )
        .await
    }

    async fn export(&self, app: &Router) -> Result<Vec<u8>> {
        let response = self
            .get(app, &format!("/transfer/export/{}", self.project))
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/zip");
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION].to_str()?,
            format!(
                "attachment; filename=\"ratio-project-{}.zip\"",
                self.project
            )
        );
        Ok(to_bytes(response.into_body(), 5_000_000).await?.to_vec())
    }
}

async fn redirect(response: Response) -> Result<String> {
    let status = response.status();
    if status != StatusCode::SEE_OTHER {
        let body = to_bytes(response.into_body(), 100_000).await?;
        anyhow::bail!(
            "expected redirect, received {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    Ok(response
        .headers()
        .get(header::LOCATION)
        .context("Location header")?
        .to_str()?
        .to_owned())
}

fn zip_files(files: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for &(name, bytes) in files {
        writer.start_file(name, SimpleFileOptions::default())?;
        writer.write_all(bytes)?;
    }
    Ok(writer.finish()?.into_inner())
}

fn unpack(bytes: Vec<u8>) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut files = BTreeMap::new();
    for idx in 0..archive.len() {
        let mut file = archive.by_index(idx)?;
        if !file.is_dir() {
            let mut content = Vec::new();
            file.read_to_end(&mut content)?;
            files.insert(file.name().to_string(), content);
        }
    }
    Ok(files)
}

async fn counts(db: &PgPool, project: Uuid) -> Result<(i64, i64, i64)> {
    Ok(sqlx::query_as(
        "SELECT (SELECT count(*) FROM notes WHERE project_id=$1), \
                (SELECT count(*) FROM transfer_blobs WHERE project_id=$1), \
                (SELECT count(*) FROM issues WHERE project_id=$1)",
    )
    .bind(project)
    .fetch_one(db)
    .await?)
}

#[sqlx::test]
async fn vault_roundtrip_restores_sources_images_links_and_scopes_the_preview(
    db: PgPool,
) -> Result<()> {
    let _scenario = TRANSFER_SCENARIO.lock().await;
    let app = app(AppState::for_test(db.clone()));
    let author = Tenant::new(&db).await?;
    let reader = Tenant::new(&db).await?;
    let document = b"\xef\xbb\xbf---\r\ncustom_field: 11 # keep this\r\n---\r\n# Brief\r\n\r\nSee [[Index]] and ![sample](../media/figure.png).\r\n";
    let other = "# Index\n\n# Décision\n".as_bytes();
    let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGOIT0gEAAJCASHMB3bBAAAAAElFTkSuQmCC")?;
    let archive = zip_files(&[
        ("my-vault/仕様/Brief.md", document),
        ("my-vault/仕様/Index.md", other),
        ("my-vault/media/figure.png", &png),
        ("my-vault/.obsidian/private.md", b"settings"),
        ("unselected.md", b"outside the selected vault"),
    ])?;
    let review = author.preview(&app, "vault", "my-vault", &archive).await?;
    assert_eq!(
        counts(&db, author.project).await?,
        (0, 0, 0),
        "preview must roll back trial writes"
    );
    for path in [
        format!("/transfer/review/{review}"),
        format!("/transfer/export/{}", author.project),
    ] {
        assert_eq!(
            reader.get(&app, &path).await?.status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        reader.commit(&app, review, "incoming").await?.status(),
        StatusCode::NOT_FOUND
    );
    let (first, retry) = tokio::try_join!(
        author.commit(&app, review, "local"),
        author.commit(&app, review, "local"),
    )?;
    assert_eq!(redirect(first).await?, redirect(retry).await?);
    assert_eq!(counts(&db, author.project).await?, (2, 1, 0));
    author.import(&app, "vault", "my-vault", &archive).await?;
    assert_eq!(counts(&db, author.project).await?, (2, 1, 0));

    let blob_id: Uuid = sqlx::query_scalar("SELECT id FROM transfer_blobs WHERE owner_id=$1")
        .bind(author.id)
        .fetch_one(&db)
        .await?;
    let asset = format!("/transfer/assets/{blob_id}");
    let own_response = author.get(&app, &asset).await?;
    assert_eq!(own_response.status(), StatusCode::OK);
    assert_eq!(own_response.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(
        to_bytes(own_response.into_body(), 10_000).await?.as_ref(),
        png
    );
    assert_eq!(
        reader.get(&app, &asset).await?.status(),
        StatusCode::NOT_FOUND
    );

    // Archived projects remain portable after a downgrade.
    sqlx::query("UPDATE projects SET archived=true WHERE id=$1")
        .bind(author.project)
        .execute(&db)
        .await?;
    assert_eq!(
        reader.delete_asset(&app, blob_id).await?.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        author.delete_asset(&app, blob_id).await?.status(),
        StatusCode::CONFLICT
    );
    let exported = author.export(&app).await?;
    let original = unpack(exported.clone())?;
    assert_eq!(original["Notes/仕様/Brief.md"], document);
    assert_eq!(original["Notes/仕様/Index.md"], other);
    assert_eq!(original["Notes/media/figure.png"], png);
    assert!(original.contains_key("history.jsonl"));
    let manifest: serde_json::Value = serde_json::from_slice(&original["manifest.json"])?;
    assert_eq!(manifest["version"], 1);
    let files = manifest["files"].as_array().context("manifest files")?;
    assert_eq!(files.len(), 3);
    for file in files {
        let path = file["path"].as_str().context("manifest file path")?;
        assert_eq!(file["sha256"], hex::encode(Sha256::digest(&original[path])));
    }

    for _ in 0..2 {
        reader.import(&app, "vault", "", &exported).await?;
    }
    let restored = unpack(reader.export(&app).await?)?;
    for path in [
        "Notes/仕様/Brief.md",
        "Notes/仕様/Index.md",
        "Notes/media/figure.png",
    ] {
        assert_eq!(restored[path], original[path], "source at {path}");
    }
    assert_eq!(counts(&db, reader.project).await?, (2, 1, 0));
    let linked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM note_links l JOIN notes target ON target.id=l.dest_note
         WHERE l.project_id=$1 AND target.project_id=$1 AND target.path=$2",
    )
    .bind(reader.project)
    .bind("仕様/Index.md")
    .fetch_one(&db)
    .await?;
    assert_eq!(
        linked, 1,
        "wiki link must resolve into the destination project"
    );

    // A vault editor renames a file without changing its bytes or rewriting the
    // export manifest. The stable source identity must survive that move.
    let lookup = "SELECT id FROM notes WHERE project_id=$1 AND path=$2";
    let before_id: Uuid = sqlx::query_scalar(lookup)
        .bind(reader.project)
        .bind("仕様/Brief.md")
        .fetch_one(&db)
        .await?;
    let renamed_files: Vec<(&str, &[u8])> = original
        .iter()
        .map(|(path, content)| {
            let name = if path == "Notes/仕様/Brief.md" {
                "Notes/仕様/Renamed.md"
            } else {
                path.as_str()
            };
            (name, content.as_slice())
        })
        .collect();
    reader
        .import(&app, "vault", "", &zip_files(&renamed_files)?)
        .await?;
    let after_id: Uuid = sqlx::query_scalar(lookup)
        .bind(reader.project)
        .bind("仕様/Renamed.md")
        .fetch_one(&db)
        .await?;
    assert_eq!(before_id, after_id);
    assert_eq!(counts(&db, reader.project).await?, (2, 1, 0));
    let restored_blob: Uuid =
        sqlx::query_scalar("SELECT id FROM transfer_blobs WHERE project_id=$1")
            .bind(reader.project)
            .fetch_one(&db)
            .await?;
    let before_delete = ratio_server::store::storage_used(&db, reader.id).await?;
    redirect(reader.delete_asset(&app, restored_blob).await?).await?;
    let after_delete = ratio_server::store::storage_used(&db, reader.id).await?;
    assert_eq!(before_delete - after_delete, png.len() as i64);
    assert_eq!(counts(&db, reader.project).await?, (2, 0, 0));
    Ok(())
}

#[sqlx::test]
async fn linear_reimports_resolve_parents_and_a_stale_preview_preserves_local_edits(
    db: PgPool,
) -> Result<()> {
    let _scenario = TRANSFER_SCENARIO.lock().await;
    let app = app(AppState::for_test(db.clone()));
    let owner = Tenant::new(&db).await?;
    let csv = "ID,Title,Description,Status,Priority,Project,Parent issue,Created\r\n\
               CORE-2,Child,Next step,In Progress,High,Planning,CORE-1,2026-01-02T12:00:00Z\r\n\
               CORE-1,Parent,Initial description,Todo,Low,Planning,,2026-01-01T12:00:00Z\r\n";
    for _ in 0..2 {
        owner.import(&app, "linear", "", csv.as_bytes()).await?;
    }
    assert_eq!(counts(&db, owner.project).await?, (0, 0, 2));
    let (status, priority, created, parent): (
        String,
        String,
        chrono::DateTime<chrono::Utc>,
        String,
    ) = sqlx::query_as(
        "SELECT i.status,i.priority,i.created_at,parent.title FROM transfer_linear l
         JOIN issues i ON i.id=l.issue_id JOIN issues parent ON parent.id=l.parent_id
         WHERE l.project_id=$1 AND l.source_id='CORE-2'",
    )
    .bind(owner.project)
    .fetch_one(&db)
    .await?;
    assert_eq!(
        (status.as_str(), priority.as_str(), parent.as_str()),
        ("in_progress", "high", "Parent")
    );
    assert_eq!(created.to_rfc3339(), "2026-01-02T12:00:00+00:00");

    let changed = csv.replace("Next step", "Changed in source");
    let preview = owner
        .preview(&app, "linear", "", changed.as_bytes())
        .await?;
    sqlx::query("UPDATE issues SET body='Saved in app',revision=revision+1,updated_at=now() WHERE project_id=$1 AND title='Child'")
        .bind(owner.project).execute(&db).await?;
    assert_eq!(
        owner.commit(&app, preview, "incoming").await?.status(),
        StatusCode::CONFLICT
    );
    let read_body = || {
        sqlx::query_scalar::<_, String>(
            "SELECT body FROM issues WHERE project_id=$1 AND title='Child'",
        )
        .bind(owner.project)
        .fetch_one(&db)
    };
    assert_eq!(read_body().await?, "Saved in app");
    let next = owner
        .preview(&app, "linear", "", changed.as_bytes())
        .await?;
    redirect(owner.commit(&app, next, "incoming").await?).await?;
    assert_eq!(read_body().await?, "Changed in source");
    Ok(())
}

#[sqlx::test]
async fn rejected_archive_has_no_partial_notes(db: PgPool) -> Result<()> {
    let _scenario = TRANSFER_SCENARIO.lock().await;
    let app = app(AppState::for_test(db.clone()));
    let owner = Tenant::new(&db).await?;
    let zip = zip_files(&[
        ("First.md", b"valid source"),
        ("Second.md", b"invalid\0source"),
    ])?;
    let response = owner.upload(&app, owner.project, "vault", "", &zip).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(counts(&db, owner.project).await?, (0, 0, 0));
    Ok(())
}

#[sqlx::test]
async fn native_restore_remaps_and_preserves_published_history_on_reimport(
    db: PgPool,
) -> Result<()> {
    let _scenario = TRANSFER_SCENARIO.lock().await;
    let app = app(AppState::for_test(db.clone()));
    let original = Tenant::new(&db).await?;
    let restored = Tenant::new(&db).await?;
    let issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues(owner_id,project_id,number,title,status,started_at,completed_at,created_at,updated_at,checkpoint_at,next_action)
         VALUES($1,$2,17,'Deploy', 'done','2025-02-01T09:00Z','2025-02-04T11:30Z','2025-02-01T07:00Z','2025-02-04T11:30Z',
                '2025-02-02T08:15Z','Review the deployment log') RETURNING id",
    ).bind(original.id).bind(original.project).fetch_one(&db).await?;
    sqlx::query(
        "INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,occurred_at)
         VALUES($1,$2,$3,'in_progress','done','2025-02-04T11:30Z')",
    )
    .bind(original.id)
    .bind(original.project)
    .bind(issue)
    .execute(&db)
    .await?;
    let release: Uuid = sqlx::query_scalar(
        "INSERT INTO releases(owner_id,project_id,name,version,created_at) VALUES($1,$2,'First shipment','v1','2025-02-01T08:00Z') RETURNING id",
    ).bind(original.id).bind(original.project).fetch_one(&db).await?;
    sqlx::query(
        "INSERT INTO release_entries(owner_id,project_id,release_id,issue_id,number,title,issue_revision)
         VALUES($1,$2,$3,$4,17,'Deploy',1)",
    ).bind(original.id).bind(original.project).bind(release).bind(issue).execute(&db).await?;
    sqlx::query("UPDATE releases SET published_on='2025-02-05',published_at='2025-02-05T14:00Z',publish_command=gen_random_uuid() WHERE id=$1")
        .bind(release).execute(&db).await?;
    sqlx::query(
        "INSERT INTO release_amendments(owner_id,project_id,release_id,issue_id,actor_id,reason,replacement,created_at)
         VALUES($1,$2,$3,$4,$1,'Spelling correction','Deploy beta','2025-02-06T12:00Z')",
    ).bind(original.id).bind(original.project).bind(release).bind(issue).execute(&db).await?;
    let export = original.export(&app).await?;
    let files = unpack(export.clone())?;
    let history = std::str::from_utf8(files.get("history.jsonl").context("history file")?)?;
    assert!(history.contains("Spelling correction"));
    let checkpoint = std::str::from_utf8(
        files
            .get("Checkpoints/17.md")
            .context("readable checkpoint")?,
    )?;
    for expected in [
        "DOC-17",
        "2025-02-02T08:15:00Z",
        "Review the deployment log",
        "../Issues/17.md",
    ] {
        assert!(
            checkpoint.contains(expected),
            "checkpoint is missing {expected}"
        );
    }
    assert!(
        files
            .keys()
            .any(|p| p.starts_with("Issues/") && p.ends_with(".md"))
    );
    assert!(
        files
            .keys()
            .any(|p| p.starts_with("Releases/") && p.ends_with(".md"))
    );
    for _ in 0..2 {
        restored.import(&app, "vault", "", &export).await?;
    }

    assert_eq!(
        counts(&db, restored.project).await?,
        (0, 0, 1),
        "checkpoint reading copies must not create notes"
    );
    let snapshot: Vec<(Uuid, Uuid, String, String, String)> = sqlx::query_as(
        "SELECT i.id,r.id,r.name,a.replacement,e.title FROM releases r
         JOIN release_entries e ON e.release_id=r.id JOIN issues i ON i.id=e.issue_id
         JOIN release_amendments a ON a.release_id=e.release_id AND a.issue_id=e.issue_id
         WHERE r.project_id=$1 AND r.owner_id=$2 AND a.actor_id=$2 AND a.project_id=$1
           AND r.published_on='2025-02-05' AND a.created_at='2025-02-06T12:00Z'",
    )
    .bind(restored.project)
    .bind(restored.id)
    .fetch_all(&db)
    .await?;
    assert_eq!(snapshot.len(), 1);
    let (local_issue, local_release, name, replacement, immutable_entry) = &snapshot[0];
    assert_ne!(local_issue, &issue);
    assert_ne!(local_release, &release);
    assert_eq!(
        (
            name.as_str(),
            replacement.as_str(),
            immutable_entry.as_str()
        ),
        ("First shipment", "Deploy beta", "Deploy")
    );
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_events WHERE project_id=$1 AND issue_id=$2 AND occurred_at='2025-02-04T11:30Z' AND old_status='in_progress' AND new_status='done'")
        .bind(restored.project).bind(local_issue).fetch_one(&db).await?;
    assert_eq!(
        events, 1,
        "preserve older history on every tier and never duplicate it"
    );
    Ok(())
}

#[sqlx::test]
async fn workspace_quota_uses_uncompressed_bytes_and_preview_rolls_back(db: PgPool) -> Result<()> {
    let _scenario = TRANSFER_SCENARIO.lock().await;
    let app = app(AppState::for_test(db.clone()));
    let owner = Tenant::new(&db).await?;
    // PostgreSQL compresses this existing content on disk. The owner's storage
    // meter must still account for each of the 20 logical 5 MB image blobs.
    let fixture = vec![b'A'; 5_000_000];
    let digest = hex::encode(Sha256::digest(&fixture));
    sqlx::query(
        "INSERT INTO transfer_blobs(owner_id,project_id,path,content_type,bytes,sha256)
         SELECT $1,$2,concat('stored-',n,'.png'),'image/png',$3,$4 FROM generate_series(1,20) AS n",
    )
    .bind(owner.id)
    .bind(owner.project)
    .bind(&fixture)
    .bind(digest)
    .execute(&db)
    .await?;
    let archive = zip_files(&[("Imported.md", b"Cannot fit")])?;
    let response = owner
        .upload(&app, owner.project, "vault", "", &archive)
        .await?;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(counts(&db, owner.project).await?, (0, 20, 0));
    let previews: i64 =
        sqlx::query_scalar("SELECT count(*) FROM transfer_previews WHERE owner_id=$1")
            .bind(owner.id)
            .fetch_one(&db)
            .await?;
    assert_eq!(previews, 0);
    Ok(())
}

#[sqlx::test]
async fn note_links_survive_project_key_and_target_path_changes(db: PgPool) -> Result<()> {
    let _scenario = TRANSFER_SCENARIO.lock().await;
    let app = app(AppState::for_test(db.clone()));
    let author = Tenant::new(&db).await?;
    let reader = Tenant::new(&db).await?;
    let mut tx = db.begin().await?;
    for (user, key) in [(&author, "ALPHA"), (&reader, "BETA")] {
        sqlx::query("UPDATE projects SET key=$2 WHERE id=$1")
            .bind(user.project)
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    let issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues(owner_id,project_id,number,title) VALUES($1,$2,17,'Portable links') RETURNING id",
    ).bind(author.id).bind(author.project).fetch_one(&mut *tx).await?;
    let original_body =
        format!("[[ALPHA-17]] and [Open issue](/issues/{issue}) and [[Architecture]].\n");
    sqlx::query("INSERT INTO notes(owner_id,project_id,path,title,body) VALUES($1,$2,'Decisions.md','Decisions',$3),($1,$2,'Architecture.md','Architecture','Snapshot')")
        .bind(author.id).bind(author.project).bind(&original_body).execute(&mut *tx).await?;
    ratio_server::notes::reindex(&mut tx, author.id, author.project).await?;
    sqlx::query("UPDATE notes SET path='Record.md',title='Record',revision=revision+1 WHERE project_id=$1 AND path='Architecture.md'")
        .bind(author.project).execute(&mut *tx).await?;
    ratio_server::notes::reindex(&mut tx, author.id, author.project).await?;
    let source_count: i64 = sqlx::query_scalar("SELECT count(*) FROM note_links WHERE project_id=$1 AND (dest_note IS NOT NULL OR dest_issue IS NOT NULL)")
        .bind(author.project).fetch_one(&mut *tx).await?;
    assert_eq!(
        source_count, 3,
        "source editor preserved the three resolved references"
    );
    tx.commit().await?;

    let archive = author.export(&app).await?;
    for _ in 0..2 {
        reader.import(&app, "vault", "", &archive).await?;
        let mut tx = db.begin().await?;
        ratio_server::notes::reindex(&mut tx, reader.id, reader.project).await?;
        tx.commit().await?;
        let (body, decision): (String, Uuid) =
            sqlx::query_as("SELECT body,id FROM notes WHERE project_id=$1 AND path='Decisions.md'")
                .bind(reader.project)
                .fetch_one(&db)
                .await?;
        assert_eq!(body, original_body);
        let local_issue: Uuid = sqlx::query_scalar(
            "SELECT id FROM issues WHERE project_id=$1 AND title='Portable links'",
        )
        .bind(reader.project)
        .fetch_one(&db)
        .await?;
        assert_ne!(issue, local_issue);
        let local_note: Uuid =
            sqlx::query_scalar("SELECT id FROM notes WHERE project_id=$1 AND path='Record.md'")
                .bind(reader.project)
                .fetch_one(&db)
                .await?;
        let bindings: Vec<(Option<Uuid>,Option<Uuid>)> = sqlx::query_as(
            "SELECT dest_issue,dest_note FROM note_links WHERE owner_id=$1 AND project_id=$2 AND source_id=$3 ORDER BY ordinal",
        ).bind(reader.id).bind(reader.project).bind(decision).fetch_all(&db).await?;
        assert_eq!(
            bindings,
            [
                (Some(local_issue), None),
                (Some(local_issue), None),
                (None, Some(local_note))
            ]
        );
    }
    Ok(())
}
