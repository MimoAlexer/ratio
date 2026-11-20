use super::cards::{coverage_db, home_db};
use super::{Command, disconnect};
use crate::{AppState, auth::Auth};
use axum::{Form, extract::State, http::StatusCode, response::IntoResponse};
use sqlx::PgPool;
use uuid::Uuid;

struct Workspace {
    actor: Auth,
    project: Uuid,
}

async fn workspace(db: &PgPool) -> Workspace {
    let user_id = Uuid::new_v4();
    let email = format!("{user_id}@example.invalid");
    sqlx::query("INSERT INTO users(id,email) VALUES ($1,$2)")
        .bind(user_id)
        .bind(&email)
        .execute(db)
        .await
        .unwrap();
    let project = sqlx::query_scalar(
        "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Private project','WORK') RETURNING id",
    )
    .bind(user_id)
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO github_connections(owner_id,user_remote_id,access_cipher,expires_at)
         VALUES ($1,111,$2,now() + interval '1 hour')",
    )
    .bind(user_id)
    .bind(b"fixture-ciphertext".as_slice())
    .execute(db)
    .await
    .unwrap();
    let actor = Auth {
        user_id,
        email,
        csrf: "a".repeat(64),
    };
    Workspace { actor, project }
}

async fn seed_repo(db: &PgPool, workspace: &Workspace, repo: i64, name: &str, package: &str) {
    sqlx::query(
        "INSERT INTO github_repos(owner_id,repo_id,project_id,installation_id,display_name,
                                  repo_grant_until,alert_grant_until,last_success,last_attempt)
         VALUES ($1,$2,$3,123,$4,now() + interval '4 minutes',now() + interval '4 minutes',now(),now())",
    )
    .bind(workspace.actor.user_id)
    .bind(repo)
    .bind(workspace.project)
    .bind(name)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO github_alerts(owner_id,repo_id,number,package,manifest,patched,updated_at)
         VALUES ($1,$2,77,$3,'private/build/manifest.lock','1.3.9',now())",
    )
    .bind(workspace.actor.user_id)
    .bind(repo)
    .bind(package)
    .execute(db)
    .await
    .unwrap();
}

async fn panel(db: &PgPool, workspace: &Workspace) -> String {
    home_db(db, &workspace.actor, Some(workspace.project), true)
        .await
        .into_string()
}

#[sqlx::test(migrations = "./migrations")]
async fn home_counts_only_checked_sources_for_the_authenticated_owner(db: PgPool) {
    let member = workspace(&db).await;
    let other = workspace(&db).await;
    seed_repo(&db, &member, 41, "owner/checked", "owner-only-package").await;
    seed_repo(&db, &other, 41, "other/private", "other-only-package").await;
    seed_repo(&db, &member, 42, "owner/pending", "partial-page-package").await;
    sqlx::query("UPDATE github_repos SET last_success=NULL WHERE owner_id=$1 AND repo_id=42")
        .bind(member.actor.user_id)
        .execute(&db)
        .await
        .unwrap();
    seed_repo(&db, &member, 43, "owner/paused", "paused-source-package").await;
    sqlx::query("UPDATE github_repos SET billing_paused=true WHERE owner_id=$1 AND repo_id=43")
        .bind(member.actor.user_id)
        .execute(&db)
        .await
        .unwrap();

    seed_repo(&db, &member, 44, "owner/stale", "stale-package").await;
    sqlx::query("UPDATE github_repos SET last_success=now()-interval '3 hours' WHERE owner_id=$1 AND repo_id=44")
        .bind(member.actor.user_id)
        .execute(&db)
        .await
        .unwrap();
    seed_repo(&db, &member, 45, "owner/clear", "resolved-package").await;
    sqlx::query("DELETE FROM github_alerts WHERE owner_id=$1 AND repo_id=45")
        .bind(member.actor.user_id)
        .execute(&db)
        .await
        .unwrap();
    let other_project = Workspace {
        actor: member.actor.clone(),
        project: sqlx::query_scalar("INSERT INTO projects(owner_id,name,key) VALUES ($1,'Another project','OTHER') RETURNING id")
            .bind(member.actor.user_id).fetch_one(&db).await.unwrap(),
    };
    seed_repo(
        &db,
        &other_project,
        46,
        "owner/second-project",
        "second-project-package",
    )
    .await;
    assert_eq!(
        coverage_db(&db, member.actor.user_id, member.project, true)
            .await
            .unwrap(),
        (2, 2, 1)
    );
    let html = panel(&db, &member).await;
    assert!(html.contains("owner-only-package"));
    for denied in [
        "other-only-package",
        "partial-page-package",
        "paused-source-package",
        "stale-package",
        "resolved-package",
        "second-project-package",
    ] {
        assert!(!html.contains(denied), "unexpected source value {denied}");
    }
    let cross_project = home_db(&db, &other.actor, Some(member.project), true)
        .await
        .into_string();
    assert!(!cross_project.contains("owner-only-package"));
    assert!(!cross_project.contains("owner/checked"));
}

