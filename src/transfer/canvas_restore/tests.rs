use crate::{
    AppState,
    auth::Auth,
    transfer::{Mode, Report, apply_vault, archive, export},
};
use anyhow::Result;
use axum::{
    body::to_bytes,
    extract::{Path, State},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

struct Project {
    owner: Uuid,
    id: Uuid,
}

impl Project {
    async fn create(db: &PgPool) -> Result<Self> {
        let owner = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email) VALUES ($1,$2)")
            .bind(owner)
            .bind(format!("{owner}@example.invalid"))
            .execute(db)
            .await?;
        let id = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,key,name) VALUES ($1,'MAP','Service plan') RETURNING id",
        )
        .bind(owner)
        .fetch_one(db)
        .await?;
        Ok(Self { owner, id })
    }

    async fn issue(&self, db: &PgPool, title: &str) -> Result<Uuid> {
        Ok(sqlx::query_scalar(
            "INSERT INTO issues(owner_id,project_id,number,title) VALUES($1,$2,1,$3) RETURNING id",
        )
        .bind(self.owner)
        .bind(self.id)
        .bind(title)
        .fetch_one(db)
        .await?)
    }

    async fn export(&self, db: &PgPool) -> Result<Vec<u8>> {
        let _guard = crate::transfer::TRANSFER_SCENARIO.lock().await;
        let auth = Auth {
            user_id: self.owner,
            email: format!("{}@example.invalid", self.owner),
            csrf: String::new(),
        };
        let response =
            export::download(auth, State(AppState::for_test(db.clone())), Path(self.id)).await?;
        Ok(to_bytes(response.into_body(), 1_000_000).await?.to_vec())
    }

    async fn import(&self, db: &PgPool, bytes: &[u8], mode: Mode) -> Result<Report> {
        let package = archive::parse(bytes, "", 5_000_000)?;
        let mut tx = db.begin().await?;
        crate::store::lock_project(&mut tx, self.owner, self.id).await?;
        let report = apply_vault(&mut tx, self.owner, self.id, package, mode).await?;
        tx.commit().await?;
        Ok(report)
    }

    async fn board(&self, db: &PgPool) -> Result<(Uuid, String, i64, Value)> {
        let boards = sqlx::query_as(
            "SELECT id,title,revision,document FROM canvas_boards WHERE owner_id=$1 AND project_id=$2",
        )
        .bind(self.owner)
        .bind(self.id)
        .fetch_all(db)
        .await?;
        assert_eq!(
            boards.len(),
            1,
            "repeated imports must keep the board identity"
        );
        Ok(boards.into_iter().next().unwrap())
    }
}

fn node(id: Uuid, label: &str, kind: &str, source: Uuid) -> Value {
    json!({"id":id, "kind":"note", "label":label, "body":"", "x":24, "y":52,
           "w":140, "h":62, "source":{"kind":kind, "id":source}})
}

