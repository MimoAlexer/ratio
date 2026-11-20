//! HTTP acceptance tests. SQLx provisions a migrated database for each test.
use crate::AppState;
use anyhow::{Context, Result};
use reqwest::{Client, Response, StatusCode, header::COOKIE, redirect::Policy};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

struct Tenant {
    owner: Uuid,
    project: Uuid,
    cookie: String,
    csrf: String,
}

impl Tenant {
    async fn seed(app: &TestApp) -> Result<Self> {
        let db = &app.db;
        let owner = Uuid::new_v4();
        let project = Uuid::new_v4();
        let raw = hex::encode([*Uuid::new_v4().as_bytes(), *Uuid::new_v4().as_bytes()].concat());
        let mut tx = db.begin().await?;
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(owner)
            .bind(format!("{owner}@example.invalid"))
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO projects(id,owner_id,key,name) VALUES($1,$2,'DEV','Development')")
            .bind(project)
            .bind(owner)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO user_sessions(user_id,token_hash) VALUES($1,$2)")
            .bind(owner)
            .bind(Sha256::digest(raw.as_bytes()).to_vec())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut tenant = Self {
            owner,
            project,
            cookie: format!("workspace_session={raw}"),
            csrf: String::new(),
        };
        let page = app.read(&tenant, "/issues").await?;
        let csrf = page
            .split("<input ")
            .filter_map(|input| input.split_once('>'))
            .map(|(attributes, _)| attributes)
            .find(|attributes| attributes.contains("name=\"csrf\""))
            .and_then(|attributes| attributes.split_once("value=\""))
            .and_then(|(_, remaining)| remaining.split_once('"'))
            .map(|(value, _)| value)
            .context("expected a session CSRF token in a rendered issue form")?;
        tenant.csrf = csrf.to_owned();
        Ok(tenant)
    }
}

struct TestApp {
    db: PgPool,
    client: Client,
    url: String,
    server: tokio::task::JoinHandle<()>,
}

