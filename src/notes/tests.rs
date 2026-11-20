use super::*;

async fn workspace(db: &PgPool) -> (Uuid, Uuid) {
    let owner = sqlx::query_scalar("INSERT INTO users(email) VALUES ($1) RETURNING id")
        .bind(format!("{}@example.test", Uuid::new_v4()))
        .fetch_one(db)
        .await
        .unwrap();
    let project = sqlx::query_scalar(
        "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Garden','GR') RETURNING id",
    )
    .bind(owner)
    .fetch_one(db)
    .await
    .unwrap();
    (owner, project)
}

fn draft(project: Uuid, title: &str, path: &str, body: &str) -> NoteForm {
    NoteForm {
        back: None,
        draft_token: None,
        project,
        title: title.into(),
        path: path.into(),
        body: body.into(),
        revision: None,
        csrf: String::new(),
        note_id: None,
    }
}

#[sqlx::test]
async fn revisions_protect_another_tabs_text_and_retain_raw_bytes(db: PgPool) {
    let (owner, project) = workspace(&db).await;
    let mut form = draft(
        project,
        "Decision",
        "research/decision.md",
        "\u{feff}---\r\nextra: a\r\n---\r\n\r\nKeep [[Plan]].\r\n",
    );
    let id = save(&db, owner, None, &form).await.unwrap();
    let stored = load_note(&db, owner, id).await.unwrap();
    assert_eq!(stored.body, form.body);
    form.revision = Some(1);
    form.body = "The first edit".into();
    save(&db, owner, Some(id), &form).await.unwrap();
    form.body = "The stale edit".into();
    assert!(matches!(
        save(&db, owner, Some(id), &form).await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(
        load_note(&db, owner, id).await.unwrap().body,
        "The first edit"
    );
}

#[sqlx::test]
async fn promotion_is_atomic_repeatable_and_scoped(db: PgPool) {
    let (owner, project) = workspace(&db).await;
    let (outsider, _) = workspace(&db).await;
    let mut form = draft(
        project,
        "Work",
        "work.md",
        "Review the caching decision. ^decision",
    );
    let note = save(&db, owner, None, &form).await.unwrap();
    let p = Document::parse(&form.body).paragraphs.remove(0);
    let mut request = IssueForm {
        csrf: String::new(),
        revision: 1,
        source_anchor: p.anchor.clone(),
        title: "Review cache".into(),
        command_id: Uuid::new_v4(),
    };
    assert!(matches!(
        promote(&db, outsider, note, &request).await,
        Err(AppError::NotFound)
    ));
    let issue = promote(&db, owner, note, &request).await.unwrap();
    assert_eq!(promote(&db, owner, note, &request).await.unwrap(), issue);
    let counts: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM issues),(SELECT count(*) FROM issue_events),(SELECT count(*) FROM note_issue_sources)").fetch_one(&db).await.unwrap();
    assert_eq!(counts, (1, 1, 1));
    form.revision = Some(1);
    form.body = "Entirely different.".into();
    save(&db, owner, Some(note), &form).await.unwrap();
    request.command_id = Uuid::new_v4();
    assert!(matches!(
        promote(&db, owner, note, &request).await,
        Err(AppError::Conflict(_))
    ));
    let excerpt: String =
        sqlx::query_scalar("SELECT excerpt FROM note_issue_sources WHERE issue_id=$1")
            .bind(issue)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(excerpt, p.excerpt);
}

#[sqlx::test]
async fn deletion_rechecks_owner_revision_and_confirmation_and_releases_storage(db: PgPool) {
    let (owner, project) = workspace(&db).await;
    let (other, _) = workspace(&db).await;
    let form = draft(
        project,
        "Private decision",
        "private.md",
        "Private paragraph ^private",
    );
    let id = save(&db, owner, None, &form).await.unwrap();
    let linked = save(
        &db,
        owner,
        None,
        &draft(project, "Index", "index.md", "[[private]]"),
    )
    .await
    .unwrap();
    let issue = promote(
        &db,
        owner,
        id,
        &IssueForm {
            csrf: String::new(),
            revision: 1,
            source_anchor: "block-private".into(),
            title: "An independent issue".into(),
            command_id: Uuid::new_v4(),
        },
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO transfer_records(owner_id,project_id,kind,source_id,local_id,base_hash,path) VALUES($1,$2,'note',$3,$3,'base','private.md')").bind(owner).bind(project).bind(id).execute(&db).await.unwrap();
    assert!(matches!(
        remove_note(&db, other, id, 1, "private.md").await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        remove_note(&db, owner, id, 1, "wrong.md").await,
        Err(AppError::BadRequest(_))
    ));
    assert!(matches!(
        remove_note(&db, owner, id, 2, "private.md").await,
        Err(AppError::Conflict(_))
    ));
    sqlx::query("UPDATE projects SET archived=true WHERE id=$1")
        .bind(project)
        .execute(&db)
        .await
        .unwrap();
    let before = store::storage_used(&db, owner).await.unwrap();
    assert_eq!(
        remove_note(&db, owner, id, 1, "private.md").await.unwrap(),
        project
    );
    assert!(matches!(
        load_note(&db, owner, id).await,
        Err(AppError::NotFound)
    ));
    assert!(load_note(&db, owner, linked).await.is_ok());
    let (issues,sources,records,links):(i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM issues WHERE id=$1),(SELECT count(*) FROM note_issue_sources),(SELECT count(*) FROM transfer_records),(SELECT count(*) FROM note_links WHERE source_id=$2 AND dest_note IS NULL)").bind(issue).bind(linked).fetch_one(&db).await.unwrap();
    assert_eq!((issues, sources, records, links), (1, 0, 0, 1));
    assert_eq!(
        before - store::storage_used(&db, owner).await.unwrap(),
        (form.title.len() + form.path.len() + form.body.len()) as i64
    );
}

#[sqlx::test]
async fn note_search_finds_late_text_and_scopes_every_result(db: PgPool) {
    let (me, project) = workspace(&db).await;
    let (other, foreign) = workspace(&db).await;
    let source = format!("{} marker at the tail", "design ".repeat(6000));
    let mine = save(
        &db,
        me,
        None,
        &draft(project, "Storage plan", "Decisions/Storage.md", &source),
    )
    .await
    .unwrap();
    save(
        &db,
        other,
        None,
        &draft(
            foreign,
            "Another storage plan",
            "Decisions/private.md",
            &source,
        ),
    )
    .await
    .unwrap();
    for term in ["marker at the tail", "decisions/storage", "storage plan"] {
        let rows = views::find_notes(&db, me, None, term, 0).await.unwrap();
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), vec![mine]);
    }
    assert!(
        views::find_notes(&db, me, Some(foreign), "Storage", 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn rendered_ambiguity_chooser_requires_a_scoped_matching_note(db: PgPool) {
    let (owner, project) = workspace(&db).await;
    let (other, other_project) = workspace(&db).await;
    let a = save(
        &db,
        owner,
        None,
        &draft(project, "First prospect", "alpha/Beta.md", "## Context"),
    )
    .await
    .unwrap();
    let b = save(
        &db,
        owner,
        None,
        &draft(project, "Second prospect", "beta/Beta.md", "## Context"),
    )
    .await
    .unwrap();
    let foreign = save(
        &db,
        other,
        None,
        &draft(
            other_project,
            "Foreign prospect",
            "private/Beta.md",
            "## Context",
        ),
    )
    .await
    .unwrap();
    let id = save(
        &db,
        owner,
        None,
        &draft(
            project,
            "Index",
            "index.md",
            "See [[Beta#Context|the selected decision]].",
        ),
    )
    .await
    .unwrap();
    let state = AppState::for_test(db.clone());
    let auth = Auth {
        user_id: owner,
        email: "person@example.test".into(),
        csrf: "a".repeat(64),
    };
    let before = views::detail(
        State(state.clone()),
        auth.clone(),
        Path(id),
        axum::extract::Query(Default::default()),
    )
    .await
    .unwrap()
    .0;
    assert!(
        before.contains("alpha/Beta.md")
            && before.contains("beta/Beta.md")
            && !before.contains("private/Beta.md")
    );
    let choose = |target, csrf: String| {
        choose_link(
            State(state.clone()),
            auth.clone(),
            Path((id, 0)),
            Form(Choice {
                csrf,
                revision: 1,
                target,
            }),
        )
    };
    assert!(matches!(
        choose(a, "b".repeat(64)).await,
        Err(AppError::Forbidden)
    ));
    assert!(matches!(
        choose(foreign, auth.csrf.clone()).await,
        Err(AppError::BadRequest(_))
    ));
    let chosen = choose(b, auth.csrf.clone()).await.unwrap().into_response();
    assert_eq!(chosen.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(matches!(
        choose(a, auth.csrf.clone()).await,
        Err(AppError::Conflict(_))
    ));
    let after = views::detail(
        State(state),
        auth,
        Path(id),
        axum::extract::Query(Default::default()),
    )
    .await
    .unwrap()
    .0;
    assert!(after.contains(&format!("href=\"/notes/{b}#heading-context\"")));
    assert!(!after.contains("Choose a destination"));
}

#[sqlx::test]
async fn autocomplete_searches_beyond_the_initial_page_without_leaking_other_workspaces(
    db: PgPool,
) {
    let (owner, project) = workspace(&db).await;
    let (other, foreign) = workspace(&db).await;
    sqlx::query("INSERT INTO issues(owner_id,project_id,number,title) SELECT $1,$2,n,'Alpha ' || lpad(n::text,3,'0') FROM generate_series(1,305) n")
        .bind(owner).bind(project).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO issues(owner_id,project_id,number,title) VALUES($1,$2,305,'Foreign exact match')")
        .bind(other).bind(foreign).execute(&db).await.unwrap();
    let project = store::project(&db, owner, project).await.unwrap();
    let initial = views::link_suggestions(&db, owner, &project, "")
        .await
        .unwrap();
    assert!(!initial.iter().any(|row| row.token == "[[GR-305]]"));
    let exact = views::link_suggestions(&db, owner, &project, "GR-305")
        .await
        .unwrap();
    assert_eq!(
        exact.iter().map(|r| &r.token).collect::<Vec<_>>(),
        vec!["[[GR-305]]"]
    );
    assert!(
        views::link_suggestions(&db, owner, &project, "Foreign exact")
            .await
            .unwrap()
            .is_empty()
    );
}
