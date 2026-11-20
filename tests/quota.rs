use anyhow::Result;
use ratio_server::{AppError, store};
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(db: &PgPool) -> Result<(Uuid, Uuid)> {
    let (owner, project) = sqlx::query_as(
        "WITH owner AS (INSERT INTO users(email) VALUES(gen_random_uuid()::text || '@example.invalid') RETURNING id)
         INSERT INTO projects(owner_id, name, key) SELECT id, 'Sample', 'APP' FROM owner RETURNING owner_id, id",
    )
    .fetch_one(db)
    .await?;
    Ok((owner, project))
}

fn bytes(fields: &[&str]) -> i64 {
    fields.iter().map(|value| value.len() as i64).sum()
}

#[sqlx::test(migrations = false)]
async fn canvas_meter_counts_each_retained_version(db: PgPool) -> Result<()> {
    for migration in ratio_server::MIGRATIONS
        .iter()
        .take_while(|m| m.version < 28)
    {
        sqlx::raw_sql(&migration.sql).execute(&db).await?;
    }
    let (owner, project) = workspace(&db).await?;
    let (other, _) = workspace(&db).await?;
    let (board, stored): (Uuid, String) = sqlx::query_as(
        r#"INSERT INTO canvas_boards(owner_id,project_id,title,document)
           VALUES($1,$2,'Schéma','{"nodes":[],"edges":[],"strokes":[{"id":"78aa48dc-eccc-439c-ad89-12f20fa581c2","points":[[0,0],[24,12]]}],"version":1}')
           RETURNING id, document::text"#,
    )
    .bind(owner)
    .bind(project)
    .fetch_one(&db)
    .await?;
    let checkpoint: Uuid = sqlx::query_scalar(
        "INSERT INTO canvas_checkpoints(owner_id,project_id,board_id,name,title,document)
         SELECT owner_id,project_id,id,'Étape',title,document FROM canvas_boards WHERE id=$1
         RETURNING id",
    )
    .bind(board)
    .fetch_one(&db)
    .await?;
    sqlx::query(
        "INSERT INTO canvas_checkpoints(owner_id,project_id,board_id,name,title,document,automatic)
         SELECT owner_id,project_id,id,'Before restore',title,document,true FROM canvas_boards WHERE id=$1",
    )
    .bind(board)
    .execute(&db)
    .await?;
    let base = bytes(&["Schéma", &stored]);
    let named = base + bytes(&["Étape"]);
    let automatic = base + bytes(&["Before restore"]);
    sqlx::raw_sql(include_str!(
        "../migrations/0028_canvas_storage_and_transfer.sql"
    ))
    .execute(&db)
    .await?;
    assert_eq!(
        store::storage_used(&db, owner).await?,
        base + named + automatic
    );

    sqlx::query(
        "INSERT INTO transfer_previews(owner_id,project_id,format,folder,payload,report)
         VALUES($1,$2,'vault','','0'::bytea,'{}')",
    )
    .bind(owner)
    .bind(project)
    .execute(&db)
    .await?;
    // Both checkpoints still contain the original stroke after clearing the board.
    let empty: String = sqlx::query_scalar(
        "UPDATE canvas_boards SET title=title || '🚀',document=jsonb_set(document,'{strokes}','[]')
         WHERE id=$1 RETURNING document::text",
    )
    .bind(board)
    .fetch_one(&db)
    .await?;
    let cleared = bytes(&["Schéma🚀", &empty]);
    assert!(cleared < base);
    assert_eq!(
        store::storage_used(&db, owner).await?,
        cleared + named + automatic
    );
    let stale: bool = sqlx::query_scalar("SELECT stale FROM transfer_previews WHERE owner_id=$1")
        .bind(owner)
        .fetch_one(&db)
        .await?;
    assert!(stale);
    sqlx::query("UPDATE transfer_previews SET stale=false WHERE owner_id=$1")
        .bind(owner)
        .execute(&db)
        .await?;
    sqlx::query(
        "UPDATE canvas_checkpoints SET title='v2', name='N', document=$2::jsonb WHERE id=$1",
    )
    .bind(checkpoint)
    .bind(&empty)
    .execute(&db)
    .await?;
    assert_eq!(
        store::storage_used(&db, owner).await?,
        cleared + bytes(&["v2", "N", &empty]) + automatic
    );
    sqlx::query("DELETE FROM canvas_checkpoints WHERE id=$1")
        .bind(checkpoint)
        .execute(&db)
        .await?;
    assert_eq!(store::storage_used(&db, owner).await?, cleared + automatic);
    let stale: bool = sqlx::query_scalar("SELECT stale FROM transfer_previews WHERE owner_id=$1")
        .bind(owner)
        .fetch_one(&db)
        .await?;
    assert!(stale);
    assert_eq!(store::storage_used(&db, other).await?, 0);
    sqlx::query("DELETE FROM canvas_checkpoints WHERE board_id=$1 AND automatic")
        .bind(board)
        .execute(&db)
        .await?;
    assert_eq!(store::storage_used(&db, owner).await?, cleared);
    sqlx::query("DELETE FROM projects WHERE id=$1")
        .bind(project)
        .execute(&db)
        .await?;
    assert_eq!(store::storage_used(&db, owner).await?, 0);
    Ok(())
}