impl TestApp {
    async fn listen(db: PgPool) -> Result<Self> {
        let client = Client::builder().redirect(Policy::none()).build()?;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let url = format!("http://{}", listener.local_addr()?);
        let app = crate::app(AppState::for_test(db.clone()));
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .expect("serve test routes");
        });
        Ok(Self {
            db,
            client,
            url,
            server,
        })
    }

    async fn get(&self, user: &Tenant, path: &str) -> Result<Response> {
        Ok(self
            .client
            .get(format!("{}{path}", self.url))
            .header(COOKIE, &user.cookie)
            .send()
            .await?)
    }

    async fn read(&self, user: &Tenant, path: &str) -> Result<String> {
        let response = self.get(user, path).await?;
        assert_eq!(response.status(), StatusCode::OK, "GET {path}");
        Ok(response.text().await?)
    }

    async fn post(
        &self,
        user: &Tenant,
        path: &str,
        command: Uuid,
        revision: Option<i64>,
        fields: &[(&str, &str)],
    ) -> Result<StatusCode> {
        let mut form = vec![
            ("csrf", user.csrf.clone()),
            ("command_id", command.to_string()),
        ];
        if let Some(revision) = revision {
            form.push(("revision", revision.to_string()));
        }
        form.extend(
            fields
                .iter()
                .map(|(key, value)| (*key, (*value).to_owned())),
        );
        let response = self
            .client
            .post(format!("{}{path}", self.url))
            .header(COOKIE, &user.cookie)
            .form(&form)
            .send()
            .await?;
        Ok(response.status())
    }

    async fn revision(&self, user: &Tenant, id: Uuid) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT revision FROM issues WHERE owner_id=$1 AND id=$2")
                .bind(user.owner)
                .bind(id)
                .fetch_one(&self.db)
                .await?,
        )
    }

    async fn change(
        &self,
        user: &Tenant,
        id: Uuid,
        action: &str,
        fields: &[(&str, &str)],
    ) -> Result<StatusCode> {
        self.post(
            user,
            &format!("/issues/{id}/{action}"),
            Uuid::new_v4(),
            Some(self.revision(user, id).await?),
            fields,
        )
        .await
    }

    async fn create(&self, user: &Tenant, title: &str) -> Result<Uuid> {
        let project = user.project.to_string();
        let fields = [
            ("project", project.as_str()),
            ("title", title),
            ("body", "Reproduce the observed issue"),
            ("status", "in_progress"),
            ("priority", "high"),
            ("kind", "bug"),
            ("labels", "server, import"),
            ("next_action", ""),
            ("severity", "none"),
        ];
        let command = Uuid::new_v4();
        let (one, two) = tokio::try_join!(
            self.post(user, "/issues", command, None, &fields),
            self.post(user, "/issues", command, None, &fields),
        )?;
        assert_eq!(
            (one, two),
            (StatusCode::SEE_OTHER, StatusCode::SEE_OTHER),
            "retry create {title}"
        );
        let rows: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM issues WHERE owner_id=$1 AND title=$2")
                .bind(user.owner)
                .bind(title)
                .fetch_all(&self.db)
                .await?;
        assert_eq!(rows.len(), 1, "one issue per command");
        let events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM issue_events WHERE owner_id=$1 AND command_id=$2",
        )
        .bind(user.owner)
        .bind(command)
        .fetch_one(&self.db)
        .await?;
        assert_eq!(events, 1);
        Ok(rows[0])
    }

    async fn resume(&self, user: &Tenant) -> Result<String> {
        let page = self.read(user, "/").await?;
        Ok(page
            .split_once("id=\"resume\"")
            .context("Home resume section")?
            .1
            .split_once("</section>")
            .context("end of resume section")?
            .0
            .to_owned())
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[sqlx::test]
async fn invalid_issue_edit_retains_submitted_text_and_can_be_corrected(db: PgPool) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let owner = Tenant::seed(&app).await?;
    let issue = app.create(&owner, "Saved issue").await?;
    let path = format!("/issues/{issue}/edit");
    let revision = app.revision(&owner, issue).await?;
    let invalid = app
        .client
        .post(format!("{}{path}", app.url))
        .header(COOKIE, &owner.cookie)
        .form(&[
            ("csrf", owner.csrf.clone()),
            ("command_id", Uuid::new_v4().to_string()),
            ("revision", revision.to_string()),
            ("title", "   ".into()),
            ("body", "\nDraft <body>".into()),
            ("labels", "one, two  ,three".into()),
            ("priority", "urgent".into()),
        ])
        .send()
        .await?;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let html = invalid.text().await?;
    let form = html
        .split_once(&format!("action=\"{path}\""))
        .context("correctable edit form")?
        .1;
    assert!(form.contains("Enter an issue title between 1 and 240 characters."));
    assert!(form.contains("aria-invalid=\"true\""));
    assert!(form.contains("value=\"   \"") && form.contains("\n\nDraft &lt;body&gt;"));
    assert!(
        form.contains("value=\"one, two  ,three\"") && form.contains("value=\"urgent\" selected")
    );
    assert_eq!(app.revision(&owner, issue).await?, revision);
    let retry = form
        .split_once("name=\"command_id\" value=\"")
        .context("retry command")?
        .1
        .split('"')
        .next()
        .context("retry command value")?
        .parse()?;
    let corrected = [
        ("title", "Corrected issue"),
        ("body", "\nDraft <body>"),
        ("labels", "one, two  ,three"),
        ("priority", "urgent"),
    ];
    assert_eq!(
        app.post(&owner, &path, retry, Some(revision), &corrected)
            .await?,
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.revision(&owner, issue).await?, revision + 1);
    let row: (String, String, String, Vec<String>) =
        sqlx::query_as("SELECT title,body,priority,labels FROM issues WHERE owner_id=$1 AND id=$2")
            .bind(owner.owner)
            .bind(issue)
            .fetch_one(&app.db)
            .await?;
    assert_eq!(
        row,
        (
            "Corrected issue".into(),
            "\nDraft <body>".into(),
            "urgent".into(),
            vec!["one".into(), "two".into(), "three".into()]
        )
    );
    Ok(())
}

