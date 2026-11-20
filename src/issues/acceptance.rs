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
    let mut close_fields = fields.to_vec();
    close_fields.push(("finding_closed", "true"));
    assert_eq!(
        app.change(&owner, finding, "edit", &close_fields).await?,
        StatusCode::SEE_OTHER
    );
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