#[sqlx::test(migrations = false)]
async fn migration_preserves_existing_source_and_workflow_charges(db: PgPool) -> Result<()> {
    for migration in ratio_server::MIGRATIONS
        .iter()
        .take_while(|m| m.version < 18)
    {
        sqlx::raw_sql(&migration.sql).execute(&db).await?;
    }
    let (owner, project) = workspace(&db).await?;
    sqlx::query(
        "INSERT INTO notes(owner_id,project_id,title,path,body) VALUES($1,$2,'Résumé','a.md','β')",
    )
    .bind(owner)
    .bind(project)
    .execute(&db)
    .await?;
    sqlx::query("INSERT INTO workflow_views(owner_id,name,search) VALUES($1,'Focused','🌒')")
        .bind(owner)
        .execute(&db)
        .await?;
    sqlx::query(
        "INSERT INTO issue_templates(owner_id,name,title,body,next_action,status,priority,kind)
                 VALUES($1,'Start','Title','Body','Next','ready','normal','feature')",
    )
    .bind(owner)
    .execute(&db)
    .await?;
    sqlx::query(
        "INSERT INTO transfer_blobs(owner_id,project_id,path,content_type,bytes,sha256,trashed_at)
                 VALUES($1,$2,'a.png','image/png',decode('0000ff','hex'),repeat('0',64),now())",
    )
    .bind(owner)
    .bind(project)
    .execute(&db)
    .await?;
    sqlx::query(
        "INSERT INTO milestones(owner_id,project_id,title,details) VALUES($1,$2,'M','Jalão')",
    )
    .bind(owner)
    .bind(project)
    .execute(&db)
    .await?;

    sqlx::raw_sql(include_str!("../migrations/0018_content_usage.sql"))
        .execute(&db)
        .await?;
    let expected = 3 + bytes(&[
        "Résumé", "a.md", "β", "Focused", "🌒", "Start", "Title", "Body", "Next", "M", "Jalão",
    ]);
    assert_eq!(store::storage_used(&db, owner).await?, expected);
    sqlx::query("UPDATE notes SET body=body || 'é' WHERE owner_id=$1")
        .bind(owner)
        .execute(&db)
        .await?;
    assert_eq!(store::storage_used(&db, owner).await?, expected + 2);
    Ok(())
}

#[sqlx::test]
async fn named_and_recovery_checkpoints_share_the_workspace_cap(db: PgPool) -> Result<()> {
    let (owner, project) = workspace(&db).await?;
    let (board, document): (Uuid, String) = sqlx::query_as(
        r#"INSERT INTO canvas_boards(owner_id,project_id,title,document)
           VALUES($1,$2,'Map','{"version":1,"nodes":[],"edges":[],"strokes":[]}')
           RETURNING id,document::text"#,
    )
    .bind(owner)
    .bind(project)
    .fetch_one(&db)
    .await?;
    let cap = store::limits("free").storage_bytes;
    let board_bytes = bytes(&["Map", &document]);
    let snapshot_bytes = board_bytes + bytes(&["v1"]);
    let reserved = i32::try_from(board_bytes + 2 * snapshot_bytes)?;
    // Fill the account with real content, leaving space for exactly two copies.
    sqlx::query(
        "INSERT INTO transfer_blobs(owner_id,project_id,path,content_type,bytes,sha256)
         SELECT $1,$2,n::text,'image/png',
                repeat('x',CASE WHEN n=20 THEN 5000000-$3 ELSE 5000000 END)::bytea,repeat('0',64)
           FROM generate_series(1,20) n",
    )
    .bind(owner)
    .bind(project)
    .bind(reserved)
    .execute(&db)
    .await?;
    assert_eq!(
        store::storage_used(&db, owner).await?,
        cap - 2 * snapshot_bytes
    );
    let capture = "INSERT INTO canvas_checkpoints(owner_id,project_id,board_id,title,name,document,automatic)
                   SELECT owner_id,project_id,id,title,'v1',document,$2 FROM canvas_boards WHERE id=$1";
    for automatic in [false, true] {
        sqlx::query(capture)
            .bind(board)
            .bind(automatic)
            .execute(&db)
            .await?;
    }
    assert_eq!(store::storage_used(&db, owner).await?, cap);
    for automatic in [false, true] {
        let err = sqlx::query(capture)
            .bind(board)
            .bind(automatic)
            .execute(&db)
            .await
            .expect_err("each retained version must fit the remaining quota");
        assert_eq!(
            err.as_database_error().and_then(|e| e.constraint()),
            Some("workspace_storage_limit")
        );
        assert_eq!(store::storage_used(&db, owner).await?, cap);
    }
    let mut replace = db.begin().await?;
    sqlx::query(capture)
        .bind(board)
        .bind(true)
        .execute(&mut *replace)
        .await?;
    sqlx::query("DELETE FROM canvas_checkpoints WHERE board_id=$1 AND NOT automatic")
        .bind(board)
        .execute(&mut *replace)
        .await?;
    replace.commit().await?;
    assert_eq!(store::storage_used(&db, owner).await?, cap);
    sqlx::query("DELETE FROM canvas_checkpoints WHERE board_id=$1")
        .bind(board)
        .execute(&db)
        .await?;
    assert_eq!(
        store::storage_used(&db, owner).await?,
        cap - 2 * snapshot_bytes
    );
    Ok(())
}