#[sqlx::test]
async fn resume_order_is_explicit_and_finishing_or_archiving_reveals_previous_work(
    db: PgPool,
) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let owner = Tenant::seed(&app).await?;
    let first = app.create(&owner, "Inspect decoder overflow").await?;
    let second = app.create(&owner, "Exercise archive fixture").await?;
    assert!(
        app.resume(&owner).await?.contains(&second.to_string()),
        "most recent active edit"
    );
    for (id, next) in [
        (first, "Collect the saved trace"),
        (second, "Compare both compressed blobs"),
    ] {
        assert_eq!(
            app.change(&owner, id, "checkpoint", &[("next_action", next)])
                .await?,
            StatusCode::SEE_OTHER
        );
    }
    let _ = app.read(&owner, &format!("/issues/{first}")).await?;
    let resume = app.resume(&owner).await?;
    assert!(
        resume.contains("Compare both compressed blobs"),
        "a passive visit must preserve checkpoint order"
    );
    assert_eq!(
        app.change(&owner, second, "archive", &[("archived", "true")])
            .await?,
        StatusCode::SEE_OTHER
    );
    assert!(
        app.resume(&owner)
            .await?
            .contains("Collect the saved trace")
    );
    assert_eq!(
        app.change(&owner, second, "undo", &[]).await?,
        StatusCode::SEE_OTHER
    );
    assert!(
        app.resume(&owner)
            .await?
            .contains("Compare both compressed blobs")
    );
    let path = format!("/issues/{second}/status");
    let revision = app.revision(&owner, second).await?;
    let command = Uuid::new_v4();
    for _ in 0..2 {
        assert_eq!(
            app.post(
                &owner,
                &path,
                command,
                Some(revision),
                &[("status", "done")]
            )
            .await?,
            StatusCode::SEE_OTHER
        );
    }
    assert_eq!(app.revision(&owner, second).await?, revision + 1);
    assert_eq!(
        app.post(
            &owner,
            &path,
            Uuid::new_v4(),
            Some(revision),
            &[("status", "ready")]
        )
        .await?,
        StatusCode::CONFLICT
    );
    assert!(
        app.resume(&owner)
            .await?
            .contains("Collect the saved trace")
    );
    let completed: bool = sqlx::query_scalar(
        "SELECT started_at IS NOT NULL AND completed_at IS NOT NULL FROM issues WHERE id=$1",
    )
    .bind(second)
    .fetch_one(&app.db)
    .await?;
    assert!(completed);
    assert_eq!(
        app.change(&owner, second, "undo", &[]).await?,
        StatusCode::SEE_OTHER
    );
    let restored: bool = sqlx::query_scalar("SELECT status='in_progress' AND started_at IS NOT NULL AND completed_at IS NULL FROM issues WHERE id=$1")
        .bind(second).fetch_one(&app.db).await?;
    assert!(restored);
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM issue_events WHERE command_id=$1 AND owner_id=$2")
            .bind(command)
            .bind(owner.owner)
            .fetch_one(&app.db)
            .await?;
    assert_eq!(events, 1, "undo preserves the original command in history");
    Ok(())
}