#[sqlx::test]
async fn canvas_archive_remaps_pins_and_snapshots_and_preserves_divergent_work(
    db: PgPool,
) -> Result<()> {
    let origin = Project::create(&db).await?;
    let destination = Project::create(&db).await?;
    let stranger = Project::create(&db).await?;
    let issue = origin.issue(&db, "Trace ingestion").await?;
    let foreign_issue = stranger.issue(&db, "Private issue").await?;
    let note: Uuid = sqlx::query_scalar(
        "INSERT INTO notes(owner_id,project_id,path,title,body) VALUES ($1,$2,'API.md','API','Public contract') RETURNING id",
    ).bind(origin.owner).bind(origin.id).fetch_one(&db).await?;
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let diagram = json!({
        "version":1,
        "nodes":[
            node(first, "Worker", "issue", issue),
            node(second, "Contract", "note", note),
            node(Uuid::new_v4(), "Deleted source", "issue", Uuid::new_v4()),
            node(Uuid::new_v4(), "Foreign source", "issue", foreign_issue),
            node(Uuid::new_v4(), "Wrong kind", "issue", note)
        ],
        "edges":[{"id":Uuid::new_v4(), "from":first, "to":second, "label":"reads"}],
        "strokes":[{"id":Uuid::new_v4(), "points":[[1,2],[1.5,3]]}]
    });
    let board: Uuid = sqlx::query_scalar(
        "INSERT INTO canvas_boards(owner_id,project_id,title,document) VALUES ($1,$2,'Ingestion',$3) RETURNING id",
    ).bind(origin.owner).bind(origin.id).bind(&diagram).fetch_one(&db).await?;
    for (name, automatic) in [("Review", false), ("Before restore", true)] {
        sqlx::query(
            "INSERT INTO canvas_checkpoints(owner_id,project_id,board_id,name,title,document,automatic)
             VALUES ($1,$2,$3,$4,'Ingestion',$5,$6)",
        ).bind(origin.owner).bind(origin.id).bind(board).bind(name).bind(&diagram).bind(automatic).execute(&db).await?;
    }

    let first_archive = origin.export(&db).await?;
    let report = destination
        .import(&db, &first_archive, Mode::KeepLocal)
        .await?;
    assert_eq!(report.conflicts, 0);
    assert!(
        !report.warnings.is_empty(),
        "the review must disclose detached pin targets"
    );
    let (local_board, title, revision, restored) = destination.board(&db).await?;
    assert_ne!(local_board, board);
    assert_eq!(title, "Ingestion");
    let local_issue: Uuid = sqlx::query_scalar("SELECT id FROM issues WHERE project_id=$1")
        .bind(destination.id)
        .fetch_one(&db)
        .await?;
    let local_note: Uuid = sqlx::query_scalar("SELECT id FROM notes WHERE project_id=$1")
        .bind(destination.id)
        .fetch_one(&db)
        .await?;
    assert_ne!(local_issue, issue);
    assert_ne!(local_note, note);
    let mut expected = diagram.clone();
    expected["nodes"][0]["source"]["id"] = json!(local_issue);
    expected["nodes"][1]["source"]["id"] = json!(local_note);
    for n in &mut expected["nodes"].as_array_mut().unwrap()[2..] {
        n.as_object_mut().unwrap().remove("source");
    }
    assert_eq!(
        restored, expected,
        "only pin IDs change in the actual board"
    );

    let identical = destination
        .import(&db, &first_archive, Mode::KeepLocal)
        .await?;
    assert_eq!(
        (identical.conflicts, identical.created, identical.updated),
        (0, 0, 0)
    );
    assert_eq!(destination.board(&db).await?.2, revision);
    let snapshots: Vec<(Uuid, String, bool, Value)> = sqlx::query_as(
        "SELECT board_id,name,automatic,document FROM canvas_checkpoints
         WHERE project_id=$1 AND owner_id=$2 ORDER BY automatic",
    )
    .bind(destination.id)
    .bind(destination.owner)
    .fetch_all(&db)
    .await?;
    assert_eq!(
        snapshots,
        vec![
            (local_board, "Review".to_string(), false, expected.clone()),
            (local_board, "Before restore".to_string(), true, expected),
        ]
    );

    sqlx::query(
        "UPDATE canvas_boards SET title='Locally revised', revision=revision+1 WHERE id=$1",
    )
    .bind(local_board)
    .execute(&db)
    .await?;
    destination
        .import(&db, &first_archive, Mode::KeepLocal)
        .await?;
    assert_eq!(destination.board(&db).await?.1, "Locally revised");
    sqlx::query("UPDATE canvas_boards SET title='New design', revision=revision+1 WHERE id=$1")
        .bind(board)
        .execute(&db)
        .await?;
    let changed_archive = origin.export(&db).await?;
    assert_eq!(
        destination
            .import(&db, &changed_archive, Mode::KeepLocal)
            .await?
            .conflicts,
        1
    );
    assert_eq!(destination.board(&db).await?.1, "Locally revised");
    destination
        .import(&db, &changed_archive, Mode::UseIncoming)
        .await?;
    assert_eq!(destination.board(&db).await?.1, "New design");
    assert_eq!(
        origin.board(&db).await?.3,
        diagram,
        "restoration must never alter source data"
    );
    Ok(())
}