#[sqlx::test]
async fn source_meter_counts_utf8_once_and_cascades_at_the_project_boundary(
    db: PgPool,
) -> Result<()> {
    let (owner, project) = workspace(&db).await?;
    let (other, other_project) = workspace(&db).await?;
    let issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues(owner_id,project_id,number,title,body,next_action,labels)
         VALUES($1,$2,1,'Ship','ö','Go',ARRAY['été','🚀']) RETURNING id",
    )
    .bind(owner)
    .bind(project)
    .fetch_one(&db)
    .await?;
    let release: Uuid = sqlx::query_scalar(
        "INSERT INTO releases(owner_id,project_id,name,version,source_url)
         VALUES($1,$2,'Launch','1.1','https://example.invalid') RETURNING id",
    )
    .bind(owner)
    .bind(project)
    .fetch_one(&db)
    .await?;
    sqlx::query("INSERT INTO release_entries(owner_id,project_id,release_id,issue_id,number,title,issue_revision)
                 VALUES($1,$2,$3,$4,1,'Ship',1)")
        .bind(owner).bind(project).bind(release).bind(issue).execute(&db).await?;
    sqlx::query("UPDATE releases SET published_on=current_date,published_at=now(),publish_command=gen_random_uuid() WHERE id=$1")
        .bind(release).execute(&db).await?;
    sqlx::query("INSERT INTO release_amendments(owner_id,project_id,actor_id,release_id,issue_id,reason,replacement)
                 VALUES($1,$2,$1,$3,$4,'Fix typo','Shipped')")
        .bind(owner).bind(project).bind(release).bind(issue).execute(&db).await?;
    let milestone: Uuid = sqlx::query_scalar(
        "INSERT INTO milestones(owner_id,project_id,title,details) VALUES($1,$2,'β','Stabilize') RETURNING id",
    )
    .bind(owner).bind(project).fetch_one(&db).await?;
    sqlx::query("INSERT INTO roadmap_weeks_off(owner_id,project_id,week_of,note) VALUES($1,$2,'2026-11-16','Congé')")
        .bind(owner).bind(project).execute(&db).await?;
    let expected = bytes(&[
        "Ship",
        "ö",
        "Go",
        "été",
        "🚀",
        "Launch",
        "1.1",
        "https://example.invalid",
        "Fix typo",
        "Shipped",
        "β",
        "Stabilize",
        "Congé",
    ]);
    assert_eq!(store::storage_used(&db, owner).await?, expected);
    assert_eq!(store::storage_used(&db, other).await?, 0);

    // Archiving, publishing and taking history snapshots keep the same source.
    sqlx::query("UPDATE projects SET archived=true WHERE id=$1")
        .bind(project)
        .execute(&db)
        .await?;
    assert_eq!(store::storage_used(&db, owner).await?, expected);
    sqlx::query("UPDATE projects SET archived=false WHERE id=$1")
        .bind(project)
        .execute(&db)
        .await?;
    sqlx::query("UPDATE milestones SET owner_id=$2,project_id=$3 WHERE id=$1")
        .bind(milestone)
        .bind(other)
        .bind(other_project)
        .execute(&db)
        .await?;
    let moved = bytes(&["β", "Stabilize"]);
    assert_eq!(store::storage_used(&db, owner).await?, expected - moved);
    assert_eq!(store::storage_used(&db, other).await?, moved);
    sqlx::query("DELETE FROM projects WHERE id=$1")
        .bind(project)
        .execute(&db)
        .await?;
    assert_eq!(store::storage_used(&db, owner).await?, 0);
    assert_eq!(store::storage_used(&db, other).await?, moved);
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(other)
        .execute(&db)
        .await?;
    assert_eq!(store::storage_used(&db, other).await?, 0);
    Ok(())
}

#[sqlx::test]
async fn atomic_imports_allow_net_reductions_and_reject_competing_growth(db: PgPool) -> Result<()> {
    let (owner, project) = workspace(&db).await?;
    let cap = store::limits("free").storage_bytes;
    let milestone: Uuid = sqlx::query_scalar(
        "INSERT INTO milestones(owner_id,project_id,title,details) VALUES($1,$2,'M','abc') RETURNING id",
    )
    .bind(owner).bind(project).fetch_one(&db).await?;
    // Place the workspace exactly at its free limit using real, highly
    // compressible images. Later downgrade tests use the same stored content.
    sqlx::query(
        "INSERT INTO transfer_blobs(owner_id,project_id,path,content_type,bytes,sha256)
         SELECT $1,$2,n::text,'image/png',repeat('x',CASE WHEN n=20 THEN 4999996 ELSE 5000000 END)::bytea,repeat('0',64)
         FROM generate_series(1,20) n",
    )
    .bind(owner).bind(project).execute(&db).await?;
    assert_eq!(store::storage_used(&db, owner).await?, cap);

    let board_error = sqlx::query(
        r#"INSERT INTO canvas_boards(owner_id,project_id,title,document)
           VALUES($1,$2,'Sketch','{"version":1,"nodes":[],"edges":[],"strokes":[]}')"#,
    )
    .bind(owner)
    .bind(project)
    .execute(&db)
    .await
    .expect_err("board growth must respect the same workspace cap");
    assert_eq!(
        board_error.as_database_error().and_then(|e| e.constraint()),
        Some("workspace_storage_limit")
    );
    assert_eq!(store::storage_used(&db, owner).await?, cap);

    let add_week = "INSERT INTO roadmap_weeks_off(owner_id,project_id,week_of,note) VALUES($1,$2,'2026-11-16','é')";
    let mut preview = db.begin().await?;
    sqlx::query(add_week)
        .bind(owner)
        .bind(project)
        .execute(&mut *preview)
        .await?;
    assert_eq!(store::storage_used_tx(&mut preview, owner).await?, cap + 2);
    preview.rollback().await?;
    assert_eq!(store::storage_used(&db, owner).await?, cap);

    let mut rejected = db.begin().await?;
    sqlx::query(add_week)
        .bind(owner)
        .bind(project)
        .execute(&mut *rejected)
        .await?;
    let failure = rejected.commit().await.unwrap_err();
    assert_eq!(
        failure
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("workspace_storage_limit")
    );
    assert!(matches!(AppError::from(failure), AppError::Conflict(_)));
    assert_eq!(store::storage_used(&db, owner).await?, cap);

    let mut replacement = db.begin().await?;
    sqlx::query(add_week)
        .bind(owner)
        .bind(project)
        .execute(&mut *replacement)
        .await?;
    sqlx::query("UPDATE milestones SET details='' WHERE id=$1")
        .bind(milestone)
        .execute(&mut *replacement)
        .await?;
    replacement.commit().await?;
    assert_eq!(store::storage_used(&db, owner).await?, cap - 1);

    // Independent direct writers race for the last byte. Exactly one commits.
    let insert = || async {
        sqlx::query("INSERT INTO milestones(owner_id,project_id,title) VALUES($1,$2,'x')")
            .bind(owner)
            .bind(project)
            .execute(&db)
            .await
    };
    let (left, right) = tokio::join!(insert(), insert());
    let failure = match (left, right) {
        (Ok(_), Err(e)) | (Err(e), Ok(_)) => e,
        _ => panic!("exactly one writer should reserve the last byte"),
    };
    assert_eq!(
        failure
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("workspace_storage_limit")
    );
    assert_eq!(store::storage_used(&db, owner).await?, cap);

    sqlx::query("UPDATE users SET plan='plus' WHERE id=$1")
        .bind(owner)
        .execute(&db)
        .await?;
    sqlx::query("UPDATE milestones SET details='previous' WHERE id=$1")
        .bind(milestone)
        .execute(&db)
        .await?;
    sqlx::query("UPDATE users SET plan='free' WHERE id=$1")
        .bind(owner)
        .execute(&db)
        .await?;
    let mut reduced = db.begin().await?;
    sqlx::query("UPDATE milestones SET details='' WHERE id=$1")
        .bind(milestone)
        .execute(&mut *reduced)
        .await?;
    sqlx::query("UPDATE milestones SET details='small' WHERE id=$1")
        .bind(milestone)
        .execute(&mut *reduced)
        .await?;
    reduced.commit().await?;
    assert_eq!(store::storage_used(&db, owner).await?, cap + 5);
    sqlx::query("DELETE FROM milestones WHERE id=$1")
        .bind(milestone)
        .execute(&db)
        .await?;
    assert_eq!(store::storage_used(&db, owner).await?, cap - 1);
    Ok(())
}
