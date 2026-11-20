use super::{
    DependencyForm, IssueForm, MilestoneForm, RoadmapQuery, create, roadmap, save_dependency,
    save_issue, update,
};
use crate::{AppState, auth::Auth};
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

const CSRF: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

fn actor(user: Uuid) -> Auth {
    Auth {
        user_id: user,
        email: format!("{user}@example.invalid"),
        csrf: CSRF.into(),
    }
}

struct Fixture {
    state: AppState,
    owner: Uuid,
    project: Uuid,
}

impl Fixture {
    async fn new(db: PgPool) -> Self {
        let owner = user(&db).await;
        let project = project(&db, owner, "ROAD").await;
        let state = AppState::for_test(db);
        Self {
            state,
            owner,
            project,
        }
    }

    async fn milestone(&self, title: &str) -> Uuid {
        let response = create(
            State(self.state.clone()),
            actor(self.owner),
            Path(self.project),
            Form(milestone_form(title, None)),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        sqlx::query_scalar("SELECT id FROM milestones WHERE project_id=$1 AND title=$2")
            .bind(self.project)
            .bind(title)
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }

    async fn edge(&self, dependent: Uuid, prerequisite: Uuid, operation: &str) -> StatusCode {
        save_dependency(
            State(self.state.clone()),
            actor(self.owner),
            Path((self.project, dependent)),
            Form(DependencyForm {
                csrf: CSRF.into(),
                prerequisite_id: prerequisite,
                operation: operation.into(),
            }),
        )
        .await
        .into_response()
        .status()
    }

    async fn link(&self, milestone: Uuid, issue: Uuid, operation: &str) -> StatusCode {
        save_issue(
            State(self.state.clone()),
            actor(self.owner),
            Path((self.project, milestone)),
            Form(IssueForm {
                csrf: CSRF.into(),
                issue_id: issue,
                operation: operation.into(),
            }),
        )
        .await
        .into_response()
        .status()
    }

    async fn edge_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM milestone_dependencies WHERE project_id=$1")
            .bind(self.project)
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }
}

async fn user(db: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id,email) VALUES ($1,$2)")
        .bind(id)
        .bind(format!("{id}@example.invalid"))
        .execute(db)
        .await
        .unwrap();
    id
}

async fn project(db: &PgPool, owner: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO projects (owner_id,name,key) VALUES ($1,$2,$2) RETURNING id")
        .bind(owner)
        .bind(key)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn issue(db: &PgPool, owner: Uuid, project: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO issues (owner_id,project_id,number,title)
         VALUES ($1,$2,1,'Roadmap issue') RETURNING id",
    )
    .bind(owner)
    .bind(project)
    .fetch_one(db)
    .await
    .unwrap()
}

fn milestone_form(title: &str, revision: Option<i64>) -> MilestoneForm {
    MilestoneForm {
        csrf: CSRF.into(),
        title: title.into(),
        details: String::new(),
        status: "planned".into(),
        start_on: "2026-11-02".into(),
        end_on: "2026-11-09".into(),
        target_on: "2026-11-15".into(),
        revision,
        id: Some(Uuid::new_v4()),
    }
}