#[sqlx::test(migrations = "./migrations")]
async fn home_hides_cached_alert_details_when_effective_access_expires(db: PgPool) {
    let member = workspace(&db).await;
    seed_repo(&db, &member, 43, "owner/private-repo", "sensitive-package").await;
    assert_eq!(
        coverage_db(&db, member.actor.user_id, member.project, false)
            .await
            .unwrap(),
        (0, 1, 0)
    );
    assert!(
        !home_db(&db, &member.actor, Some(member.project), false)
            .await
            .into_string()
            .contains("sensitive-package")
    );
    let good = panel(&db, &member).await;
    assert!(good.contains("sensitive-package"));
    assert!(good.contains("private/build/manifest.lock"));

    // Repository metadata access can remain while the authorization to read alerts expires.
    sqlx::query(
        "UPDATE github_repos SET alert_grant_until=now()-interval '1 second' WHERE owner_id=$1",
    )
    .bind(member.actor.user_id)
    .execute(&db)
    .await
    .unwrap();
    let denied = panel(&db, &member).await;
    assert_eq!(
        coverage_db(&db, member.actor.user_id, member.project, true)
            .await
            .unwrap(),
        (0, 1, 0)
    );
    for private in [
        "sensitive-package",
        "private/build/manifest.lock",
        "/dependabot/77",
    ] {
        assert!(!denied.contains(private));
    }

    // Expiry of repository visibility invalidates the alert association too.
    sqlx::query(
        "UPDATE github_repos SET alert_grant_until=now()+interval '4 minutes',
                repo_grant_until=now()-interval '1 second' WHERE owner_id=$1",
    )
    .bind(member.actor.user_id)
    .execute(&db)
    .await
    .unwrap();
    let denied = panel(&db, &member).await;
    assert!(!denied.contains("owner/private-repo"));
    assert!(!denied.contains("sensitive-package"));

    // A cached alert grant cannot outlive the connection's underlying user access token.
    sqlx::query(
        "UPDATE github_repos SET repo_grant_until=now()+interval '4 minutes' WHERE owner_id=$1",
    )
    .bind(member.actor.user_id)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE github_connections SET expires_at=now()-interval '1 second' WHERE owner_id=$1",
    )
    .bind(member.actor.user_id)
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        coverage_db(&db, member.actor.user_id, member.project, true)
            .await
            .unwrap(),
        (0, 1, 0)
    );
    let denied = panel(&db, &member).await;
    for private in [
        "owner/private-repo",
        "sensitive-package",
        "private/build/manifest.lock",
        "/dependabot/77",
    ] {
        assert!(!denied.contains(private));
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn disconnect_purges_source_rows_and_preserves_local_work(db: PgPool) {
    let member = workspace(&db).await;
    seed_repo(&db, &member, 47, "owner/disconnect", "disconnect-package").await;
    let issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues(owner_id,project_id,number,title,body)
         VALUES ($1,$2,1,'Update build dependencies','My own triage notes') RETURNING id",
    )
    .bind(member.actor.user_id)
    .bind(member.project)
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO github_fixes(owner_id,repo_id,number,issue_id) VALUES ($1,47,77,$2)")
        .bind(member.actor.user_id)
        .bind(issue)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO github_pulls(owner_id,repo_id,number,issue_id) VALUES ($1,47,8,$2)")
        .bind(member.actor.user_id)
        .bind(issue)
        .execute(&db)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO github_oauth_states(owner_id,state_hash,session_hash,expires_at)
            VALUES($1,$2,$2,now()+interval '2 minutes')",
    )
    .bind(member.actor.user_id)
    .bind(b"fixture-state-hash".as_slice())
    .execute(&db)
    .await
    .unwrap();
    let state = AppState::for_test(db.clone());
    let refused = disconnect(
        State(state.clone()),
        member.actor.clone(),
        Form(Command {
            csrf: "b".repeat(64),
        }),
    )
    .await
    .into_response();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(panel(&db, &member).await.contains("disconnect-package"));
    let disconnected = disconnect(
        State(state),
        member.actor.clone(),
        Form(Command {
            csrf: member.actor.csrf.clone(),
        }),
    )
    .await
    .into_response();
    assert_eq!(disconnected.status(), StatusCode::SEE_OTHER);
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM github_oauth_states WHERE owner_id=$1")
            .bind(member.actor.user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(pending, 0);
    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM github_repos), (SELECT count(*) FROM github_alerts),
                (SELECT count(*) FROM github_pulls), (SELECT count(*) FROM github_fixes)",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(counts, (0, 0, 0, 0));
    let native: (String, String, bool) = sqlx::query_as(
        "SELECT title,body,security_critical FROM issues WHERE owner_id=$1 AND id=$2",
    )
    .bind(member.actor.user_id)
    .bind(issue)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        native,
        (
            "Update build dependencies".into(),
            "My own triage notes".into(),
            false
        )
    );
    assert_eq!(
        coverage_db(&db, member.actor.user_id, member.project, true)
            .await
            .unwrap(),
        (0, 0, 0)
    );
    assert!(!panel(&db, &member).await.contains("disconnect-package"));
}