#[sqlx::test]
async fn tenant_reads_writes_and_blocker_cycles_are_scoped(db: PgPool) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let owner = Tenant::seed(&app).await?;
    let outsider = Tenant::seed(&app).await?;
    let a = app.create(&owner, "CascadeParser first edge").await?;
    let b = app.create(&owner, "CascadeParser second edge").await?;
    let c = app.create(&owner, "CascadeParser third edge").await?;
    let private = app
        .create(&outsider, "CascadeParser outsider secret")
        .await?;
    for path in [
        "/".to_owned(),
        "/issues?q=CascadeParser".to_owned(),
        "/issues?q=CascadeParser&view=board".to_owned(),
    ] {
        let page = app.read(&owner, &path).await?;
        assert!(!page.contains("outsider secret"), "foreign title on {path}");
        assert!(page.contains("CascadeParser"), "own work missing on {path}");
    }
    assert_eq!(
        app.get(&owner, &format!("/issues/{private}"))
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get(&owner, &format!("/issues?project={}", outsider.project))
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    let status = app
        .post(
            &owner,
            &format!("/issues/{private}/status"),
            Uuid::new_v4(),
            Some(app.revision(&outsider, private).await?),
            &[("status", "done")],
        )
        .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for (issue, blocker) in [(a, b), (b, c)] {
        assert_eq!(
            app.change(
                &owner,
                issue,
                "blocker",
                &[("blocker_id", &blocker.to_string()), ("operation", "add")]
            )
            .await?,
            StatusCode::SEE_OTHER
        );
    }
    let cycle = app
        .change(
            &owner,
            c,
            "blocker",
            &[("blocker_id", &a.to_string()), ("operation", "add")],
        )
        .await?;
    assert!(
        matches!(cycle, StatusCode::CONFLICT | StatusCode::BAD_REQUEST),
        "{cycle}"
    );
    let foreign = app
        .change(
            &owner,
            c,
            "blocker",
            &[("blocker_id", &private.to_string()), ("operation", "add")],
        )
        .await?;
    assert!(
        matches!(foreign, StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST),
        "{foreign}"
    );
    assert_eq!(
        app.change(&owner, b, "undo", &[]).await?,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        app.change(
            &owner,
            c,
            "blocker",
            &[("blocker_id", &a.to_string()), ("operation", "add")]
        )
        .await?,
        StatusCode::SEE_OTHER
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_blockers WHERE owner_id=$1")
        .bind(owner.owner)
        .fetch_one(&app.db)
        .await?;
    assert_eq!(count, 2);
    Ok(())
}

#[sqlx::test]
async fn manual_critical_beside_checkpoint_can_close_independently_and_respects_project_filter(
    db: PgPool,
) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let mut owner = Tenant::seed(&app).await?;
    let first_project = owner.project;
    let focus = app.create(&owner, "Checksum verification").await?;
    assert_eq!(
        app.change(
            &owner,
            focus,
            "checkpoint",
            &[("next_action", "Replay the local trace")]
        )
        .await?,
        StatusCode::SEE_OTHER
    );

    sqlx::query("UPDATE users SET plan='plus' WHERE id=$1")
        .bind(owner.owner)
        .execute(&app.db)
        .await?;
    owner.project = sqlx::query_scalar(
        "INSERT INTO projects(owner_id,key,name) VALUES($1,'DEP','Dependencies') RETURNING id",
    )
    .bind(owner.owner)
    .fetch_one(&app.db)
    .await?;
    let finding = app.create(&owner, "Inspect package lockfile").await?;
    let fields = [
        ("title", "Manual package finding"),
        ("body", "Review the lockfile entry"),
        ("status", "done"),
        ("priority", "urgent"),
        ("kind", "security"),
        ("labels", "audit"),
        ("next_action", "Check the published fix"),
        ("severity", "critical"),
    ];
    assert_eq!(
        app.change(&owner, finding, "edit", &fields).await?,
        StatusCode::SEE_OTHER
    );
    let home = app.read(&owner, "/").await?;
    assert!(home.contains("Manual package finding") && home.contains("Replay the local trace"));
    let scoped = app
        .read(&owner, &format!("/?project={first_project}"))
        .await?;
    assert!(!scoped.contains("Manual package finding"));
    assert!(scoped.contains("Replay the local trace"));

    let wrong_csrf = Tenant {
        owner: owner.owner,
        project: owner.project,
        cookie: owner.cookie.clone(),
        csrf: "wrong".into(),
    };
    let revision = app.revision(&owner, finding).await?;
    assert_eq!(
        app.change(&wrong_csrf, finding, "status", &[("status", "canceled")])
            .await?,
        StatusCode::FORBIDDEN
    );
    assert_eq!(app.revision(&owner, finding).await?, revision);
    let close_path = format!("/issues/{finding}/edit");
    let close_form = home
        .split_once(&format!("action=\"{close_path}\""))
        .and_then(|(_, rest)| rest.split_once("</form>"))
        .map(|(fields, _)| fields)
        .context("close manual finding from Home")?;
    assert!(close_form.contains("name=\"finding_closed\" value=\"true\""));
    assert_eq!(
        app.change(&owner, finding, "edit", &[("finding_closed", "true")])
            .await?,
        StatusCode::SEE_OTHER
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM issues WHERE owner_id=$1 AND id=$2")
            .bind(owner.owner)
            .bind(finding)
            .fetch_one(&app.db)
            .await?;
    assert_eq!(status, "done");
    assert!(
        !app.read(&owner, "/")
            .await?
            .contains("Manual package finding")
    );
    assert_eq!(
        app.change(&owner, finding, "edit", &fields).await?,
        StatusCode::SEE_OTHER
    );
    assert!(
        app.read(&owner, "/")
            .await?
            .contains("Manual package finding")
    );
    Ok(())
}

#[sqlx::test]
async fn large_descriptions_remain_importable_and_searchable_past_the_indexed_prefix(
    db: PgPool,
) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let owner = Tenant::seed(&app).await?;
    let title = "Large compiler diagnostic dump";
    let bytes: i32 = sqlx::query_scalar(
        "INSERT INTO issues(owner_id,project_id,number,title,body)
         SELECT $1,$2,1,$3,string_agg(md5(n::text), ' ' ORDER BY n) || ' tail marker'
         FROM generate_series(1,45000) AS n
         RETURNING octet_length(body)",
    )
    .bind(owner.owner)
    .bind(owner.project)
    .bind(title)
    .fetch_one(&app.db)
    .await?;
    assert!((1_048_577..=2_097_152).contains(&bytes));
    assert!(
        app.read(&owner, "/issues?q=tail%20marker")
            .await?
            .contains(title)
    );
    assert!(
        !app.read(&owner, "/issues?q=absent%20marker")
            .await?
            .contains(title)
    );
    Ok(())
}