#[sqlx::test]
async fn dependencies_serialize_concurrent_writes_and_reject_indirect_cycles(db: PgPool) {
    let f = Fixture::new(db).await;
    let a = f.milestone("Alpha").await;
    let b = f.milestone("Beta").await;
    let c = f.milestone("Candidate").await;
    // Hold the project row until both writers are waiting, so the race runs on every DB.
    let mut guard = f.state.db.begin().await.unwrap();
    sqlx::query("SELECT id FROM projects WHERE id=$1 FOR UPDATE")
        .bind(f.project)
        .execute(&mut *guard)
        .await
        .unwrap();
    let release_writers = async {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let waiting: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_stat_activity
                     WHERE datname = current_database() AND wait_event_type = 'Lock'
                     AND cardinality(pg_blocking_pids(pid)) > 0",
                )
                .fetch_one(&mut *guard)
                .await
                .unwrap();
                if waiting >= 2 {
                    break;
                }
                // PostgreSQL caches statistics until the next transaction snapshot.
                sqlx::query("SELECT pg_stat_clear_snapshot()")
                    .execute(&mut *guard)
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("both dependency writers should wait for the project lock");
        guard.commit().await.unwrap();
    };
    let (ab, ba, ()) = tokio::join!(f.edge(b, a, "add"), f.edge(a, b, "add"), release_writers);
    let (first, second, refused) = if ab == StatusCode::SEE_OTHER {
        (a, b, ba)
    } else {
        assert_eq!(ba, StatusCode::SEE_OTHER);
        (b, a, ab)
    };
    assert!(refused.is_client_error());
    assert_eq!(f.edge_count().await, 1);

    assert!(f.edge(first, first, "add").await.is_client_error());
    assert_eq!(f.edge(c, second, "add").await, StatusCode::SEE_OTHER);
    assert!(f.edge(first, c, "add").await.is_client_error());
    assert_eq!(f.edge_count().await, 2);

    assert_eq!(f.edge(c, second, "remove").await, StatusCode::SEE_OTHER);
    assert_eq!(f.edge(first, c, "add").await, StatusCode::SEE_OTHER);
    assert_eq!(f.edge_count().await, 2);
    let rejected = sqlx::query(
        "INSERT INTO milestone_dependencies(owner_id,project_id,prerequisite_id,dependent_id)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(f.owner)
    .bind(f.project)
    .bind(second)
    .bind(c)
    .execute(&f.state.db)
    .await
    .unwrap_err();
    assert_eq!(
        rejected.as_database_error().and_then(|e| e.constraint()),
        Some("milestone_dependencies_acyclic")
    );
}

#[sqlx::test]
async fn roadmap_uses_workspace_calendar_day(db: PgPool) {
    let owner = user(&db).await;
    let mut days = Vec::new();
    for zone in ["Pacific/Kiritimati", "Pacific/Pago_Pago"] {
        sqlx::query("UPDATE users SET timezone=$2 WHERE id=$1")
            .bind(owner)
            .bind(zone)
            .execute(&db)
            .await
            .unwrap();
        let (day, name) = super::workspace_clock(&db, owner).await.unwrap();
        assert_eq!(name, zone);
        days.push(day);
    }
    assert!(days[0] > days[1]);
}

#[sqlx::test]
async fn roadmap_mutations_scope_issue_and_milestone_links_and_preserve_edits(db: PgPool) {
    let f = Fixture::new(db).await;
    let db = &f.state.db;
    let m = f.milestone("Original milestone").await;
    let other_project = project(db, f.owner, "SIDE").await;
    let outsider = user(db).await;
    let outsider_project = project(db, outsider, "THEIR").await;
    let local_issue = issue(db, f.owner, f.project).await;
    let another_issue = issue(db, f.owner, other_project).await;
    let outsider_issue = issue(db, outsider, outsider_project).await;

    assert_eq!(f.link(m, local_issue, "add").await, StatusCode::SEE_OTHER);
    for (owner, project, id) in [
        (f.owner, other_project, another_issue),
        (outsider, outsider_project, outsider_issue),
    ] {
        let foreign: Uuid = sqlx::query_scalar(
            "INSERT INTO milestones (owner_id,project_id,title) VALUES ($1,$2,'Foreign milestone') RETURNING id"
        ).bind(owner).bind(project).fetch_one(db).await.unwrap();
        assert!(f.edge(m, foreign, "add").await.is_client_error());
        assert!(f.edge(foreign, m, "add").await.is_client_error());
        assert!(f.link(m, id, "add").await.is_client_error());
        assert!(f.link(foreign, local_issue, "add").await.is_client_error());
        let invalid_fk = sqlx::query("INSERT INTO milestone_issues(owner_id,project_id,milestone_id,issue_id) VALUES ($1,$2,$3,$4)")
            .bind(f.owner).bind(f.project).bind(m).bind(id).execute(db).await.expect_err("FK must prevent cross-project issue links");
        assert!(
            invalid_fk
                .as_database_error()
                .is_some_and(|e| e.is_foreign_key_violation())
        );
    }
    assert_eq!(f.edge_count().await, 0);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM milestone_issues WHERE project_id=$1")
            .bind(f.project)
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let selected = |who, project| {
        roadmap(
            State(f.state.clone()),
            actor(who),
            Query(RoadmapQuery {
                project: Some(project),
                milestone: Some(m),
                ..RoadmapQuery::default()
            }),
        )
    };
    let body = selected(f.owner, f.project).await.unwrap().0;
    assert!(body.contains("Original milestone"));
    assert!(body.contains(&format!("href=\"/issues/{local_issue}\"")));
    assert!(!body.contains(&another_issue.to_string()));
    assert!(!body.contains(&outsider_issue.to_string()));
    assert!(!body.contains("Foreign milestone"));
    assert_eq!(
        selected(f.owner, other_project)
            .await
            .into_response()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        selected(outsider, f.project).await.into_response().status(),
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        f.link(m, local_issue, "remove").await,
        StatusCode::SEE_OTHER
    );

    let revision: i64 = sqlx::query_scalar("SELECT revision FROM milestones WHERE id=$1")
        .bind(m)
        .fetch_one(db)
        .await
        .unwrap();
    let edit = |who, form| {
        update(
            State(f.state.clone()),
            actor(who),
            Path((f.project, m)),
            Form(form),
        )
    };
    let mut forged = milestone_form("Forged", Some(revision));
    forged.csrf = "wrong".into();
    assert_eq!(
        edit(f.owner, forged).await.into_response().status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        edit(outsider, milestone_form("Outsider", Some(revision)))
            .await
            .into_response()
            .status()
            .is_client_error()
    );
    assert_eq!(
        edit(f.owner, milestone_form("Updated", Some(revision)))
            .await
            .into_response()
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        edit(f.owner, milestone_form("Stale", Some(revision)))
            .await
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );

    sqlx::query("UPDATE projects SET archived=true WHERE id=$1")
        .bind(f.project)
        .execute(db)
        .await
        .unwrap();
    assert!(
        edit(f.owner, milestone_form("Archived", Some(revision + 1)))
            .await
            .into_response()
            .status()
            .is_client_error()
    );
    assert!(f.link(m, local_issue, "add").await.is_client_error());
    let saved: String = sqlx::query_scalar("SELECT title FROM milestones WHERE id=$1")
        .bind(m)
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(saved, "Updated");
}

