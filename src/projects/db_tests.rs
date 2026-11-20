use super::{counts, create_project, delete_project, edit_project, set_archived};
use axum::response::IntoResponse;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

async fn account(db: &PgPool, plan: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id,email,plan) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(format!("{id}@example.invalid"))
        .bind(plan)
        .execute(db)
        .await
        .unwrap();
    id
}

async fn note(db: &PgPool, owner: Uuid, project: Uuid) {
    sqlx::query("INSERT INTO notes(owner_id,project_id,title,path,body) VALUES($1,$2,'Plan','Plan.md','Keep this text')")
        .bind(owner).bind(project).execute(db).await.unwrap();
}

#[sqlx::test]
async fn restore_and_create_contend_for_the_last_project_slot(db: PgPool) {
    let owner = account(&db, "free").await;
    let original = create_project(&db, owner, "Original", "ORIG")
        .await
        .unwrap();
    note(&db, owner, original).await;
    set_archived(&db, owner, original, true).await.unwrap();
    set_archived(&db, owner, original, true).await.unwrap();

    let mut guard = db.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .execute(&mut *guard)
        .await
        .unwrap();
    // Release the owner row only after both contenders reach a PostgreSQL lock.
    let release = async {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                sqlx::query("SELECT pg_stat_clear_snapshot()")
                    .execute(&mut *guard)
                    .await
                    .unwrap();
                let waiting: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database()
                     AND wait_event_type='Lock' AND cardinality(pg_blocking_pids(pid)) > 0",
                )
                .fetch_one(&mut *guard)
                .await
                .unwrap();
                if waiting >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("both project mutations must acquire the owner's lock");
        guard.commit().await.unwrap();
    };
    let (created, restored, ()) = tokio::join!(
        create_project(&db, owner, "New", "NEW"),
        set_archived(&db, owner, original, false),
        release
    );
    let active = match (created, restored) {
        (Ok(new), Err(error)) => {
            assert!(error.into_response().status().is_client_error());
            new
        }
        (Err(error), Ok(())) => {
            assert!(error.into_response().status().is_client_error());
            original
        }
        outcomes => panic!("exactly one project should occupy the slot: {outcomes:?}"),
    };
    set_archived(&db, owner, active, false).await.unwrap();
    let active_ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM projects WHERE owner_id=$1 AND NOT archived")
            .bind(owner)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(active_ids, vec![active]);
    assert_eq!(counts(&db, owner, original).await.unwrap().notes, 1);
}

#[sqlx::test]
async fn edits_respect_workspace_keys_ownership_and_archive_state(db: PgPool) {
    let owner = account(&db, "plus").await;
    let other = account(&db, "free").await;
    let first = create_project(&db, owner, "First", "alpha").await.unwrap();
    let second = create_project(&db, owner, "Second", "beta").await.unwrap();
    create_project(&db, other, "Separate", "alpha")
        .await
        .unwrap();
    assert!(
        create_project(&db, owner, "Duplicate", "ALPHA")
            .await
            .unwrap_err()
            .into_response()
            .status()
            .is_client_error()
    );
    assert!(
        edit_project(&db, owner, second, "Collision", "aLpHa")
            .await
            .unwrap_err()
            .into_response()
            .status()
            .is_client_error()
    );
    assert!(
        edit_project(&db, other, first, "Stolen", "OTHER")
            .await
            .unwrap_err()
            .into_response()
            .status()
            .is_client_error()
    );
    assert!(
        set_archived(&db, other, first, true)
            .await
            .unwrap_err()
            .into_response()
            .status()
            .is_client_error()
    );
    edit_project(&db, owner, second, "Second revision", "rev2")
        .await
        .unwrap();
    set_archived(&db, owner, second, true).await.unwrap();
    assert!(
        edit_project(&db, owner, second, "Read-only", "GONE")
            .await
            .unwrap_err()
            .into_response()
            .status()
            .is_client_error()
    );
    let rows: Vec<(String, String, bool)> =
        sqlx::query_as("SELECT name,key,archived FROM projects WHERE owner_id=$1 ORDER BY key")
            .bind(owner)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("First".into(), "ALPHA".into(), false),
            ("Second revision".into(), "REV2".into(), true)
        ]
    );
}