#[sqlx::test]
async fn a_next_action_entered_on_create_becomes_one_explicit_checkpoint(db: PgPool) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let owner = Tenant::seed(&app).await?;
    let project = owner.project.to_string();
    let first_title = "Reindex package docs";
    let second_title = "Triage another report";
    let fields = [
        ("project", project.as_str()),
        ("title", first_title),
        ("status", "ready"),
        ("next_action", "Compare the two rebuilt shards"),
    ];
    let command = Uuid::new_v4();
    assert_eq!(
        app.post(&owner, "/issues", command, None, &fields).await?,
        StatusCode::SEE_OTHER
    );
    let (id, initial): (Uuid, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT id,checkpoint_at FROM issues WHERE owner_id=$1 AND title=$2")
            .bind(owner.owner)
            .bind(first_title)
            .fetch_one(&app.db)
            .await?;
    assert!(
        app.resume(&owner)
            .await?
            .contains("Compare the two rebuilt shards")
    );
    assert_eq!(
        app.post(&owner, "/issues", command, None, &fields).await?,
        StatusCode::SEE_OTHER
    );
    let after_retry: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT checkpoint_at FROM issues WHERE id=$1")
            .bind(id)
            .fetch_one(&app.db)
            .await?;
    assert_eq!(after_retry, initial);
    let untimed = [
        ("project", project.as_str()),
        ("title", second_title),
        ("status", "ready"),
        ("next_action", ""),
    ];
    assert_eq!(
        app.post(&owner, "/issues", Uuid::new_v4(), None, &untimed)
            .await?,
        StatusCode::SEE_OTHER
    );
    let other_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM issues WHERE owner_id=$1 AND title=$2 AND checkpoint_at IS NULL",
    )
    .bind(owner.owner)
    .bind(second_title)
    .fetch_one(&app.db)
    .await?;
    let _ = app.read(&owner, &format!("/issues/{other_id}")).await?;
    assert!(app.resume(&owner).await?.contains(&id.to_string()));
    Ok(())
}

#[sqlx::test]
async fn grouped_issue_pages_sort_across_results_and_keep_board_lanes_visible(
    db: PgPool,
) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let tenant = Tenant::seed(&app).await?;
    let inbox = app.read(&tenant, "/issues?status=backlog").await?;
    assert!(inbox.contains("<h1>Inbox</h1>") && inbox.contains("Inbox is clear"));
    let creation = inbox.split_once("id=\"new-issue\"").unwrap().1;
    assert!(creation.contains("value=\"backlog\" selected"));
    sqlx::query(
        "INSERT INTO issues(owner_id,project_id,number,title,status,priority)
         SELECT $1,$2,n,'Work item ' || lpad(n::text,2,'0'),
             CASE n WHEN 52 THEN 'in_progress' WHEN 53 THEN 'backlog' ELSE 'ready' END,
             CASE n WHEN 53 THEN 'urgent' ELSE 'normal' END
         FROM generate_series(1,53) n",
    )
    .bind(tenant.owner)
    .bind(tenant.project)
    .execute(&app.db)
    .await?;

    let grouped = app.read(&tenant, "/issues?group=status&sort=title").await?;
    assert!(grouped.find("Work item 52").unwrap() < grouped.find("Work item 01").unwrap());
    assert!(!grouped.contains("Work item 53"));
    let second = app
        .read(&tenant, "/issues?group=status&sort=title&p=2")
        .await?;
    assert!(second.find("Work item 50").unwrap() < second.find("Work item 53").unwrap());
    assert!(!second.contains("Work item 49"));
    let flat = app
        .read(&tenant, "/issues?group=none&sort=priority")
        .await?;
    assert!(
        flat.split_once("class=\"issue-title\">")
            .unwrap()
            .1
            .starts_with("Work item 53</span>")
    );
    assert!(flat.contains("data-group=\"none\""));
    let board = app
        .read(&tenant, "/issues?view=board&group=status&sort=title")
        .await?;
    for lane in ["Work item 01", "Work item 52", "Work item 53"] {
        assert!(board.contains(lane));
    }
    assert!(board.contains("name=\"sort\" value=\"title\""));
    assert_eq!(
        app.get(&tenant, "/issues?sort=invalid").await?.status(),
        StatusCode::BAD_REQUEST
    );
    Ok(())
}

