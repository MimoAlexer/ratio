use std::io::Cursor;

use anyhow::Result;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::{AppError, AppState, store, transfer};

struct Owner {
    user: Uuid,
    project: Uuid,
    cookie: String,
    csrf: String,
}

impl Owner {
    async fn create(db: &PgPool) -> Result<Self> {
        let user = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email) VALUES ($1,$2)")
            .bind(user)
            .bind(format!("{user}@example.invalid"))
            .execute(db)
            .await?;
        let project = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES($1,'Files','DOC') RETURNING id",
        )
        .bind(user)
        .fetch_one(db)
        .await?;
        let raw = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        sqlx::query("INSERT INTO user_sessions(user_id,token_hash) VALUES ($1,$2)")
            .bind(user)
            .bind(Sha256::digest(raw.as_bytes()).to_vec())
            .execute(db)
            .await?;
        let mut mac = Hmac::<Sha256>::new_from_slice(raw.as_bytes())?;
        mac.update(b"workspace/form-csrf/v1");
        Ok(Self {
            user,
            project,
            cookie: format!("workspace_session={raw}"),
            csrf: hex::encode(mac.finalize().into_bytes()),
        })
    }

    async fn image(&self, db: &PgPool, path: &str) -> Result<(Uuid, Vec<u8>)> {
        let bytes = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGOIT0gEAAJCASHMB3bBAAAAAElFTkSuQmCC")?;
        let id = sqlx::query_scalar(
            "INSERT INTO transfer_blobs(owner_id,project_id,path,content_type,bytes,sha256)
             VALUES($1,$2,$3,'image/png',$4,$5) RETURNING id",
        )
        .bind(self.user)
        .bind(self.project)
        .bind(path)
        .bind(&bytes)
        .bind(transfer::hash(&bytes))
        .fetch_one(db)
        .await?;
        sqlx::query(
            "INSERT INTO transfer_records(owner_id,project_id,kind,source_id,local_id,base_hash,path)
             VALUES($1,$2,'blob',$3,$3,$4,$5)",
        ).bind(self.user).bind(self.project).bind(id).bind(transfer::hash(&bytes)).bind(path).execute(db).await?;
        Ok((id, bytes))
    }

    async fn get(&self, app: &Router, path: &str) -> Result<Response> {
        Ok(app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::COOKIE, &self.cookie)
                    .body(Body::empty())?,
            )
            .await?)
    }

    async fn post(
        &self,
        app: &Router,
        id: Uuid,
        action: &str,
        csrf: &str,
        extra: &str,
    ) -> Result<StatusCode> {
        let response = app
            .clone()
            .oneshot(
                Request::post(format!("/transfer/assets/{id}/{action}"))
                    .header(header::COOKIE, &self.cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf}{extra}")))?,
            )
            .await?;
        Ok(response.status())
    }
}

async fn html(response: Response) -> Result<String> {
    assert_eq!(response.status(), StatusCode::OK);
    Ok(String::from_utf8(
        to_bytes(response.into_body(), 512_000).await?.to_vec(),
    )?)
}

