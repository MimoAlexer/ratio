use super::{list_checkpoints, remove_checkpoint, restore_checkpoint, save_checkpoint};
use crate::{AppError, AppState};
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

fn document(label: &str) -> Value {
    json!({
        "version": 1, "edges": [], "strokes": [],
        "nodes": [{
            "id": Uuid::new_v4(), "kind": "note", "label": label, "body": "",
            "x": 40.0, "y": 60.0, "w": 140.0, "h": 64.0
        }]
    })
}

struct Fixture {
    owner: Uuid,
    project: Uuid,
    board: Uuid,
}

impl Fixture {
    async fn new(db: &PgPool) -> Self {
        let owner: Uuid = sqlx::query_scalar("INSERT INTO users(email) VALUES ($1) RETURNING id")
            .bind(format!("{}@example.test", Uuid::new_v4()))
            .fetch_one(db)
            .await
            .unwrap();
        let project = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Maps','MAP') RETURNING id",
        )
        .bind(owner)
        .fetch_one(db)
        .await
        .unwrap();
        let board = Self::add_board(db, owner, project).await;
        Self {
            owner,
            project,
            board,
        }
    }

    async fn add_board(db: &PgPool, owner: Uuid, project: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO canvas_boards(owner_id,project_id,title,document)
             VALUES ($1,$2,'Draft',$3) RETURNING id",
        )
        .bind(owner)
        .bind(project)
        .bind(document("First"))
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn edit(&self, db: &PgPool, title: &str) -> i64 {
        sqlx::query_scalar(
            "UPDATE canvas_boards SET title=$2,document=$3,revision=revision+1
             WHERE id=$1 RETURNING revision",
        )
        .bind(self.board)
        .bind(title)
        .bind(document(title))
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn state(&self, db: &PgPool) -> (String, Value, i64) {
        sqlx::query_as("SELECT title,document,revision FROM canvas_boards WHERE id=$1")
            .bind(self.board)
            .fetch_one(db)
            .await
            .unwrap()
    }
}

#[sqlx::test]
async fn restores_are_revision_checked_and_the_displaced_drawing_is_recoverable(db: PgPool) {
    let f = Fixture::new(&db).await;
    let a = f.state(&db).await;
    let first = save_checkpoint(&db, f.owner, f.board, a.2, "First draft")
        .await
        .unwrap();
    let revision = f.edit(&db, "Review changes").await;
    let b = f.state(&db).await;
    let second = save_checkpoint(&db, f.owner, f.board, revision, "Reviewed")
        .await
        .unwrap();
    let revision = f.edit(&db, "Latest work").await;
    let latest = f.state(&db).await;

    let (one, two) = tokio::join!(
        restore_checkpoint(&db, f.owner, f.board, first, revision),
        restore_checkpoint(&db, f.owner, f.board, second, revision),
    );
    let expected = match (one, two) {
        (Ok(_), Err(AppError::Conflict(_))) => a,
        (Err(AppError::Conflict(_)), Ok(_)) => b,
        _ => panic!("exactly one restore must win the same board revision"),
    };
    assert_eq!(f.state(&db).await, (expected.0, expected.1, revision + 1));
    let rows = list_checkpoints(&db, f.owner, f.board).await.unwrap();
    assert_eq!(
        rows.len(),
        3,
        "a rejected restore must not leave a recovery copy"
    );
    let recovery = rows
        .iter()
        .find(|row| row.id != first && row.id != second)
        .unwrap();
    restore_checkpoint(&db, f.owner, f.board, recovery.id, revision + 1)
        .await
        .unwrap();
    assert_eq!(f.state(&db).await, (latest.0, latest.1, revision + 2));
}

#[sqlx::test]
async fn snapshots_stay_with_their_board_and_owner_and_archiving_freezes_history(db: PgPool) {
    let f = Fixture::new(&db).await;
    let other = Fixture::new(&db).await;
    let sibling = Fixture::add_board(&db, f.owner, f.project).await;
    let snap = save_checkpoint(&db, f.owner, f.board, 1, "Current")
        .await
        .unwrap();
    let revision = f.edit(&db, "Uncaptured").await;

    assert!(matches!(
        list_checkpoints(&db, other.owner, f.board).await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        save_checkpoint(&db, other.owner, f.board, revision, "Foreign").await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        restore_checkpoint(&db, other.owner, other.board, snap, 1).await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        restore_checkpoint(&db, f.owner, sibling, snap, 1).await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        remove_checkpoint(&db, f.owner, sibling, snap, 1).await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        save_checkpoint(&db, f.owner, f.board, 1, "Stale").await,
        Err(AppError::Conflict(_))
    ));

    let before = f.state(&db).await;
    sqlx::query("UPDATE projects SET archived=true WHERE id=$1")
        .bind(f.project)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        save_checkpoint(&db, f.owner, f.board, revision, "Archived")
            .await
            .is_err()
    );
    assert!(
        restore_checkpoint(&db, f.owner, f.board, snap, revision)
            .await
            .is_err()
    );
    assert!(
        remove_checkpoint(&db, f.owner, f.board, snap, revision)
            .await
            .is_err()
    );
    assert_eq!(
        list_checkpoints(&db, f.owner, f.board).await.unwrap().len(),
        1
    );
    assert_eq!(f.state(&db).await, before);
}