#[sqlx::test]
async fn generated_keys_skip_reserved_keys_even_when_their_projects_are_archived(db: PgPool) {
    let owner = account(&db, "plus").await;
    let initial = create_project(&db, owner, "Weather Station", "")
        .await
        .unwrap();
    set_archived(&db, owner, initial, true).await.unwrap();
    let next = create_project(&db, owner, "Web Service", "").await.unwrap();
    let keys: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id,key FROM projects WHERE owner_id=$1 ORDER BY key")
            .bind(owner)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(keys, vec![(initial, "WS".into()), (next, "WS2".into())]);
}

#[sqlx::test]
async fn permanent_delete_requires_the_owner_and_the_current_key_and_removes_only_that_project(
    db: PgPool,
) {
    let owner = account(&db, "plus").await;
    let other = account(&db, "free").await;
    let doomed = create_project(&db, owner, "Obsolete", "OLD").await.unwrap();
    let retained = create_project(&db, owner, "Retained", "KEEP")
        .await
        .unwrap();
    for project in [doomed, retained] {
        note(&db, owner, project).await;
    }
    edit_project(&db, owner, doomed, "Obsolete", "DROP")
        .await
        .unwrap();
    // Archiving is reversible even when a GitHub repository is mapped and leased.
    sqlx::query("INSERT INTO github_connections(owner_id,user_remote_id,access_cipher) VALUES($1,5291,'test'::bytea)")
        .bind(owner).execute(&db).await.unwrap();
    for (project, repo) in [(doomed, 271_i64), (retained, 272)] {
        sqlx::query("INSERT INTO github_repos(owner_id,project_id,repo_id,installation_id,lease_id,lease_until,repo_grant_until,alert_grant_until)
            VALUES($1,$2,$3,691,gen_random_uuid(),now()+interval '1 minute',now()+interval '1 hour', now()+interval '1 hour')")
            .bind(owner).bind(project).bind(repo).execute(&db).await.unwrap();
    }
    let retained_repo: Uuid =
        sqlx::query_scalar("SELECT id FROM github_repos WHERE owner_id=$1 AND repo_id=272")
            .bind(owner)
            .fetch_one(&db)
            .await
            .unwrap();
    let preferred = sqlx::query(
        "INSERT INTO billing_preferences(owner_id,resource_type,resource_id)
        SELECT owner_id,'project',id FROM projects WHERE owner_id=$1
        UNION ALL SELECT owner_id,'repo',id FROM github_repos WHERE owner_id=$1",
    )
    .bind(owner)
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(preferred.rows_affected(), 4);
    set_archived(&db, owner, doomed, true).await.unwrap();
    let paused: (bool, bool, bool, bool, bool) = sqlx::query_as("SELECT billing_paused,lease_id IS NULL,lease_until IS NULL,repo_grant_until IS NULL,alert_grant_until IS NULL FROM github_repos WHERE owner_id=$1 AND repo_id=271")
        .bind(owner).fetch_one(&db).await.unwrap();
    assert_eq!(paused, (true, true, true, true, true));
    let sibling_lease: bool = sqlx::query_scalar("SELECT NOT billing_paused AND lease_id IS NOT NULL AND lease_until IS NOT NULL FROM github_repos WHERE owner_id=$1 AND repo_id=272")
        .bind(owner).fetch_one(&db).await.unwrap();
    assert!(sibling_lease);
    for (actor, key) in [
        (other, "DROP"),
        (owner, "OLD"),
        (owner, "drop"),
        (owner, ""),
    ] {
        assert!(
            delete_project(&db, actor, doomed, key)
                .await
                .unwrap_err()
                .into_response()
                .status()
                .is_client_error()
        );
    }
    assert_eq!(counts(&db, owner, doomed).await.unwrap().notes, 1);
    delete_project(&db, owner, doomed, "DROP").await.unwrap();
    let survivors: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM projects WHERE owner_id=$1")
        .bind(owner)
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(survivors, vec![retained]);
    let notes: Vec<Uuid> = sqlx::query_scalar("SELECT project_id FROM notes WHERE owner_id=$1")
        .bind(owner)
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(notes, vec![retained]);
    let repos: Vec<i64> = sqlx::query_scalar("SELECT repo_id FROM github_repos WHERE owner_id=$1")
        .bind(owner)
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(repos, vec![272]);
    let preferences: Vec<(String,Uuid)> = sqlx::query_as("SELECT resource_type,resource_id FROM billing_preferences WHERE owner_id=$1 ORDER BY resource_type")
        .bind(owner).fetch_all(&db).await.unwrap();
    assert_eq!(
        preferences,
        vec![("project".into(), retained), ("repo".into(), retained_repo)]
    );
}

#[sqlx::test]
async fn overview_counts_use_actual_local_project_work(db: PgPool) {
    let owner = account(&db, "plus").await;
    let other = account(&db, "free").await;
    let project = create_project(&db, owner, "Main", "MAIN").await.unwrap();
    let sibling = create_project(&db, owner, "Other project", "SIDE")
        .await
        .unwrap();
    let foreign = create_project(&db, other, "Other workspace", "MAIN")
        .await
        .unwrap();
    for (user, id) in [(owner, project), (owner, sibling), (other, foreign)] {
        sqlx::query(
            "INSERT INTO issues(owner_id,project_id,number,title,status,archived)
          VALUES ($1,$2,1,'Queued','backlog',false),($1,$2,2,'Active','in_progress',false),
          ($1,$2,3,'Finished','done',false),($1,$2,4,'Abandoned','canceled',false),
          ($1,$2,5,'Hidden','ready',true),($1,$2,6,'Past','done',true)",
        )
        .bind(user)
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
        note(&db, user, id).await;
        sqlx::query("INSERT INTO milestones(owner_id,project_id,title) VALUES($1,$2,'Beta')")
            .bind(user)
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO releases(owner_id,project_id,name) VALUES($1,$2,'Release')")
            .bind(user)
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
    }
    assert!(matches!(
        counts(&db, owner, foreign).await,
        Err(crate::AppError::NotFound)
    ));
    let totals = counts(&db, owner, project).await.unwrap();
    assert_eq!(
        (
            totals.open_issues,
            totals.completed_issues,
            totals.notes,
            totals.milestones,
            totals.releases
        ),
        (2, 1, 1, 1, 1)
    );
    sqlx::query("UPDATE issues SET status='done' WHERE owner_id=$1 AND project_id=$2 AND number=2")
        .bind(owner)
        .bind(project)
        .execute(&db)
        .await
        .unwrap();
    let updated = counts(&db, owner, project).await.unwrap();
    assert_eq!((updated.open_issues, updated.completed_issues), (1, 2));
}

#[sqlx::test]
async fn project_http_forms_check_csrf_link_to_the_selected_project_and_export_archived_work(
    db: PgPool,
) {
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use sha2::{Digest, Sha256};
    use std::io::{Cursor, Read};
    use tower::ServiceExt;

    let owner = account(&db, "free").await;
    let outsider = account(&db, "free").await;
    let foreign = create_project(&db, outsider, "Outside workspace", "SIDE")
        .await
        .unwrap();
    let raw = hex::encode(rand::random::<[u8; 32]>());
    sqlx::query("INSERT INTO user_sessions(token_hash,user_id) VALUES($1,$2)")
        .bind(Sha256::digest(raw.as_bytes()).to_vec())
        .bind(owner)
        .execute(&db)
        .await
        .unwrap();
    let app = crate::app(crate::AppState::for_test(db.clone()));
    let request = |method: Method, uri: &str, form: String| {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, format!("workspace_session={raw}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        app.clone().oneshot(req)
    };
    let index = request(Method::GET, "/projects", String::new())
        .await
        .unwrap();
    assert_eq!(index.status(), StatusCode::OK);
    let index =
        String::from_utf8(to_bytes(index.into_body(), 256_000).await.unwrap().to_vec()).unwrap();
    assert!(!index.contains("Outside workspace"));
    let csrf = index
        .split("name=\"csrf\" value=\"")
        .nth(1)
        .expect("rendered CSRF field")
        .split('"')
        .next()
        .unwrap();
    let invalid_create = request(
        Method::POST,
        "/projects",
        "csrf=forged&name=Private&key=LOCAL".into(),
    )
    .await
    .unwrap();
    assert_eq!(invalid_create.status(), StatusCode::FORBIDDEN);
    let created = request(
        Method::POST,
        "/projects",
        format!("csrf={csrf}&name=Private&key=LOCAL&next=first-issue"),
    )
    .await
    .unwrap();
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let id: Uuid = sqlx::query_scalar("SELECT id FROM projects WHERE owner_id=$1 AND key='LOCAL'")
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        created.headers()[header::LOCATION].to_str().unwrap(),
        format!("/issues?project={id}#new-issue")
    );
    note(&db, owner, id).await;

    let archived = request(
        Method::POST,
        &format!("/projects/{id}/archive"),
        format!("csrf={csrf}"),
    )
    .await
    .unwrap();
    assert_eq!(archived.status(), StatusCode::SEE_OTHER);
    let overview_path = archived.headers()[header::LOCATION].to_str().unwrap();
    assert_eq!(overview_path, format!("/projects/{id}"));
    let overview = request(Method::GET, overview_path, String::new())
        .await
        .unwrap();
    assert_eq!(overview.status(), StatusCode::OK);
    let overview = String::from_utf8(
        to_bytes(overview.into_body(), 256_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(overview.contains(&format!("href=\"/issues?project={id}&amp;scope=archived\"")));
    for view in ["notes", "roadmap", "health", "releases"] {
        assert!(
            overview.contains(&format!("href=\"/{view}?project={id}\"")),
            "missing {view} project navigation"
        );
    }
    let export_path = format!("/transfer/export/{id}");
    assert!(overview.contains(&format!("href=\"{export_path}\"")));
    for forbidden in [
        format!("/projects/{foreign}"),
        format!("/transfer/export/{foreign}"),
    ] {
        assert_eq!(
            request(Method::GET, &forbidden, String::new())
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let export = request(Method::GET, &export_path, String::new())
        .await
        .unwrap();
    assert_eq!(export.status(), StatusCode::OK);
    assert_eq!(export.headers()[header::CONTENT_TYPE], "application/zip");
    let bytes = to_bytes(export.into_body(), 256_000).await.unwrap();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut exported_note = String::new();
    archive
        .by_name("Notes/Plan.md")
        .unwrap()
        .read_to_string(&mut exported_note)
        .unwrap();
    assert_eq!(exported_note, "Keep this text");
}

#[sqlx::test]
async fn key_rename_resolves_pending_note_links_and_keeps_bound_links(db: PgPool) {
    let owner = account(&db, "free").await;
    let project = create_project(&db, owner, "Notes project", "OLD")
        .await
        .unwrap();
    let issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues(owner_id,project_id,number,title) VALUES($1,$2,1,'First') RETURNING id",
    )
    .bind(owner)
    .bind(project)
    .fetch_one(&db)
    .await
    .unwrap();
    let source:Uuid = sqlx::query_scalar("INSERT INTO notes(owner_id,project_id,title,path,body) VALUES($1,$2,'References','References.md','See [[OLD-1]] or [[NEW-1]].') RETURNING id")
        .bind(owner).bind(project).fetch_one(&db).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    crate::notes::reindex(&mut tx, owner, project)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let links = || {
        sqlx::query_as::<_, (String, Option<Uuid>)>("SELECT target,dest_issue FROM note_links WHERE owner_id=$1 AND project_id=$2 AND source_id=$3 ORDER BY ordinal")
        .bind(owner).bind(project).bind(source).fetch_all(&db)
    };
    assert_eq!(
        links().await.unwrap(),
        vec![("OLD-1".into(), Some(issue)), ("NEW-1".into(), None)]
    );
    edit_project(&db, owner, project, "Notes project", "NEW")
        .await
        .unwrap();
    assert_eq!(
        links().await.unwrap(),
        vec![("OLD-1".into(), Some(issue)), ("NEW-1".into(), Some(issue))]
    );
}
