use anyhow::Result;
use solo_tracker::{AppError, store};
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
async fn migration_preserves_existing_source_and_workflow_charges(db: PgPool) -> Result<()> {
    for migration in solo_tracker::MIGRATIONS
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