#[sqlx::test]
async fn named_limits_leave_room_for_recovery_and_only_automatic_copies_rotate(db: PgPool) {
    let f = Fixture::new(&db).await;
    let mut named = Vec::new();
    for n in 1..30 {
        named.push(
            save_checkpoint(&db, f.owner, f.board, 1, &format!("Plan {n}"))
                .await
                .unwrap(),
        );
    }
    let (last, extra) = tokio::join!(
        save_checkpoint(&db, f.owner, f.board, 1, "Last slot"),
        save_checkpoint(&db, f.owner, f.board, 1, "Concurrent slot"),
    );
    match (last, extra) {
        (Ok(id), Err(AppError::Conflict(_))) | (Err(AppError::Conflict(_)), Ok(id)) => {
            named.push(id)
        }
        _ => panic!("at most one writer can take the last named checkpoint slot"),
    }
    for n in 1..=7 {
        let revision = f.edit(&db, &format!("Latest {n}")).await;
        restore_checkpoint(&db, f.owner, f.board, named[0], revision)
            .await
            .unwrap();
    }

    let snapshots = list_checkpoints(&db, f.owner, f.board).await.unwrap();
    assert_eq!(snapshots.len(), 35);
    assert!(named.iter().all(|id| snapshots.iter().any(|s| s.id == *id)));
    let recovery_titles: Vec<String> = sqlx::query_scalar(
        "SELECT title FROM canvas_checkpoints WHERE board_id=$1 AND automatic ORDER BY title",
    )
    .bind(f.board)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        recovery_titles,
        ["Latest 3", "Latest 4", "Latest 5", "Latest 6", "Latest 7"]
    );
    let revision = f.state(&db).await.2;
    let automatic = snapshots.iter().find(|s| !named.contains(&s.id)).unwrap();
    assert!(
        remove_checkpoint(&db, f.owner, f.board, automatic.id, revision)
            .await
            .is_err()
    );
    remove_checkpoint(&db, f.owner, f.board, named[1], revision)
        .await
        .unwrap();
    let revision = f.state(&db).await.2;
    save_checkpoint(&db, f.owner, f.board, revision, "Replacement")
        .await
        .unwrap();
}

#[sqlx::test]
async fn restoring_after_a_source_deletion_preserves_the_drawing_and_other_pins(db: PgPool) {
    let f = Fixture::new(&db).await;
    let deleted_note: Uuid = sqlx::query_scalar(
        "INSERT INTO notes(owner_id,project_id,title,path,body) VALUES ($1,$2,'Brief','Brief.md','Decision') RETURNING id",
    ).bind(f.owner).bind(f.project).fetch_one(&db).await.unwrap();
    let kept_issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues(owner_id,project_id,number,title) VALUES ($1,$2,1,'Implement') RETURNING id",
    ).bind(f.owner).bind(f.project).fetch_one(&db).await.unwrap();
    let mut saved = document("Decision record");
    saved["nodes"][0]["source"] = json!({"kind":"note","id":deleted_note});
    let mut linked = document("Implement")["nodes"][0].clone();
    linked["source"] = json!({"kind":"issue","id":kept_issue});
    linked["x"] = json!(320.0);
    saved["edges"] = json!([{
        "id":Uuid::new_v4(),"from":saved["nodes"][0]["id"],"to":linked["id"],"label":"guides"
    }]);
    saved["nodes"].as_array_mut().unwrap().push(linked);
    sqlx::query("UPDATE canvas_boards SET document=$2 WHERE id=$1")
        .bind(f.board)
        .bind(&saved)
        .execute(&db)
        .await
        .unwrap();
    let snap = save_checkpoint(&db, f.owner, f.board, 1, "Decision map")
        .await
        .unwrap();
    let revision = f.edit(&db, "Later map").await;

    sqlx::query("DELETE FROM notes WHERE id=$1")
        .bind(deleted_note)
        .execute(&db)
        .await
        .unwrap();
    restore_checkpoint(&db, f.owner, f.board, snap, revision)
        .await
        .unwrap();
    saved["nodes"][0].as_object_mut().unwrap().remove("source");
    assert_eq!(
        f.state(&db).await,
        ("Draft".to_owned(), saved, revision + 1)
    );
}

#[sqlx::test]
async fn logged_in_requests_need_csrf_to_change_history(db: PgPool) {
    let f = Fixture::new(&db).await;
    let snap = save_checkpoint(&db, f.owner, f.board, 1, "Draft")
        .await
        .unwrap();
    let revision = f.edit(&db, "Pending").await;
    let raw = hex::encode(rand::random::<[u8; 32]>());
    sqlx::query("INSERT INTO user_sessions(token_hash,user_id) VALUES ($1,$2)")
        .bind(Sha256::digest(raw.as_bytes()).to_vec())
        .bind(f.owner)
        .execute(&db)
        .await
        .unwrap();
    let app = crate::app(AppState::for_test(db.clone()));
    let checkpoints = format!("/api/canvas/{}/checkpoints", f.board);
    for (url, body) in [
        (
            checkpoints.clone(),
            json!({"csrf":"invalid","name":"Forged","revision":revision}),
        ),
        (
            format!("{checkpoints}/{snap}/restore"),
            json!({"csrf":"invalid","revision":revision}),
        ),
        (
            format!("{checkpoints}/{snap}/delete"),
            json!({"csrf":"invalid","revision":revision}),
        ),
    ] {
        let request = Request::post(url)
            .header(header::COOKIE, format!("workspace_session={raw}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        list_checkpoints(&db, f.owner, f.board).await.unwrap().len(),
        1
    );
    assert_eq!(f.state(&db).await.2, revision);
}
