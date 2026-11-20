use super::{
    configured, credentials,
    provider::{Alert, Provider},
};
use crate::{AppError, AppResult, AppState, auth::Auth};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use std::time::Duration;
use uuid::Uuid;

#[derive(Deserialize)]
struct RemoteId {
    id: i64,
    app_id: Option<i64>,
}

// Parse only routing identifiers. Incoming findings, pull request text and credentials
// do not enter the database or logs. The worker reads current state from GitHub.
#[derive(Deserialize)]
struct Envelope {
    installation: Option<RemoteId>,
    repository: Option<RemoteId>,
    sender: Option<RemoteId>,
    action: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Change {
    UserRevoked(i64),
    Uninstalled(i64),
    Repositories {
        installation: i64,
        repo: Option<i64>,
    },
    Ignore,
}

fn malformed() -> AppError {
    AppError::BadRequest("Invalid GitHub webhook".into())
}

fn change(event: &str, bytes: &[u8]) -> AppResult<Change> {
    let payload: Envelope = serde_json::from_slice(bytes).map_err(|_| malformed())?;
    let id = |v: Option<RemoteId>| v.map(|r| r.id).filter(|id| *id > 0).ok_or_else(malformed);
    match event {
        "github_app_authorization"
            if payload.action.as_deref() == Some("revoked")
                && payload.installation.is_none()
                && payload.repository.is_none() =>
        {
            Ok(Change::UserRevoked(id(payload.sender)?))
        }
        // GitHub signs the body, not the event headers. A full installation object
        // distinguishes this terminal event from a repository event with action "deleted".
        "installation"
            if payload.action.as_deref() == Some("deleted")
                && payload
                    .installation
                    .as_ref()
                    .and_then(|i| i.app_id)
                    .is_some_and(|id| id > 0) =>
        {
            Ok(Change::Uninstalled(id(payload.installation)?))
        }
        // A switch from all repositories to a selection can send an empty
        // repositories_removed array. Recheck every bound repository in this installation.
        "installation"
        | "installation_repositories"
        | "installation_target"
        | "membership"
        | "organization" => Ok(Change::Repositories {
            installation: id(payload.installation)?,
            repo: None,
        }),
        "repository" | "dependabot_alert" | "member" | "security_and_analysis" => {
            Ok(Change::Repositories {
                installation: id(payload.installation)?,
                repo: Some(id(payload.repository)?),
            })
        }
        _ => Ok(Change::Ignore),
    }
}

pub(super) async fn webhook(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<StatusCode> {
    let provider = configured(&s)?;
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let signature = header("x-hub-signature-256").ok_or(AppError::Forbidden)?;
    if !provider.valid_webhook(&body, signature) {
        return Err(AppError::Forbidden);
    }
    let delivery = header("x-github-delivery")
        .and_then(|v| Uuid::parse_str(v).ok())
        .ok_or_else(malformed)?;
    let change = change(header("x-github-event").ok_or_else(malformed)?, &body)?;
    if change != Change::Ignore {
        accept(&s.db, delivery, change).await?;
    }
    Ok(StatusCode::ACCEPTED)
}

async fn accept(db: &PgPool, delivery: Uuid, change: Change) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let inserted =
        sqlx::query("INSERT INTO github_deliveries(delivery) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(delivery)
            .execute(&mut *tx)
            .await?;
    if inserted.rows_affected() == 1 {
        match change {
            Change::UserRevoked(remote_id) => {
                // The connection owns only provider data. Its foreign keys remove
                // tokens, bindings and cached findings while native issues survive.
                sqlx::query("DELETE FROM github_connections WHERE user_remote_id=$1")
                    .bind(remote_id)
                    .execute(&mut *tx)
                    .await?;
            }
            Change::Uninstalled(installation) => {
                sqlx::query("DELETE FROM github_repos WHERE installation_id=$1")
                    .bind(installation)
                    .execute(&mut *tx)
                    .await?;
            }
            Change::Repositories { installation, repo } => {
                sqlx::query(
                    "UPDATE github_repos SET generation=gen_random_uuid(), \
                     lease_until=NULL, lease_id=NULL, repo_grant_until=NULL, \
                     alert_grant_until=NULL, next_sync_at=now() \
                     WHERE installation_id=$1 AND ($2::bigint IS NULL OR repo_id=$2)",
                )
                .bind(installation)
                .bind(repo)
                .execute(&mut *tx)
                .await?;
            }
            Change::Ignore => {}
        }
    }
    tx.commit().await?;
    Ok(())
}

pub(super) async fn manual(s: &AppState, auth: &Auth, id: i64) -> AppResult<()> {
    manual_for_owner(&s.db, auth.user_id, id).await
}

async fn manual_for_owner(db: &PgPool, owner: Uuid, id: i64) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let project: Uuid = sqlx::query_scalar(
        "SELECT project_id FROM github_repos WHERE owner_id=$1 AND repo_id=$2 AND NOT billing_paused",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    crate::store::lock_project(&mut tx, owner, project).await?;
    let result = sqlx::query(
        "UPDATE github_repos SET last_manual=now(), next_sync_at=now(), \
         lease_id=NULL, lease_until=NULL \
         WHERE owner_id=$1 AND repo_id=$2 AND project_id=$3 AND NOT billing_paused \
         AND (last_manual IS NULL OR last_manual < now() - interval '1 minute')",
    )
    .bind(owner)
    .bind(id)
    .bind(project)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::Conflict(
            "Refresh is available once per minute.".into(),
        ));
    }
    tx.commit().await?;
    Ok(())
}