fn issue_preview(page: &str) -> Option<&str> {
    page.split_once("id=\"issue-preview\"")
        .and_then(|(_, after)| after.split_once("</aside>"))
        .map(|(preview, _)| preview)
}

#[sqlx::test]
async fn workbench_inspects_only_the_current_result_page_and_preserves_its_query(
    db: PgPool,
) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let owner = Tenant::seed(&app).await?;
    // The latest checkpoint is on page two, and a completed issue has a newer stamp.
    sqlx::query(
        "INSERT INTO issues(owner_id,project_id,number,title,status,next_action,checkpoint_at)
         SELECT $1,$2,n,'Audit ' || lpad(n::text,2,'0'),
                CASE WHEN n=3 THEN 'done' ELSE 'ready' END,
                'Verify result ' || n,
                CASE WHEN n=3 THEN now() + interval '1 day'
                     WHEN n IN (2,49,52) THEN now() + n * interval '1 second' END
           FROM generate_series(1,52) n",
    )
    .bind(owner.owner)
    .bind(owner.project)
    .execute(&app.db)
    .await?;
    let query = format!("project={}&q=Audit&group=none&sort=title", owner.project);
    let first = app.read(&owner, &format!("/issues?{query}")).await?;
    assert!(issue_preview(&first).is_none());
    let second = app.read(&owner, &format!("/issues?{query}&p=2")).await?;
    assert!(issue_preview(&second).is_none());
    // The queue indicates the latest open checkpoint on each page without opening it.
    for (page, title) in [(&first, "Audit 49"), (&second, "Audit 52")] {
        let marked_row = page
            .split("<li ")
            .find(|row| row.contains("title=\"Latest checkpoint\""));
        assert!(marked_row.is_some_and(|row| row.contains(title)));
    }

    let id: Uuid = sqlx::query_scalar("SELECT id FROM issues WHERE owner_id=$1 AND number=51")
        .bind(owner.owner)
        .fetch_one(&app.db)
        .await?;
    let base = reqwest::Url::parse(&app.url)?;
    let link = second
        .split("href=\"")
        .filter_map(|s| s.split_once('"'))
        .map(|(href, _)| href.replace("&amp;", "&"))
        .filter_map(|href| base.join(&href).ok())
        .find(|href| {
            href.query_pairs()
                .any(|(k, v)| k == "inspect" && v == id.to_string())
        })
        .context("native preview link for a visible issue")?;
    let params: std::collections::HashMap<_, _> = link.query_pairs().collect();
    for (key, expected) in [
        ("q", "Audit"),
        ("group", "none"),
        ("sort", "title"),
        ("p", "2"),
    ] {
        assert_eq!(params.get(key).map(|v| v.as_ref()), Some(expected));
    }
    assert_eq!(
        params.get("project").map(|v| v.as_ref()),
        Some(owner.project.to_string().as_str())
    );
    let chosen = app
        .read(&owner, &format!("/issues?{}", link.query().unwrap()))
        .await?;
    let preview = issue_preview(&chosen).context("selected issue")?;
    assert!(preview.contains("Audit 51") && preview.contains(&format!("href=\"/issues/{id}\"")));
    let off_page = app
        .read(&owner, &format!("/issues?{query}&inspect={id}"))
        .await?;
    assert!(!off_page.contains("Audit 51") && !off_page.contains("Verify result 51"));
    let collapsed = app
        .read(&owner, &format!("/issues?{query}&p=2&inspect=none"))
        .await?;
    assert!(issue_preview(&collapsed).is_none());
    assert!(collapsed.contains(&format!("href=\"/issues/{id}\"")));
    Ok(())
}