#[sqlx::test(migrations = "./migrations")]
async fn known_revocation_purges_only_the_observed_repo_generation(db: PgPool) {
    let member = workspace(&db).await;
    let other = workspace(&db).await;
    seed_repo(&db, &member, 11, "owner/versioned", "member-package").await;
    seed_repo(&db, &other, 11, "other/versioned", "other-package").await;
    let state = AppState::for_test(db.clone());
    let generation: Uuid =
        sqlx::query_scalar("SELECT generation FROM github_repos WHERE owner_id=$1 AND repo_id=11")
            .bind(member.actor.user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    super::known_loss(&state, member.actor.user_id, 11, Uuid::new_v4())
        .await
        .unwrap();
    assert!(panel(&db, &member).await.contains("member-package"));

    super::known_loss(&state, member.actor.user_id, 11, generation)
        .await
        .unwrap();
    let remaining: (bool, bool, i64) = sqlx::query_as(
        "SELECT r.display_name='', r.repo_grant_until IS NULL AND r.alert_grant_until IS NULL,
                (SELECT count(*) FROM github_alerts a WHERE a.owner_id=r.owner_id AND a.repo_id=r.repo_id)
         FROM github_repos r WHERE r.owner_id=$1 AND r.repo_id=11",
    ).bind(member.actor.user_id).fetch_one(&db).await.unwrap();
    assert_eq!(remaining, (true, true, 0));
    assert_eq!(
        coverage_db(&db, member.actor.user_id, member.project, true)
            .await
            .unwrap(),
        (0, 1, 0)
    );
    assert!(panel(&db, &other).await.contains("other-package"));
}