#[derive(FromRow)]
struct Job {
    owner_id: Uuid,
    repo_id: i64,
    installation_id: i64,
    generation: Uuid,
    lease_id: Uuid,
    checked_from: DateTime<Utc>,
    sweep: bool,
}

async fn claim(db: &PgPool) -> AppResult<Option<Job>> {
    Ok(sqlx::query_as(
        "WITH picked AS ( \
             SELECT r.owner_id, r.repo_id, (r.next_sync_at <= now() OR r.alert_grant_until IS NULL) AS sweep \
             FROM github_repos r JOIN projects p ON p.id=r.project_id AND p.owner_id=r.owner_id \
             WHERE NOT r.billing_paused AND NOT p.archived \
             AND (r.lease_until IS NULL OR r.lease_until <= now()) \
             AND (r.next_sync_at <= now() OR (r.failures=0 AND r.last_success IS NOT NULL \
                   AND (r.alert_grant_until IS NULL OR r.alert_grant_until < now()+interval '1 minute'))) \
             ORDER BY least(r.next_sync_at, coalesce(r.alert_grant_until, r.next_sync_at)) \
             LIMIT 1 FOR UPDATE OF r SKIP LOCKED \
         ) UPDATE github_repos r SET lease_id=gen_random_uuid(), \
             lease_until=now()+interval '3 minutes', last_attempt=now() FROM picked \
         WHERE r.owner_id=picked.owner_id AND r.repo_id=picked.repo_id \
         RETURNING r.owner_id, r.repo_id, r.installation_id, r.generation, \
                   r.lease_id, r.last_attempt AS checked_from, picked.sweep",
    )
    .fetch_optional(db)
    .await?)
}

enum Check {
    Authorized,
    Sweep(Vec<Alert>),
    Denied,
    Unavailable,
}

impl Check {
    fn failed(error: AppError) -> Self {
        match error {
            AppError::Forbidden | AppError::NotFound => Self::Denied,
            _ => Self::Unavailable,
        }
    }
}

struct Observation {
    repo_name: Option<String>,
    alerts: Check,
}

impl Observation {
    fn without_repository(alerts: Check) -> Self {
        Self {
            repo_name: None,
            alerts,
        }
    }
}

async fn observe(s: &AppState, provider: &Provider, job: &Job) -> Observation {
    let token = match credentials(s, provider, job.owner_id).await {
        Ok(token) => token,
        Err(error) => return Observation::without_repository(Check::failed(error)),
    };
    // Repository visibility can survive the loss of permission for private alerts.
    // Verify both independently using this workspace owner's GitHub user token.
    let repo = match provider
        .repository(&token, job.installation_id, job.repo_id)
        .await
    {
        Ok(Some(repo)) => repo,
        Ok(None) => return Observation::without_repository(Check::Denied),
        Err(error) => return Observation::without_repository(Check::failed(error)),
    };
    let result = if job.sweep {
        provider.alerts(&token, &repo).await.map(Check::Sweep)
    } else {
        provider
            .alert_access(&token, &repo)
            .await
            .map(|()| Check::Authorized)
    };
    Observation {
        repo_name: Some(repo.name),
        alerts: result.unwrap_or_else(Check::failed),
    }
}