#[sqlx::test]
async fn workbench_scopes_the_preview_and_keeps_native_board_commands(db: PgPool) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let owner = Tenant::seed(&app).await?;
    let other = Tenant::seed(&app).await?;
    let own = app.create(&owner, "Visible release fix").await?;
    let private = app.create(&other, "Unpublished diagnostics report").await?;
    let source: Uuid = sqlx::query_scalar(
        "INSERT INTO notes(owner_id,project_id,title,path,body)
         VALUES($1,$2,'Release notes','release.md','Check the signing key') RETURNING id",
    )
    .bind(owner.owner)
    .bind(owner.project)
    .fetch_one(&app.db)
    .await?;
    sqlx::query(
        "INSERT INTO note_issue_sources(owner_id,project_id,note_id,issue_id,source_anchor,excerpt,note_revision)
         VALUES($1,$2,$3,$4,'block-signing','Check the signing key',1)",
    )
    .bind(owner.owner)
    .bind(owner.project)
    .bind(source)
    .bind(own)
    .execute(&app.db)
    .await?;
    let other_project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects(owner_id,key,name) VALUES($1,'DOC','Documentation') RETURNING id",
    )
    .bind(owner.owner)
    .fetch_one(&app.db)
    .await?;
    let hidden: Uuid = sqlx::query_scalar(
        "INSERT INTO issues(owner_id,project_id,number,title,next_action)
         VALUES($1,$2,1,'External review','Distribute the private proof') RETURNING id",
    )
    .bind(owner.owner)
    .bind(other_project)
    .fetch_one(&app.db)
    .await?;
    let scoped = format!("/issues?project={}", owner.project);
    let first = app.read(&owner, &scoped).await?;
    assert!(issue_preview(&first).is_none());
    let chosen = app.read(&owner, &format!("{scoped}&inspect={own}")).await?;
    let preview = issue_preview(&chosen).context("explicit preview of a visible issue")?;
    assert!(preview.contains("Visible release fix"));
    assert!(preview.contains("Issue created"));
    assert!(preview.contains(&format!("href=\"/notes/{source}#block-signing\"")));
    for (id, restricted) in [
        (private, "Unpublished diagnostics report"),
        (hidden, "Distribute the private proof"),
    ] {
        let page = app.read(&owner, &format!("{scoped}&inspect={id}")).await?;
        assert!(!page.contains(restricted));
    }
    let board = app
        .read(&owner, &format!("{scoped}&view=board&inspect={own}"))
        .await?;
    assert!(issue_preview(&board).is_none());
    let action = format!("/issues/{own}/status");
    let form = board
        .split("<form ")
        .filter_map(|s| s.split_once("</form>"))
        .map(|(form, _)| form)
        .find(|form| form.contains(&format!("action=\"{action}\"")))
        .context("native status form on the board")?;
    assert!(form.contains("method=\"post\""));
    assert!(form.contains("name=\"status\""));
    let input = |name: &str| -> Result<&str> {
        form.split("<input ")
            .filter_map(|s| s.split_once('>'))
            .map(|(attrs, _)| attrs)
            .find(|attrs| attrs.contains(&format!("name=\"{name}\"")))
            .and_then(|attrs| attrs.split_once("value=\""))
            .and_then(|(_, tail)| tail.split_once('"'))
            .map(|(value, _)| value)
            .with_context(|| format!("missing {name} in board status form"))
    };
    let response = app
        .client
        .post(format!("{}{action}", app.url))
        .header(COOKIE, &owner.cookie)
        .form(&[
            ("csrf", input("csrf")?),
            ("command_id", input("command_id")?),
            ("revision", input("revision")?),
            ("status", "done"),
        ])
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let done = app
        .read(&owner, &format!("{scoped}&view=board&status=done"))
        .await?;
    assert!(done.contains("Visible release fix"));
    app.create(&owner, "Prepare the next release").await?;
    let done_group_open = |html: &str| {
        html.split("<details ")
            .filter_map(|s| s.split_once('>'))
            .find(|(attrs, _)| attrs.contains("data-group=\"done\""))
            .map(|(attrs, _)| attrs.split_ascii_whitespace().any(|attr| attr == "open"))
    };
    assert_eq!(
        done_group_open(&app.read(&owner, &scoped).await?),
        Some(false)
    );
    for query in ["q=Visible".to_owned(), format!("inspect={own}")] {
        let focused = app.read(&owner, &format!("{scoped}&{query}")).await?;
        assert_eq!(done_group_open(&focused), Some(true));
    }
    sqlx::query("UPDATE issues SET manual_severity='critical', security_critical=true WHERE owner_id=$1 AND id=$2")
        .bind(owner.owner).bind(own).execute(&app.db).await?;
    let unresolved = app.read(&owner, &scoped).await?;
    assert_eq!(done_group_open(&unresolved), Some(true));
    assert!(unresolved.contains("Critical finding"));
    Ok(())
}