#[sqlx::test]
async fn merging_two_valid_checkpoint_histories_preserves_the_destination_on_overflow(
    db: PgPool,
) -> Result<()> {
    let origin = Project::create(&db).await?;
    let destination = Project::create(&db).await?;
    let document = json!({"version":1, "nodes":[], "edges":[], "strokes":[]});
    let board: Uuid = sqlx::query_scalar(
        "INSERT INTO canvas_boards(owner_id,project_id,title,document) VALUES($1,$2,'Baseline',$3) RETURNING id",
    ).bind(origin.owner).bind(origin.id).bind(&document).fetch_one(&db).await?;
    sqlx::query(
        "INSERT INTO canvas_checkpoints(owner_id,project_id,board_id,name,title,document,automatic)
         SELECT $1,$2,$3,'Recovery ' || n,'Baseline',$4,true FROM generate_series(1,5) n",
    )
    .bind(origin.owner)
    .bind(origin.id)
    .bind(board)
    .bind(&document)
    .execute(&db)
    .await?;
    destination
        .import(&db, &origin.export(&db).await?, Mode::KeepLocal)
        .await?;
    let baseline = destination.board(&db).await?;

    // Both boards have five recovery copies. The source rotates the oldest one,
    // while omission from an archive must preserve the destination's copy.
    sqlx::query("DELETE FROM canvas_checkpoints WHERE board_id=$1 AND name='Recovery 1'")
        .bind(board)
        .execute(&db)
        .await?;
    sqlx::query(
        "INSERT INTO canvas_checkpoints(owner_id,project_id,board_id,name,title,document,automatic)
         VALUES ($1,$2,$3,'New recovery','New state',$4,true)",
    )
    .bind(origin.owner)
    .bind(origin.id)
    .bind(board)
    .bind(&document)
    .execute(&db)
    .await?;
    sqlx::query("UPDATE canvas_boards SET title='New state', revision=revision+1 WHERE id=$1")
        .bind(board)
        .execute(&db)
        .await?;
    let changed = origin.export(&db).await?;
    let error = destination
        .import(&db, &changed, Mode::UseIncoming)
        .await
        .err()
        .expect("merging histories would exceed the five recovery slots");
    assert!(matches!(
        error.downcast_ref::<crate::AppError>(),
        Some(crate::AppError::Conflict(_))
    ));
    assert_eq!(destination.board(&db).await?, baseline);
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM canvas_checkpoints WHERE board_id=$1 ORDER BY name")
            .bind(baseline.0)
            .fetch_all(&db)
            .await?;
    assert_eq!(
        names,
        [
            "Recovery 1",
            "Recovery 2",
            "Recovery 3",
            "Recovery 4",
            "Recovery 5"
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn checkpoint_bytes_count_at_preview_and_commit_and_leave_no_partial_restore(
    db: PgPool,
) -> Result<()> {
    use crate::{AppError, store, transfer};
    use axum::{
        Form,
        body::Body,
        extract::{FromRequest, Multipart},
        http::{Request, StatusCode, header},
        response::IntoResponse,
    };

    let source = Project::create(&db).await?;
    let dest = Project::create(&db).await?;
    let empty = json!({"version":1,"nodes":[],"edges":[],"strokes":[]});
    let board: Uuid = sqlx::query_scalar(
        "INSERT INTO canvas_boards(owner_id,project_id,title,document) VALUES($1,$2,'Map',$3) RETURNING id",
    ).bind(source.owner).bind(source.id).bind(&empty).fetch_one(&db).await?;
    let mut prior = empty;
    prior["nodes"] = json!([{
        "id": Uuid::new_v4(), "kind":"note", "label":"Old approach", "body":"é".repeat(1500),
        "x":10, "y":20, "w":180, "h":68
    }]);
    for (name, automatic) in [("Plan A", false), ("Before restore", true)] {
        sqlx::query(
            "INSERT INTO canvas_checkpoints(owner_id,project_id,board_id,name,title,document,automatic)
             VALUES($1,$2,$3,$4,'Earlier map',$5,$6)",
        ).bind(source.owner).bind(source.id).bind(board).bind(name).bind(&prior).bind(automatic)
        .execute(&db).await?;
    }
    let zip = source.export(&db).await?;
    // Compute the logical bytes independently so omitting either snapshot kind fails this test.
    let required: i64 = sqlx::query_scalar(
        "SELECT (SELECT octet_length(title)+octet_length(document::text) FROM canvas_boards WHERE id=$1)
              + (SELECT sum(octet_length(name)+octet_length(title)+octet_length(document::text))
                 FROM canvas_checkpoints WHERE board_id=$1)",
    ).bind(board).fetch_one(&db).await?;
    let limit = store::limits("free").storage_bytes;
    let exact = limit - required;
    let state = AppState::for_test(db.clone());
    let csrf = "a".repeat(64);
    let auth = || Auth {
        user_id: dest.owner,
        email: format!("{}@example.invalid", dest.owner),
        csrf: csrf.clone(),
    };
    let set_existing_usage = |bytes: i64| {
        // The owner's pre-existing balance stands in for unrelated workspace files.
        sqlx::query("UPDATE users SET content_bytes=$2 WHERE id=$1")
            .bind(dest.owner)
            .bind(bytes)
            .execute(&db)
    };
    let preview = || async {
        let boundary = "ratio-test-canvas-import";
        let mut body = Vec::new();
        for (name, value) in [
            ("csrf", csrf.clone()),
            ("project", dest.id.to_string()),
            ("format", "vault".into()),
        ] {
            body.extend_from_slice(format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            ).as_bytes());
        }
        body.extend_from_slice(format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"map.zip\"\r\nContent-Type: application/zip\r\n\r\n"
        ).as_bytes());
        body.extend_from_slice(&zip);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let request = Request::builder()
            .method("POST")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let multipart = Multipart::from_request(request, &()).await.unwrap();
        let _guard = transfer::TRANSFER_SCENARIO.lock().await;
        transfer::preview(auth(), State(state.clone()), multipart).await
    };
    let counts = || {
        sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT (SELECT count(*) FROM canvas_boards WHERE owner_id=$1),
                (SELECT count(*) FROM canvas_checkpoints WHERE owner_id=$1),
                (SELECT count(*) FROM transfer_history_receipts WHERE owner_id=$1)",
        )
        .bind(dest.owner)
        .fetch_one(&db)
    };

    set_existing_usage(exact + 1).await?;
    assert!(
        matches!(preview().await, Err(AppError::Conflict(msg)) if msg.contains("storage limit"))
    );
    assert_eq!(counts().await?, (0, 0, 0));
    assert_eq!(store::storage_used(&db, dest.owner).await?, exact + 1);

    set_existing_usage(exact).await?;
    assert_eq!(
        preview().await?.into_response().status(),
        StatusCode::SEE_OTHER
    );
    let (id, report): (Uuid, Value) =
        sqlx::query_as("SELECT id,report FROM transfer_previews WHERE owner_id=$1")
            .bind(dest.owner)
            .fetch_one(&db)
            .await?;
    assert_eq!(report["growth"], required);
    assert_eq!(
        counts().await?,
        (0, 0, 0),
        "a successful preview still rolls its simulation back"
    );
    let commit = || async {
        let _guard = transfer::TRANSFER_SCENARIO.lock().await;
        transfer::commit(
            auth(),
            State(state.clone()),
            Path(id),
            Form(transfer::Decision {
                csrf: csrf.clone(),
                decision: "incoming".into(),
            }),
        )
        .await
    };
    // Space elsewhere in the account may change without invalidating this project preview.
    set_existing_usage(exact + 1).await?;
    assert!(
        matches!(commit().await, Err(AppError::Conflict(msg)) if msg.contains("storage limit"))
    );
    assert_eq!(counts().await?, (0, 0, 0));
    assert_eq!(store::storage_used(&db, dest.owner).await?, exact + 1);
    set_existing_usage(exact).await?;
    assert_eq!(
        commit().await?.into_response().status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(counts().await?, (1, 2, 3));
    assert_eq!(store::storage_used(&db, dest.owner).await?, limit);
    Ok(())
}