async fn write_alerts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    job: &Job,
    alerts: &[Alert],
) -> AppResult<()> {
    sqlx::query("DELETE FROM github_alerts WHERE owner_id=$1 AND repo_id=$2")
        .bind(job.owner_id)
        .bind(job.repo_id)
        .execute(&mut **tx)
        .await?;
    if !alerts.is_empty() {
        sqlx::query(
            "INSERT INTO github_alerts(owner_id,repo_id,number,package,manifest,patched,updated_at) \
             SELECT $1,$2,* FROM unnest($3::bigint[],$4::text[],$5::text[],$6::text[],$7::timestamptz[])",
        )
        .bind(job.owner_id)
        .bind(job.repo_id)
        .bind(alerts.iter().map(|a| a.number).collect::<Vec<_>>())
        .bind(alerts.iter().map(|a| &a.package).collect::<Vec<_>>())
        .bind(alerts.iter().map(|a| &a.manifest).collect::<Vec<_>>())
        .bind(alerts.iter().map(|a| a.patched.as_deref()).collect::<Vec<_>>())
        .bind(alerts.iter().map(|a| a.updated_at).collect::<Vec<_>>())
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn finish(db: &PgPool, job: &Job, observation: Observation) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let current: Option<Uuid> = sqlx::query_scalar(
        "SELECT generation FROM github_repos r WHERE owner_id=$1 AND repo_id=$2 \
         AND generation=$3 AND lease_id=$4 AND lease_until>clock_timestamp() AND NOT billing_paused \
         AND EXISTS (SELECT 1 FROM projects p WHERE p.id=r.project_id AND p.owner_id=r.owner_id AND NOT p.archived) \
         FOR UPDATE OF r",
    )
    .bind(job.owner_id).bind(job.repo_id).bind(job.generation).bind(job.lease_id)
    .fetch_optional(&mut *tx).await?;
    if current.is_none() {
        return Ok(());
    }
    let complete = matches!(observation.alerts, Check::Sweep(_));
    let allowed = matches!(observation.alerts, Check::Sweep(_) | Check::Authorized);
    let denied = matches!(observation.alerts, Check::Denied);
    let repo_denied =
        observation.repo_name.is_none() && matches!(observation.alerts, Check::Denied);
    match &observation.alerts {
        Check::Sweep(alerts) => write_alerts(&mut tx, job, alerts).await?,
        Check::Denied => write_alerts(&mut tx, job, &[]).await?,
        _ => {}
    }
    // Grants expire five minutes from the start of the check. A slow multi-page
    // sweep cannot extend a stale observation by measuring from completion.
    sqlx::query(
        "UPDATE github_repos SET \
          generation=CASE WHEN $9 THEN gen_random_uuid() ELSE generation END, \
          display_name=CASE WHEN $8 THEN '' ELSE coalesce($3, display_name) END, \
          repo_grant_until=CASE WHEN $3::text IS NOT NULL THEN $4::timestamptz+interval '5 minutes' END, \
          alert_grant_until=CASE WHEN $5 THEN $4::timestamptz+interval '5 minutes' END, \
          last_success=CASE WHEN $6 THEN now() ELSE last_success END, \
          failures=CASE WHEN $5 THEN 0 ELSE least(failures+1, 20) END, \
          next_sync_at=CASE \
            WHEN $6 THEN now()+interval '55 minutes'+random()*interval '10 minutes' \
            WHEN $5 THEN next_sync_at \
            ELSE now()+(least(3600., 30.*power(2.,least(failures,7))) + random()*15.)*interval '1 second' END, \
          lease_id=NULL, lease_until=NULL \
         WHERE owner_id=$1 AND repo_id=$2 AND lease_id=$7 AND NOT billing_paused",
    )
    .bind(job.owner_id).bind(job.repo_id).bind(observation.repo_name).bind(job.checked_from)
    .bind(allowed).bind(complete).bind(job.lease_id).bind(repo_denied).bind(denied).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn poll(s: &AppState, provider: &Provider) -> AppResult<bool> {
    let Some(job) = claim(&s.db).await? else {
        return Ok(false);
    };
    let result = tokio::time::timeout(Duration::from_secs(150), observe(s, provider, &job))
        .await
        .unwrap_or_else(|_| Observation::without_repository(Check::Unavailable));
    finish(&s.db, &job, result).await?;
    Ok(true)
}

pub(super) async fn run(s: AppState) {
    let Ok(provider) = configured(&s) else { return };
    let mut prune = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
    let reconciliation = || async {
        loop {
            match poll(&s, &provider).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(_) => tracing::warn!("GitHub reconciliation temporarily unavailable"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    };
    let cleanup = async {
        loop {
            prune.tick().await;
            if sqlx::query(
                "DELETE FROM github_deliveries WHERE received_at < now()-interval '30 days'",
            )
            .execute(&s.db)
            .await
            .is_err()
            {
                tracing::warn!("GitHub delivery receipt cleanup temporarily unavailable");
            }
        }
    };
    // A slow repository must not stall grants and webhook sweeps for every owner.
    tokio::join!(
        reconciliation(),
        reconciliation(),
        reconciliation(),
        cleanup
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changing_selection_invalidates_an_entire_installation_even_when_no_removed_ids_arrive() {
        let data = br#"{"action":"removed","installation":{"id":24},"repositories_removed":[],"repositories_added":[]}"#;
        assert_eq!(
            change("installation_repositories", data).unwrap(),
            Change::Repositories {
                installation: 24,
                repo: None
            }
        );
    }

    #[test]
    fn critical_alert_events_route_by_stable_repo_and_installation_only() {
        let data = br#"{"action":"reopened","repository":{"id":13},"installation":{"id":24},"alert":{"number":300,"state":"fixed"}}"#;
        assert_eq!(
            change("dependabot_alert", data).unwrap(),
            Change::Repositories {
                installation: 24,
                repo: Some(13)
            }
        );
        assert!(change("dependabot_alert", br#"{"repository":{"id":13}}"#).is_err());
        assert!(
            change(
                "dependabot_alert",
                br#"{"installation":{"id":21},"repository":{"id":0}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn authorization_revoked_targets_only_that_github_identity() {
        let body = br#"{"action":"revoked","sender":{"id":96}}"#;
        assert_eq!(
            change("github_app_authorization", body).unwrap(),
            Change::UserRevoked(96)
        );
        assert_eq!(change("ping", br#"{"zen":"hi"}"#).unwrap(), Change::Ignore);
    }

    async fn linked_repo(db: &PgPool) -> Uuid {
        let owner = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES ('worker@example.test') RETURNING id",
        )
        .fetch_one(db)
        .await
        .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Garden','GR') RETURNING id",
        )
        .bind(owner)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO github_connections(owner_id,user_remote_id,access_cipher) VALUES ($1,96,$2)")
            .bind(owner).bind(&[0u8][..]).execute(db).await.unwrap();
        sqlx::query("INSERT INTO github_repos(owner_id,repo_id,installation_id,project_id) VALUES ($1,13,24,$2)")
            .bind(owner).bind(project).execute(db).await.unwrap();
        owner
    }

    fn sample_sweep() -> Observation {
        Observation {
            repo_name: Some("Octo/Garden".into()),
            alerts: Check::Sweep(vec![Alert {
                number: 20,
                package: "dependency".into(),
                manifest: "Cargo.toml".into(),
                patched: Some("2.0".into()),
                updated_at: Utc::now(),
            }]),
        }
    }

    #[sqlx::test]
    async fn duplicate_webhooks_do_not_requeue_and_old_workers_cannot_restore_a_revoked_grant(
        db: PgPool,
    ) {
        linked_repo(&db).await;
        let old_job = claim(&db).await.unwrap().unwrap();
        assert!(claim(&db).await.unwrap().is_none());
        let delivery = Uuid::new_v4();
        let (a, b) = tokio::join!(
            accept(
                &db,
                delivery,
                Change::Repositories {
                    installation: 24,
                    repo: Some(13)
                }
            ),
            accept(
                &db,
                delivery,
                Change::Repositories {
                    installation: 24,
                    repo: Some(13)
                }
            )
        );
        a.unwrap();
        b.unwrap();
        finish(&db, &old_job, sample_sweep()).await.unwrap();
        let pending: (bool, i64) = sqlx::query_as("SELECT alert_grant_until IS NULL, (SELECT count(*) FROM github_alerts) FROM github_repos")
            .fetch_one(&db).await.unwrap();
        assert_eq!(pending, (true, 0));
        let job = claim(&db).await.unwrap().unwrap();
        assert_ne!(job.generation, old_job.generation);
        finish(&db, &job, sample_sweep()).await.unwrap();
        accept(
            &db,
            delivery,
            Change::Repositories {
                installation: 24,
                repo: Some(13),
            },
        )
        .await
        .unwrap();
        assert!(claim(&db).await.unwrap().is_none());
        let verified: (bool, i64, i64) = sqlx::query_as(
            "SELECT alert_grant_until BETWEEN now() AND last_attempt+interval '5 minutes',              (SELECT count(*) FROM github_alerts), (SELECT count(*) FROM github_deliveries) FROM github_repos",
        ).fetch_one(&db).await.unwrap();
        assert_eq!(verified, (true, 1, 1));
    }

    #[sqlx::test]
    async fn interrupted_sweep_preserves_last_complete_snapshot_and_permission_loss_conceals_only_matching_data(
        db: PgPool,
    ) {
        linked_repo(&db).await;
        let first = claim(&db).await.unwrap().unwrap();
        finish(&db, &first, sample_sweep()).await.unwrap();
        let last_complete: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_success FROM github_repos")
                .fetch_one(&db)
                .await
                .unwrap();
        sqlx::query("UPDATE github_repos SET next_sync_at=now()")
            .execute(&db)
            .await
            .unwrap();
        let second = claim(&db).await.unwrap().unwrap();
        finish(
            &db,
            &second,
            Observation {
                repo_name: Some("Octo/Garden".into()),
                alerts: Check::Unavailable,
            },
        )
        .await
        .unwrap();
        let degraded: (bool,bool, DateTime<Utc>, i64) = sqlx::query_as(
            "SELECT repo_grant_until > now(), alert_grant_until IS NULL,last_success,              (SELECT count(*) FROM github_alerts) FROM github_repos",
        ).fetch_one(&db).await.unwrap();
        assert_eq!(degraded, (true, true, last_complete, 1));
        assert!(claim(&db).await.unwrap().is_none());
        accept(
            &db,
            Uuid::new_v4(),
            Change::Repositories {
                installation: 24,
                repo: None,
            },
        )
        .await
        .unwrap();
        let third = claim(&db).await.unwrap().unwrap();
        finish(
            &db,
            &third,
            Observation {
                repo_name: Some("Octo/Garden".into()),
                alerts: Check::Denied,
            },
        )
        .await
        .unwrap();
        let revoked: (bool,bool, DateTime<Utc>, i64) = sqlx::query_as(
            "SELECT repo_grant_until > now(), alert_grant_until IS NULL,last_success,              (SELECT count(*) FROM github_alerts) FROM github_repos",
        ).fetch_one(&db).await.unwrap();
        assert_eq!(revoked, (true, true, last_complete, 0));
        accept(&db, Uuid::new_v4(), Change::UserRevoked(96))
            .await
            .unwrap();
        let bindings: i64 = sqlx::query_scalar("SELECT count(*) FROM github_repos")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(bindings, 0);
    }

    #[sqlx::test]
    async fn uninstall_preserves_other_installations_and_stops_work_for_deleted_bindings(
        db: PgPool,
    ) {
        let owner = linked_repo(&db).await;
        sqlx::query("INSERT INTO github_repos(owner_id,repo_id,installation_id,project_id) SELECT owner_id,14,25,project_id FROM github_repos WHERE owner_id=$1")
            .bind(owner).execute(&db).await.unwrap();
        let removed = change(
            "installation",
            br#"{"action":"deleted","installation":{"id":24,"app_id":99}}"#,
        )
        .unwrap();
        assert_eq!(removed, Change::Uninstalled(24));
        assert_ne!(
            change(
                "installation",
                br#"{"action":"deleted","installation":{"id":24},"repository":{"id":13}}"#
            )
            .unwrap(),
            removed
        );
        accept(&db, Uuid::new_v4(), removed).await.unwrap();
        let remaining: Vec<i64> = sqlx::query_scalar("SELECT repo_id FROM github_repos")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(remaining, vec![14]);
        assert_eq!(claim(&db).await.unwrap().unwrap().installation_id, 25);
    }

    #[sqlx::test]
    async fn manual_sweeps_are_scoped_rate_limited_and_paused_with_billing(db: PgPool) {
        let owner = linked_repo(&db).await;
        let stranger = Uuid::new_v4();
        assert!(matches!(
            manual_for_owner(&db, stranger, 13).await,
            Err(AppError::NotFound)
        ));
        let (first, second) = tokio::join!(
            manual_for_owner(&db, owner, 13),
            manual_for_owner(&db, owner, 13)
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert!(
            matches!(first, Err(AppError::Conflict(_)))
                || matches!(second, Err(AppError::Conflict(_)))
        );
        sqlx::query("UPDATE github_repos SET billing_paused=true,last_manual=NULL")
            .execute(&db)
            .await
            .unwrap();
        assert!(matches!(
            manual_for_owner(&db, owner, 13).await,
            Err(AppError::NotFound)
        ));
        assert!(claim(&db).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn worker_uses_the_owners_token_and_keeps_a_grant_probe_separate_from_a_full_sweep(
        db: PgPool,
    ) {
        use axum::{Json, Router, extract::Request, response::IntoResponse};
        use serde_json::json;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url =
            reqwest::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let provider = Provider::fixture_at(&url);
        let sweeps = Arc::new(AtomicUsize::new(0));
        let probes = Arc::new(AtomicUsize::new(0));
        let count_sweeps = sweeps.clone();
        let count_probes = probes.clone();
        let api = Router::new().fallback(move |request: Request| {
            let sweeps = count_sweeps.clone();
            let probes = count_probes.clone();
            async move {
                if request.headers().get("authorization").and_then(|h|h.to_str().ok()) != Some("Bearer owner-fixture-token") {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                let data = match request.uri().path() {
                    "/user/installations" => json!({"installations":[{
                        "id":24, "app_slug":"workspace", "permissions":{"vulnerability_alerts":"read"}
                    }]}),
                    "/user/installations/24/repositories" => json!({"repositories":[{"id":13,"full_name":"Octo/Garden"}]}),
                    "/repos/Octo/Garden/dependabot/alerts" => {
                        let probe=request.uri().query().unwrap_or_default().split('&').any(|q|q=="per_page=1");
                        if probe {
                            probes.fetch_add(1,Ordering::SeqCst);
                            json!([])
                        } else if sweeps.fetch_add(1,Ordering::SeqCst)==0 {
                            json!([{"number":20,"state":"open","dependency":{"package":{"name":"fixture-lib"},"manifest_path":"Cargo.toml"},
                                "security_vulnerability":{"severity":"critical","first_patched_version":{"identifier":"2.0"}},
                                "updated_at":"2026-11-20T10:00:00Z"}])
                        } else {
                            json!([])
                        }
                    },
                    _ => return StatusCode::NOT_FOUND.into_response(),
                };
                Json(data).into_response()
            }
        });
        let server = tokio::spawn(async move {
            axum::serve(listener, api).await.unwrap();
        });
        let owner = linked_repo(&db).await;
        let cipher = provider.seal(owner, "owner-fixture-token").unwrap();
        sqlx::query("UPDATE github_connections SET access_cipher=$2 WHERE owner_id=$1")
            .bind(owner)
            .bind(cipher)
            .execute(&db)
            .await
            .unwrap();
        let state = AppState::for_test(db.clone());
        assert!(poll(&state, &provider).await.unwrap());
        let saved: (String, String) = sqlx::query_as("SELECT package,manifest FROM github_alerts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(saved, ("fixture-lib".into(), "Cargo.toml".into()));
        sqlx::query("UPDATE github_repos SET alert_grant_until=now()+interval '10 seconds'")
            .execute(&db)
            .await
            .unwrap();
        assert!(poll(&state, &provider).await.unwrap());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM github_alerts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        accept(
            &db,
            Uuid::new_v4(),
            Change::Repositories {
                installation: 24,
                repo: Some(13),
            },
        )
        .await
        .unwrap();
        assert!(poll(&state, &provider).await.unwrap());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM github_alerts")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(sweeps.load(Ordering::SeqCst), 2);
        server.abort();
    }
}
