//! Canvas HTTP behavior on migrated PostgreSQL, with independent real workspace sessions.
use crate::AppState;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Session {
    app: Router,
    owner: Uuid,
    project: Uuid,
    cookie: String,
    csrf: String,
}

impl Session {
    async fn new(db: &PgPool) -> Self {
        let owner = Uuid::new_v4();
        let project = Uuid::new_v4();
        let raw = hex::encode(Sha256::digest(Uuid::new_v4().as_bytes()));
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(owner)
            .bind(format!("{owner}@example.invalid"))
            .execute(db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO projects(id,owner_id,name,key) VALUES($1,$2,'Test project','DEV')",
        )
        .bind(project)
        .bind(owner)
        .execute(db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO user_sessions(user_id,token_hash) VALUES($1,$2)")
            .bind(owner)
            .bind(Sha256::digest(raw.as_bytes()).to_vec())
            .execute(db)
            .await
            .unwrap();
        let mut session = Self {
            app: crate::app(AppState::for_test(db.clone())),
            owner,
            project,
            cookie: format!("workspace_session={raw}"),
            csrf: String::new(),
        };
        let page = session.get("/canvas").await;
        assert_eq!(page.status(), StatusCode::OK);
        let bytes = to_bytes(page.into_body(), 256_000).await.unwrap();
        let html = std::str::from_utf8(&bytes).unwrap();
        session.csrf = html
            .split_once("name=\"csrf\" value=\"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .expect("csrf in a rendered form")
            .0
            .to_owned();
        session
    }

    async fn get(&self, path: &str) -> Response {
        self.app
            .clone()
            .oneshot(
                Request::get(path)
                    .header(header::COOKIE, &self.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, payload: Value) -> Response {
        self.app
            .clone()
            .oneshot(
                Request::post(path)
                    .header(header::COOKIE, &self.cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn create_in(&self, project: Uuid) -> Response {
        self.form(
            "/canvas",
            &[
                ("project", &project.to_string()),
                ("title", "Deployment map"),
                ("template", "blank"),
            ],
        )
        .await
    }

    async fn form(&self, path: &str, fields: &[(&str, &str)]) -> Response {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &self.csrf)
            .extend_pairs(fields.iter().copied())
            .finish();
        self.app
            .clone()
            .oneshot(
                Request::post(path)
                    .header(header::COOKIE, &self.cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn new_board(&self) -> String {
        let response = self.create_in(self.project).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let editor = response.headers()[header::LOCATION].to_str().unwrap();
        assert!(editor.starts_with("/canvas/"));
        assert_eq!(self.get(editor).await.status(), StatusCode::OK);
        format!("/api{editor}")
    }

    async fn read(&self, path: &str) -> Value {
        let response = self.get(path).await;
        assert_eq!(response.status(), StatusCode::OK, "GET {path}");
        response_json(response).await
    }

    fn update(&self, board: &Value, title: &str) -> Value {
        json!({"csrf":self.csrf,"revision":board["revision"],"document":board["document"],"title":title})
    }
}

async fn response_json(response: Response) -> Value {
    let body = to_bytes(response.into_body(), 3_000_000).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[sqlx::test]
async fn concurrent_writers_keep_one_complete_revision(db: PgPool) {
    let owner = Session::new(&db).await;
    let path = owner.new_board().await;
    let board = owner.read(&path).await;
    assert_eq!(board["title"], "Deployment map");
    assert_eq!(board["project"], owner.project.to_string());
    assert_eq!(board["read_only"], false);
    assert_eq!(board["document"]["version"], 1);
    let noop = owner
        .post(&path, owner.update(&board, "Deployment map"))
        .await;
    assert_eq!(noop.status(), StatusCode::OK);
    assert_eq!(response_json(noop).await["revision"], board["revision"]);
    assert_eq!(owner.read(&path).await, board);

    let left = owner.update(&board, "Branch A");
    let right = owner.update(&board, "Branch B");
    let (a, b) = tokio::join!(owner.post(&path, left), owner.post(&path, right));
    let winner = match (a.status(), b.status()) {
        (StatusCode::OK, StatusCode::CONFLICT) => "Branch A",
        (StatusCode::CONFLICT, StatusCode::OK) => "Branch B",
        status => panic!("exactly one writer must win, got {status:?}"),
    };
    let saved = owner.read(&path).await;
    assert_eq!(saved["title"], winner);
    assert_eq!(
        saved["revision"].as_i64().unwrap(),
        board["revision"].as_i64().unwrap() + 1
    );

    let mut invalid = owner.update(&saved, "Broken endpoints");
    invalid["document"]["edges"] =
        json!([{"id":Uuid::new_v4(),"from":Uuid::new_v4(),"to":Uuid::new_v4(),"label":""}]);
    assert_eq!(
        owner.post(&path, invalid).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(owner.read(&path).await, saved);

    let deletion = format!("{}/delete", path.strip_prefix("/api").unwrap());
    let original_revision = board["revision"].to_string();
    let current_revision = saved["revision"].to_string();
    let checkpoint = owner
        .post(
            &format!("{path}/checkpoints"),
            json!({"csrf":owner.csrf,"revision":saved["revision"],"name":"Before deleting"}),
        )
        .await;
    assert!(checkpoint.status().is_success());
    let snap: Uuid = response_json(checkpoint).await["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let other = Session::new(&db).await;
    assert_eq!(
        other
            .form(
                &deletion,
                &[("revision", &current_revision), ("confirmation", winner)]
            )
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        owner
            .form(
                &deletion,
                &[("revision", &original_revision), ("confirmation", winner)]
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        owner
            .form(
                &deletion,
                &[
                    ("revision", &current_revision),
                    ("confirmation", "Different board")
                ]
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(owner.read(&path).await, saved);
    assert_eq!(
        owner
            .form(
                &deletion,
                &[("revision", &current_revision), ("confirmation", winner)]
            )
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(owner.get(&path).await.status(), StatusCode::NOT_FOUND);
    let still_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM canvas_checkpoints WHERE id=$1)")
            .bind(snap)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(!still_exists);
}

#[sqlx::test]
async fn other_owners_and_archived_project_cannot_write(db: PgPool) {
    let owner = Session::new(&db).await;
    let other = Session::new(&db).await;
    let path = owner.new_board().await;
    let board = owner.read(&path).await;
    let poll = format!("{path}?since={}&read_only=false", board["revision"]);
    let anonymous = owner
        .app
        .clone()
        .oneshot(Request::get(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(response_json(anonymous).await["error"].is_string());
    assert_eq!(owner.get(&poll).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(other.get(&poll).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        other.create_in(owner.project).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(other.get(&path).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        other.get(path.strip_prefix("/api").unwrap()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other
            .get(&format!("{path}/sources?q=sample"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other.get(&format!("{path}/checkpoints")).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other
            .post(&path, other.update(&board, "Foreign"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut foreign_token = owner.update(&board, "Invalid CSRF");
    foreign_token["csrf"] = json!(other.csrf);
    assert_eq!(
        owner.post(&path, foreign_token).await.status(),
        StatusCode::FORBIDDEN
    );

    sqlx::query("UPDATE projects SET archived=true WHERE id=$1")
        .bind(owner.project)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(owner.read(&poll).await["read_only"], true);
    assert_eq!(
        owner
            .post(&path, owner.update(&board, "Archived"))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(owner.read(&path).await["revision"], board["revision"]);
}

async fn issue(db: &PgPool, owner: Uuid, project: Uuid) -> Uuid {
    sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title) VALUES($1,$2,1,'Canvas source issue') RETURNING id")
        .bind(owner).bind(project).fetch_one(db).await.unwrap()
}

fn node(source: Uuid) -> Value {
    json!({"id":Uuid::new_v4(),"kind":"process","label":"Build","body":"","x":0,"y":0,"w":160,"h":80,
           "source":{"kind":"issue","id":source}})
}

#[sqlx::test]
async fn pins_are_project_scoped_and_deleted_sources_do_not_block_editing(db: PgPool) {
    let owner = Session::new(&db).await;
    let other = Session::new(&db).await;
    let extra_project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects(owner_id,name,key) VALUES($1,'Other project','TWO') RETURNING id",
    )
    .bind(owner.owner)
    .fetch_one(&db)
    .await
    .unwrap();
    let own_issue = issue(&db, owner.owner, owner.project).await;
    let other_issue = issue(&db, other.owner, other.project).await;
    let sibling_issue = issue(&db, owner.owner, extra_project).await;
    let own_note: Uuid=sqlx::query_scalar("INSERT INTO notes(owner_id,project_id,title,path) VALUES($1,$2,'Canvas source note','Canvas.md') RETURNING id")
        .bind(owner.owner).bind(owner.project).fetch_one(&db).await.unwrap();
    let path = owner.new_board().await;
    let board = owner.read(&path).await;

    let found = owner.read(&format!("{path}/sources?q=Canvas")).await;
    let choices = found.as_array().unwrap();
    assert_eq!(choices.len(), 2);
    assert!(
        choices
            .iter()
            .any(|s| s["kind"] == "issue" && s["id"] == own_issue.to_string())
    );
    assert!(
        choices
            .iter()
            .any(|s| s["kind"] == "note" && s["id"] == own_note.to_string())
    );
    for denied in [sibling_issue, other_issue] {
        let mut edit = owner.update(&board, "Wrong reference");
        edit["document"]["nodes"] = json!([node(denied)]);
        assert_eq!(
            owner.post(&path, edit).await.status(),
            StatusCode::BAD_REQUEST
        );
    }

    let mut edit = owner.update(&board, "Release");
    edit["document"]["nodes"] = json!([node(own_issue)]);
    assert_eq!(owner.post(&path, edit).await.status(), StatusCode::OK);
    let before_delete = owner.read(&path).await;
    assert_eq!(
        before_delete["document"]["nodes"][0]["source"]["id"],
        own_issue.to_string()
    );
    sqlx::query("DELETE FROM issues WHERE id=$1")
        .bind(own_issue)
        .execute(&db)
        .await
        .unwrap();
    assert!(owner.read(&path).await["document"]["nodes"][0]["source"].is_null());
    // A browser holding the previous payload can save without resurrecting its broken pin.
    let stale_pin = owner.update(&before_delete, "After source removal");
    let detached = owner.post(&path, stale_pin).await;
    assert_eq!(detached.status(), StatusCode::OK);
    assert_eq!(
        response_json(detached).await["detached"],
        json!([{
            "node":before_delete["document"]["nodes"][0]["id"], "kind":"issue", "id":own_issue
        }])
    );
    let repaired = owner.read(&path).await;
    assert_eq!(repaired["title"], "After source removal");
    assert_eq!(repaired["document"]["nodes"][0]["label"], "Build");
    assert!(repaired["document"]["nodes"][0]["source"].is_null());
}

#[sqlx::test]
async fn metered_diagrams_and_snapshots_follow_project_deletion(db: PgPool) {
    let owner = Session::new(&db).await;
    let initial = crate::store::storage_used(&db, owner.owner).await.unwrap();
    let path = owner.new_board().await;
    let after_board = crate::store::storage_used(&db, owner.owner).await.unwrap();
    assert!(after_board > initial);
    let board = owner.read(&path).await;
    let snapshots = format!("{path}/checkpoints");
    let saved = owner
        .post(
            &snapshots,
            json!({"csrf":owner.csrf,"revision":board["revision"],"name":"Before migration"}),
        )
        .await;
    assert!(saved.status().is_success(), "{}", saved.status());
    let snapshot_count = owner.read(&snapshots).await;
    assert_eq!(snapshot_count.as_array().unwrap().len(), 1);
    let after_snapshot = crate::store::storage_used(&db, owner.owner).await.unwrap();
    assert!(after_snapshot > after_board);

    // Fill the remaining allowance without allocating a 100 MB test document.
    sqlx::query("UPDATE users SET content_bytes=$2 WHERE id=$1")
        .bind(owner.owner)
        .bind(crate::store::limits("free").storage_bytes)
        .execute(&db)
        .await
        .unwrap();
    let original = owner.read(&path).await;
    let growth = owner.update(&original, "A substantially longer diagram name");
    assert_eq!(
        owner.post(&path, growth).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(owner.read(&path).await, original);
    sqlx::query("UPDATE users SET content_bytes=$2 WHERE id=$1")
        .bind(owner.owner)
        .bind(after_snapshot)
        .execute(&db)
        .await
        .unwrap();

    sqlx::query("DELETE FROM projects WHERE id=$1")
        .bind(owner.project)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(owner.get(&path).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        crate::store::storage_used(&db, owner.owner).await.unwrap(),
        initial
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM canvas_checkpoints WHERE owner_id=$1")
            .bind(owner.owner)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[sqlx::test]
async fn quota_refusals_preserve_the_board_and_all_checkpoint_history(db: PgPool) {
    let owner = Session::new(&db).await;
    let path = owner.new_board().await;
    let checkpoints = format!("{path}/checkpoints");
    let blank = owner.read(&path).await;

    // Restoring this larger drawing would grow the account's usage even after rotating a backup.
    let mut larger = owner.update(&blank, "Architecture sketch");
    larger["document"] =
        serde_json::to_value(super::model::Document::from_template("architecture").unwrap())
            .unwrap();
    assert_eq!(owner.post(&path, larger).await.status(), StatusCode::OK);
    let planned = owner.read(&path).await;
    let save = owner
        .post(
            &checkpoints,
            json!({
                "csrf": owner.csrf, "revision": planned["revision"], "name":"Reference design"
            }),
        )
        .await;
    assert_eq!(save.status(), StatusCode::OK);
    let saved = response_json(save).await;
    let restore = format!("{checkpoints}/{}/restore", saved["id"].as_str().unwrap());

    let mut shrink = owner.update(&planned, "Working copy");
    shrink["document"] = blank["document"].clone();
    assert_eq!(owner.post(&path, shrink).await.status(), StatusCode::OK);
    let board = owner.read(&path).await;
    let board_id = Uuid::parse_str(board["id"].as_str().unwrap()).unwrap();
    sqlx::query(
        "INSERT INTO canvas_checkpoints (owner_id,project_id,board_id,name,title,document,automatic)
         SELECT owner_id,project_id,id,'Backup ' || n,title,document,true
           FROM canvas_boards CROSS JOIN generate_series(1,5) AS slots(n) WHERE id=$1"
    ).bind(board_id).execute(&db).await.unwrap();
    let history = owner.read(&checkpoints).await;
    assert_eq!(history.as_array().unwrap().len(), 6);
    let raw_rows =
        "SELECT jsonb_agg(to_jsonb(c) ORDER BY id) FROM canvas_checkpoints c WHERE board_id=$1";
    let full_history: Value = sqlx::query_scalar(raw_rows)
        .bind(board_id)
        .fetch_one(&db)
        .await
        .unwrap();

    let cap = crate::store::limits("free").storage_bytes;
    sqlx::query("UPDATE users SET content_bytes=$2 WHERE id=$1")
        .bind(owner.owner)
        .bind(cap)
        .execute(&db)
        .await
        .unwrap();

    for (endpoint, command) in [
        (
            &checkpoints,
            json!({"csrf":owner.csrf,"revision":board["revision"],"name":"Blocked checkpoint"}),
        ),
        (
            &restore,
            json!({"csrf":owner.csrf,"revision":board["revision"]}),
        ),
    ] {
        let rejected = owner.post(endpoint, command).await;
        assert_eq!(rejected.status(), StatusCode::CONFLICT);
        let error = response_json(rejected).await;
        assert!(
            error["error"].as_str().unwrap().contains("storage limit"),
            "{error}"
        );
        assert_eq!(owner.read(&path).await, board);
        assert_eq!(owner.read(&checkpoints).await, history);
        let after: Value = sqlx::query_scalar(raw_rows)
            .bind(board_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(after, full_history);
        assert_eq!(
            crate::store::storage_used(&db, owner.owner).await.unwrap(),
            cap
        );
    }
}

#[sqlx::test]
async fn new_board_page_respects_project_scope_without_creating_a_board(db: PgPool) {
    let owner = Session::new(&db).await;
    let foreign = Session::new(&db).await;
    owner.new_board().await;
    let form_url = format!("/canvas?project={}&new=true", owner.project);
    let page = owner.get(&form_url).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html =
        String::from_utf8(to_bytes(page.into_body(), 256_000).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("class=\"canvas-create-form\""));
    assert!(html.contains(&format!("name=\"project\" value=\"{}\"", owner.project)));
    assert!(html.contains(&format!("href=\"/canvas?project={}\"", owner.project)));
    assert!(!html.contains("Deployment map"));
    assert_eq!(foreign.get(&form_url).await.status(), StatusCode::NOT_FOUND);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM canvas_boards WHERE owner_id=$1")
        .bind(owner.owner)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 1);

    sqlx::query("UPDATE projects SET archived=true WHERE owner_id=$1 AND id=$2")
        .bind(owner.owner)
        .bind(owner.project)
        .execute(&db)
        .await
        .unwrap();
    let archived = owner.get(&form_url).await;
    let html = String::from_utf8(
        to_bytes(archived.into_body(), 256_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!html.contains("class=\"canvas-create-form\""));
    assert!(html.contains("active project"));

    sqlx::query("DELETE FROM projects WHERE owner_id=$1")
        .bind(foreign.owner)
        .execute(&db)
        .await
        .unwrap();
    let no_projects = foreign.get("/canvas?new=true").await;
    assert_eq!(no_projects.status(), StatusCode::OK);
    let html = String::from_utf8(
        to_bytes(no_projects.into_body(), 256_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("href=\"/projects#new-project\""));
    assert!(!html.contains("class=\"canvas-create-form\""));
}