#[sqlx::test]
async fn home_recommends_the_highest_priority_unblocked_ready_issue(db: PgPool) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let owner = Tenant::seed(&app).await?;
    sqlx::query(
        "INSERT INTO issues(owner_id,project_id,number,title,status,priority) VALUES
         ($1,$2,1,'Waiting for schema','ready','urgent'),($1,$2,2,'Ship parser','ready','high'),
         ($1,$2,3,'Trim log level','ready','normal'),($1,$2,4,'Prepare schema','backlog','normal')",
    )
    .bind(owner.owner)
    .bind(owner.project)
    .execute(&app.db)
    .await?;
    sqlx::query(
        "INSERT INTO issue_blockers(owner_id,project_id,issue_id,blocker_id)
         SELECT i.owner_id,i.project_id,i.id,b.id FROM issues i JOIN issues b USING(owner_id,project_id)
          WHERE i.owner_id=$1 AND i.number=1 AND b.number=4",
    )
    .bind(owner.owner)
    .execute(&app.db)
    .await?;
    let resume = app.resume(&owner).await?;
    assert!(resume.contains("Next ready issue") && resume.contains("Ship parser"));
    assert!(!resume.contains("Waiting for schema"));
    sqlx::query("UPDATE issues SET status='done' WHERE owner_id=$1 AND number=4")
        .bind(owner.owner)
        .execute(&app.db)
        .await?;
    assert!(app.resume(&owner).await?.contains("Waiting for schema"));
    Ok(())
}

#[sqlx::test]
async fn default_list_pages_have_stable_ties_and_count_only_visible_issues(
    db: PgPool,
) -> Result<()> {
    let app = TestApp::listen(db).await?;
    let owner = Tenant::seed(&app).await?;
    let outsider = Tenant::seed(&app).await?;
    let retired: Uuid = sqlx::query_scalar(
        "INSERT INTO projects(owner_id,key,name,archived) VALUES($1,'OLD','Retired',true) RETURNING id",
    )
    .bind(owner.owner)
    .fetch_one(&app.db)
    .await?;
    // Reverse insertion makes UUID order distinct from storage order at the page boundary.
    sqlx::query(
        "INSERT INTO issues(id,owner_id,project_id,number,title,status,updated_at,checkpoint_at)
         SELECT lpad(to_hex(n),32,'0')::uuid,$1,$2,n,'Visible '||n,
                CASE WHEN n>50 THEN 'in_progress' ELSE 'ready' END,
                now(),CASE WHEN n=20 THEN now() END FROM generate_series(52,1,-1) n",
    )
    .bind(owner.owner)
    .bind(owner.project)
    .execute(&app.db)
    .await?;
    sqlx::query(
        "INSERT INTO issues(owner_id,project_id,number,title,archived,checkpoint_at) VALUES
         ($1,$2,53,'Unshipped decoder adjustment',true,now()),($1,$3,1,'Retired renderer cleanup',false,now()),
         ($4,$5,1,'Private keyboard report',false,now())",
    )
    .bind(owner.owner)
    .bind(owner.project)
    .bind(retired)
    .bind(outsider.owner)
    .bind(outsider.project)
    .execute(&app.db)
    .await?;

    let expected: Vec<_> = (51..=52)
        .chain(1..=50)
        .map(|n| format!("Visible {n}"))
        .collect();
    for (path, titles) in [
        ("/issues", &expected[..50]),
        ("/issues?p=2", &expected[50..]),
    ] {
        let page = app.read(&owner, path).await?;
        assert!(
            page.contains("title=\"Matching issues\">52</span>"),
            "{path}"
        );
        let rows: Vec<_> = page
            .split("class=\"issue-title\">")
            .skip(1)
            .filter_map(|s| s.split_once("</span>").map(|(title, _)| title))
            .collect();
        assert_eq!(rows, titles, "{path}");
        assert!(issue_preview(&page).is_none(), "{path}");
        for excluded in [
            "Unshipped decoder adjustment",
            "Retired renderer cleanup",
            "Private keyboard report",
        ] {
            assert!(!page.contains(excluded), "{excluded} on {path}");
        }
    }
    Ok(())
}