#[sqlx::test]
async fn release_assignment_and_unavailable_weeks_apply_to_the_selected_project(db: PgPool) {
    use super::{ReleaseForm, calendar::save_availability, save_release};
    use serde_json::{from_value, json};

    let f = Fixture::new(db).await;
    let m = f.milestone("Gate").await;
    let other = project(&f.state.db, f.owner, "ELSE").await;
    let mut releases = Vec::new();
    for (project, name) in [(f.project, "November release"), (other, "Sibling release")] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO releases (owner_id,project_id,name,target_on)
             VALUES ($1,$2,$3,DATE '2026-11-18') RETURNING id",
        )
        .bind(f.owner)
        .bind(project)
        .bind(name)
        .fetch_one(&f.state.db)
        .await
        .unwrap();
        releases.push(id);
    }
    let assign = |release: String, revision| {
        let fields: ReleaseForm = from_value(json!({
            "csrf": CSRF, "release_id": release, "revision": revision
        }))
        .unwrap();
        save_release(
            State(f.state.clone()),
            actor(f.owner),
            Path((f.project, m)),
            Form(fields),
        )
    };
    assert_eq!(
        assign(releases[1].to_string(), 1)
            .await
            .into_response()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        assign(releases[0].to_string(), 1)
            .await
            .into_response()
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        assign(String::new(), 1).await.into_response().status(),
        StatusCode::CONFLICT
    );
    let recorded: Uuid =
        sqlx::query_scalar("SELECT release_id FROM milestone_releases WHERE milestone_id=$1")
            .bind(m)
            .fetch_one(&f.state.db)
            .await
            .unwrap();
    assert_eq!(recorded, releases[0]);

    let availability = |who, date: &str, action: &str, note: &str| {
        let fields =
            from_value(json!({"csrf": CSRF, "date": date, "action": action, "note": note}))
                .unwrap();
        save_availability(
            State(f.state.clone()),
            actor(who),
            Path(f.project),
            Form(fields),
        )
    };
    assert_eq!(
        availability(f.owner, "2026-11-19", "save", "Away")
            .await
            .into_response()
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        availability(f.owner, "2026-11-21", "save", "Traveling")
            .await
            .into_response()
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        availability(Uuid::new_v4(), "2026-11-19", "remove", "")
            .await
            .into_response()
            .status(),
        StatusCode::NOT_FOUND
    );
    let saved: Vec<(String, String)> =
        sqlx::query_as("SELECT week_of::text,note FROM roadmap_weeks_off WHERE project_id=$1")
            .bind(f.project)
            .fetch_all(&f.state.db)
            .await
            .unwrap();
    assert_eq!(saved, vec![("2026-11-16".into(), "Traveling".into())]);

    let page = roadmap(
        State(f.state.clone()),
        actor(f.owner),
        Query(RoadmapQuery {
            project: Some(f.project),
            milestone: Some(m),
            ..RoadmapQuery::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert!(page.contains("November release"));
    assert!(page.contains("Traveling"));
    assert!(!page.contains("Sibling release"));
    assert_eq!(
        availability(f.owner, "2026-11-16", "remove", "")
            .await
            .into_response()
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        assign(String::new(), 2).await.into_response().status(),
        StatusCode::SEE_OTHER
    );

    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM roadmap_weeks_off WHERE project_id=$1),
                (SELECT count(*) FROM milestone_releases WHERE project_id=$1)",
    )
    .bind(f.project)
    .fetch_one(&f.state.db)
    .await
    .unwrap();
    assert_eq!(counts, (0, 0));
}