#[sqlx::test]
async fn trash_restore_preserves_original_binary_quota_and_path(db: PgPool) -> Result<()> {
    let app = transfer::routes().with_state(AppState::for_test(db.clone()));
    let owner = Owner::create(&db).await?;
    let stranger = Owner::create(&db).await?;
    let path = "files/private-diagram.png";
    let (id, bytes) = owner.image(&db, path).await?;
    let asset = format!("/transfer/assets/{id}");
    let media = format!("/transfer/media/{}/{path}", owner.project);
    let trash = format!("/transfer/trash?project={}", owner.project);
    let used = store::storage_used(&db, owner.user).await?;

    assert_eq!(
        owner.post(&app, id, "trash", "incorrect", "").await?,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        stranger.post(&app, id, "trash", &stranger.csrf, "").await?,
        StatusCode::NOT_FOUND
    );
    assert_eq!(owner.get(&app, &asset).await?.status(), StatusCode::OK);
    assert_eq!(
        owner.post(&app, id, "trash", &owner.csrf, "").await?,
        StatusCode::SEE_OTHER
    );
    let first: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT trashed_at FROM transfer_blobs WHERE id=$1")
            .bind(id)
            .fetch_one(&db)
            .await?;
    assert_eq!(
        owner.post(&app, id, "trash", &owner.csrf, "").await?,
        StatusCode::SEE_OTHER
    );
    let second: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT trashed_at FROM transfer_blobs WHERE id=$1")
            .bind(id)
            .fetch_one(&db)
            .await?;
    assert_eq!(
        first, second,
        "Retrying trash must keep the original retention deadline"
    );
    for url in [&asset, &media] {
        assert_eq!(owner.get(&app, url).await?.status(), StatusCode::NOT_FOUND);
    }
    assert!(html(owner.get(&app, &trash).await?).await?.contains(path));
    assert!(
        !html(owner.get(&app, "/transfer").await?)
            .await?
            .contains(path)
    );
    assert!(
        !html(stranger.get(&app, "/transfer/trash").await?)
            .await?
            .contains(path)
    );
    assert_eq!(store::storage_used(&db, owner.user).await?, used);

    let zip = owner
        .get(&app, &format!("/transfer/export/{}", owner.project))
        .await?;
    assert_eq!(zip.status(), StatusCode::OK);
    let zip = zip::ZipArchive::new(Cursor::new(to_bytes(zip.into_body(), 512_000).await?))?;
    assert!(!zip.file_names().any(|p| p.ends_with(path)));
    let mut tx = db.begin().await?;
    store::lock_project(&mut tx, owner.user, owner.project).await?;
    let entry = transfer::archive::Entry {
        path: path.to_owned(),
        bytes: bytes.clone(),
        kind: "blob".into(),
    };
    assert!(matches!(
        transfer::vault::apply(
            &mut tx,
            owner.user,
            owner.project,
            &entry,
            false,
            None,
            transfer::Mode::UseIncoming
        )
        .await,
        Err(AppError::Conflict(_))
    ));
    tx.rollback().await?;

    assert_eq!(
        owner.post(&app, id, "restore", "incorrect", "").await?,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        stranger
            .post(&app, id, "restore", &stranger.csrf, "")
            .await?,
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE projects SET archived=true WHERE id=$1")
        .bind(owner.project)
        .execute(&db)
        .await?;
    for (action, extra) in [("restore", ""), ("delete", "&confirm=delete")] {
        assert_eq!(
            owner.post(&app, id, action, &owner.csrf, extra).await?,
            StatusCode::CONFLICT
        );
    }
    sqlx::query("UPDATE projects SET archived=false WHERE id=$1")
        .bind(owner.project)
        .execute(&db)
        .await?;
    assert_eq!(
        owner.post(&app, id, "restore", &owner.csrf, "").await?,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        owner.post(&app, id, "restore", &owner.csrf, "").await?,
        StatusCode::SEE_OTHER
    );
    assert_eq!(store::storage_used(&db, owner.user).await?, used);
    assert!(!html(owner.get(&app, &trash).await?).await?.contains(path));
    for url in [&asset, &media] {
        let response = owner.get(&app, url).await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(to_bytes(response.into_body(), 1_000).await?.as_ref(), bytes);
    }
    Ok(())
}

#[sqlx::test]
async fn confirmed_deletion_and_expiry_free_storage_and_forget_import_identity(
    db: PgPool,
) -> Result<()> {
    let app = transfer::routes().with_state(AppState::for_test(db.clone()));
    let owner = Owner::create(&db).await?;
    let initial = store::storage_used(&db, owner.user).await?;
    let (expired, bytes) = owner.image(&db, "expired.png").await?;
    let (retained, _) = owner.image(&db, "retained.png").await?;
    let (immediate, _) = owner.image(&db, "immediate.png").await?;
    for id in [expired, retained, immediate] {
        assert_eq!(
            owner.post(&app, id, "trash", &owner.csrf, "").await?,
            StatusCode::SEE_OTHER
        );
    }
    assert_eq!(
        owner
            .post(&app, immediate, "delete", "incorrect", "&confirm=delete")
            .await?,
        StatusCode::FORBIDDEN
    );
    assert!(
        owner
            .post(&app, immediate, "delete", &owner.csrf, "&confirm=no")
            .await?
            .is_client_error()
    );
    assert_eq!(
        store::storage_used(&db, owner.user).await?,
        initial + 3 * bytes.len() as i64
    );
    assert_eq!(
        owner
            .post(&app, immediate, "delete", &owner.csrf, "&confirm=delete")
            .await?,
        StatusCode::SEE_OTHER
    );
    sqlx::query("UPDATE transfer_blobs SET trashed_at=now()-interval '8 days' WHERE id=$1")
        .bind(expired)
        .execute(&db)
        .await?;
    sqlx::query("UPDATE transfer_blobs SET trashed_at=now()-interval '6 days' WHERE id=$1")
        .bind(retained)
        .execute(&db)
        .await?;
    assert!(
        owner
            .post(&app, expired, "restore", &owner.csrf, "")
            .await?
            .is_client_error()
    );
    assert_eq!(super::purge_expired(&db).await?, 1);
    assert_eq!(super::purge_expired(&db).await?, 0);
    assert_eq!(
        store::storage_used(&db, owner.user).await?,
        initial + bytes.len() as i64
    );
    let remaining: Vec<Uuid> = sqlx::query_scalar(
        "SELECT local_id FROM transfer_records WHERE owner_id=$1 AND kind='blob'",
    )
    .bind(owner.user)
    .fetch_all(&db)
    .await?;
    assert_eq!(remaining, vec![retained]);
    assert_eq!(
        owner
            .post(&app, retained, "restore", &owner.csrf, "")
            .await?,
        StatusCode::SEE_OTHER
    );
    Ok(())
}