#[sqlx::test]
async fn release_span_writes_preserve_targets_and_reject_stale_or_published_edits(db: PgPool) {
    use super::calendar::{load_calendar, save_release_span};
    use serde_json::{from_value, json};

    let f = Fixture::new(db).await;
    let other = project(&f.state.db, f.owner, "SIBLING").await;
    let release: Uuid = sqlx::query_scalar(
        "INSERT INTO releases(owner_id,project_id,name,target_on)
         VALUES ($1,$2,'Release gate',DATE '2026-12-01') RETURNING id",
    )
    .bind(f.owner)
    .bind(f.project)
    .fetch_one(&f.state.db)
    .await
    .unwrap();
    let save = |owner, project, rev, start: &str, end: &str| {
        let form =
            from_value(json!({"csrf":CSRF,"revision":rev,"start_on":start,"end_on":end})).unwrap();
        save_release_span(
            State(f.state.clone()),
            actor(owner),
            Path((project, release)),
            Form(form),
        )
    };
    for (owner, project) in [(f.owner, other), (Uuid::new_v4(), f.project)] {
        assert!(
            save(owner, project, 0, "2026-11-04", "2026-11-20")
                .await
                .into_response()
                .status()
                .is_client_error()
        );
    }
    assert_eq!(
        save(f.owner, f.project, 0, "2026-11-04", "")
            .await
            .into_response()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        save(f.owner, f.project, 0, "2026-11-04", "2026-11-20")
            .await
            .into_response()
            .status(),
        StatusCode::SEE_OTHER
    );
    let (.., releases) = load_calendar(&f.state.db, f.owner, f.project)
        .await
        .unwrap();
    let scheduled = &releases[0];
    assert_eq!(scheduled.start_on.unwrap().to_string(), "2026-11-04");
    assert_eq!(scheduled.end_on.unwrap().to_string(), "2026-11-20");
    assert_eq!(scheduled.target_on.unwrap().to_string(), "2026-12-01");
    assert_eq!(scheduled.published_on, None);
    assert_eq!(
        save(f.owner, f.project, 0, "", "")
            .await
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        save(f.owner, f.project, scheduled.span_revision, "", "")
            .await
            .into_response()
            .status(),
        StatusCode::SEE_OTHER
    );
    let (.., releases) = load_calendar(&f.state.db, f.owner, f.project)
        .await
        .unwrap();
    let cleared = &releases[0];
    assert!(cleared.span_revision > scheduled.span_revision);
    assert_eq!((cleared.start_on, cleared.end_on), (None, None));
    assert_eq!(
        save(f.owner, f.project, 0, "2026-11-01", "2026-11-21")
            .await
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE releases SET published_at=now(), published_on=DATE '2026-12-03', publish_command=$2 WHERE id=$1")
        .bind(release).bind(Uuid::new_v4()).execute(&f.state.db).await.unwrap();
    assert!(
        save(
            f.owner,
            f.project,
            cleared.span_revision,
            "2026-11-01",
            "2026-11-21"
        )
        .await
        .into_response()
        .status()
        .is_client_error()
    );
}
