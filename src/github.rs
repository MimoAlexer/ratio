#[cfg(test)]
mod authorization_tests;
mod cards;
#[cfg(test)]
mod dbtests;
mod jobs;
mod provider;
mod references;
mod settings;

use axum::{
    Form, Router,
    extract::{Path, Query, State},
    response::Redirect,
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use maud::Markup;
use rand::{RngCore, rngs::OsRng};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, store};
use provider::{Provider, Repo, Token};

pub use cards::{coverage, issue_panel, project_home, project_home_parts, resume_pr};

pub async fn home(s: &AppState, auth: &Auth) -> Markup {
    project_home(s, auth, None).await
}

pub async fn run(s: AppState) {
    if configured(&s).is_ok() {
        jobs::run(s).await;
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(cards::routes())
        .merge(references::routes())
        .merge(settings::routes())
        .route("/integrations/github/connect", post(begin))
        .route("/integrations/github/callback", get(callback))
        .route("/integrations/github/link", post(link))
        .route("/integrations/github/repositories/{id}/map", post(remap))
        .route(
            "/integrations/github/repositories/{id}/refresh",
            post(refresh),
        )
        .route(
            "/integrations/github/repositories/{id}/remove",
            post(remove),
        )
        .route("/integrations/github/disconnect", post(disconnect))
        .route(
            "/integrations/github/webhook",
            post(jobs::webhook).layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024)),
        )
}

pub fn enabled(s: &AppState) -> bool {
    configured(s).is_ok()
}

fn configured(s: &AppState) -> AppResult<Provider> {
    Provider::configured(&s.http, &s.config.base_url).ok_or_else(|| {
        AppError::Unavailable("GitHub is awaiting configuration on this server.".into())
    })
}

#[derive(Deserialize)]
struct Command {
    csrf: String,
}

fn hash(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

async fn begin(
    State(s): State<AppState>,
    auth: Auth,
    Form(cmd): Form<Command>,
) -> AppResult<Redirect> {
    auth.check_csrf(&cmd.csrf)?;
    let provider = configured(&s)?;
    let nonce = oauth_state(&s.db, &auth).await?;
    Ok(Redirect::to(&provider.authorize_url(&nonce)))
}

async fn oauth_state(db: &PgPool, auth: &Auth) -> AppResult<String> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let nonce = hex::encode(bytes);
    sqlx::query(
        "INSERT INTO github_oauth_states(owner_id,state_hash,session_hash,expires_at)
         VALUES($1,$2,$3,now() + interval '10 minutes') ON CONFLICT(owner_id)
         DO UPDATE SET state_hash=EXCLUDED.state_hash,session_hash=EXCLUDED.session_hash,
             expires_at=EXCLUDED.expires_at,started=false",
    )
    .bind(auth.user_id)
    .bind(hash(&nonce))
    .bind(hash(&auth.csrf))
    .execute(db)
    .await?;
    Ok(nonce)
}

#[derive(Deserialize)]
struct Callback {
    state: String,
    code: Option<String>,
}

async fn callback(
    State(s): State<AppState>,
    auth: Auth,
    Query(q): Query<Callback>,
) -> AppResult<Redirect> {
    let provider = configured(&s)?;
    authorize_callback(&s, &provider, &auth, q).await
}

async fn authorize_callback(
    s: &AppState,
    p: &Provider,
    auth: &Auth,
    q: Callback,
) -> AppResult<Redirect> {
    if q.state.len() != 64 || !q.state.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::Forbidden);
    }
    let digest = hash(&q.state);
    let session = hash(&auth.csrf);
    let consumed = sqlx::query(
        "UPDATE github_oauth_states SET started=true WHERE owner_id=$1 AND state_hash=$2
         AND session_hash=$3 AND expires_at>now() AND NOT started",
    )
    .bind(auth.user_id)
    .bind(&digest)
    .bind(&session)
    .execute(&s.db)
    .await?;
    if consumed.rows_affected() != 1 {
        return Err(AppError::Forbidden);
    }
    let code = q
        .code
        .filter(|c| !c.is_empty() && c.len() <= 4096)
        .ok_or_else(|| {
            AppError::BadRequest("GitHub authorization was canceled or expired.".into())
        })?;
    let token = p.exchange(&code, &q.state).await?;
    let remote = p.identity(&token.access).await?;
    let (access, refresh) = encrypted(p, auth.user_id, &token)?;
    let mut tx = s.db.begin().await?;
    owner_lock(&mut tx, auth.user_id).await?;
    // A disconnect or newer authorization attempt while GitHub was responding wins.
    let finished = sqlx::query("DELETE FROM github_oauth_states WHERE owner_id=$1 AND state_hash=$2 AND session_hash=$3 AND started AND expires_at>now()")
        .bind(auth.user_id).bind(digest).bind(session).execute(&mut *tx).await?;
    if finished.rows_affected() != 1 {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM github_connections WHERE owner_id=$1 AND user_remote_id<>$2")
        .bind(auth.user_id)
        .bind(remote)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO github_connections(owner_id,user_remote_id,access_cipher,refresh_cipher,expires_at,refresh_expires_at)
        VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(owner_id) DO UPDATE SET
        access_cipher=EXCLUDED.access_cipher,refresh_cipher=EXCLUDED.refresh_cipher,
        expires_at=EXCLUDED.expires_at,refresh_expires_at=EXCLUDED.refresh_expires_at,grant_id=gen_random_uuid()")
        .bind(auth.user_id).bind(remote).bind(access).bind(refresh).bind(token.expires_at)
        .bind(token.refresh_expires_at).execute(&mut *tx).await?;
    invalidate_grants(&mut tx, auth.user_id).await?;
    tx.commit().await?;
    Ok(Redirect::to("/integrations"))
}

async fn owner_lock(tx: &mut Transaction<'_, Postgres>, owner: Uuid) -> AppResult<String> {
    sqlx::query_scalar("SELECT plan FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)
}

async fn invalidate_grants(tx: &mut Transaction<'_, Postgres>, owner: Uuid) -> AppResult<()> {
    sqlx::query(
        "UPDATE github_repos SET repo_grant_until=NULL,alert_grant_until=NULL,
                 generation=gen_random_uuid(),lease_id=NULL,lease_until=NULL,next_sync_at=now()
                 WHERE owner_id=$1",
    )
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn encrypted(p: &Provider, owner: Uuid, token: &Token) -> AppResult<(Vec<u8>, Option<Vec<u8>>)> {
    Ok((
        p.seal(owner, &token.access)?,
        token
            .refresh
            .as_deref()
            .map(|t| p.seal(owner, t))
            .transpose()?,
    ))
}

#[derive(FromRow)]
struct StoredToken {
    grant_id: Uuid,
    access_cipher: Vec<u8>,
    refresh_cipher: Option<Vec<u8>>,
    expires_at: Option<DateTime<Utc>>,
    refresh_expires_at: Option<DateTime<Utc>>,
}

struct Access {
    value: String,
    grant_id: Uuid,
}

impl std::ops::Deref for Access {
    type Target = str;
    fn deref(&self) -> &str {
        &self.value
    }
}

// GitHub rotates both tokens at once. Serialize refreshes across server processes.
async fn credentials(s: &AppState, p: &Provider, owner: Uuid) -> AppResult<Access> {
    let mut tx = s.db.begin().await?;
    let row: StoredToken = sqlx::query_as(
        "SELECT grant_id,access_cipher,refresh_cipher,expires_at,refresh_expires_at
        FROM github_connections WHERE owner_id=$1 FOR UPDATE",
    )
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::Forbidden)?;
    let now = Utc::now();
    if row
        .expires_at
        .is_none_or(|e| e > now + Duration::seconds(30))
    {
        let access = p.unseal(owner, &row.access_cipher)?;
        tx.commit().await?;
        return Ok(Access {
            value: access,
            grant_id: row.grant_id,
        });
    }
    let refresh = row
        .refresh_cipher
        .filter(|_| row.refresh_expires_at.is_none_or(|t| t > now))
        .ok_or_else(|| {
            AppError::Unavailable("Reconnect GitHub to renew the expired authorization.".into())
        })?;
    let refresh = p.unseal(owner, &refresh)?;
    let token = match p.refresh(&refresh).await {
        Ok(token) => token,
        Err(AppError::Forbidden | AppError::NotFound) => {
            sqlx::query("UPDATE github_connections SET access_cipher=$2,refresh_cipher=NULL,
                    expires_at=now(),refresh_expires_at=now(),grant_id=gen_random_uuid() WHERE owner_id=$1")
                .bind(owner).bind(Vec::<u8>::new()).execute(&mut *tx).await?;
            invalidate_grants(&mut tx, owner).await?;
            tx.commit().await?;
            return Err(AppError::Unavailable(
                "Reconnect GitHub to renew your authorization.".into(),
            ));
        }
        Err(error) => return Err(error),
    };
    let (access, cipher) = encrypted(p, owner, &token)?;
    sqlx::query(
        "UPDATE github_connections SET access_cipher=$2,refresh_cipher=$3,expires_at=$4,
        refresh_expires_at=$5 WHERE owner_id=$1",
    )
    .bind(owner)
    .bind(access)
    .bind(cipher)
    .bind(token.expires_at)
    .bind(token.refresh_expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Access {
        value: token.access,
        grant_id: row.grant_id,
    })
}

async fn known_loss(s: &AppState, owner: Uuid, repo: i64, generation: Uuid) -> AppResult<()> {
    let mut tx = s.db.begin().await?;
    let changed=sqlx::query("UPDATE github_repos SET display_name='',repo_grant_until=NULL,alert_grant_until=NULL,
        generation=gen_random_uuid(),lease_until=NULL,lease_id=NULL,failures=failures+1,next_sync_at=now()
        WHERE owner_id=$1 AND repo_id=$2 AND generation=$3")
        .bind(owner).bind(repo).bind(generation).execute(&mut *tx).await?;
    if changed.rows_affected() == 1 {
        sqlx::query("DELETE FROM github_alerts WHERE owner_id=$1 AND repo_id=$2")
            .bind(owner)
            .bind(repo)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

// Treat every completed installation listing as evidence of removed membership.
// Generation checks prevent a slow response from undoing a subsequent rebind.
async fn catalog(s: &AppState, p: &Provider, owner: Uuid, access: &Access) -> AppResult<Vec<Repo>> {
    let checked = Utc::now();
    let rows:Vec<(i64,i64,Uuid,bool)>=sqlx::query_as(
        "SELECT r.repo_id,r.installation_id,r.generation,r.alert_grant_until IS NOT NULL FROM github_repos r
         JOIN github_connections c USING(owner_id) WHERE r.owner_id=$1 AND c.grant_id=$2
         AND (r.display_name<>'' OR r.alert_grant_until IS NOT NULL OR r.repo_grant_until IS NOT NULL)")
        .bind(owner).bind(access.grant_id).fetch_all(&s.db).await?;
    let outcome = p.repositories(access).await;
    ensure_recent(checked)?;
    let still_current: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT FROM github_connections WHERE owner_id=$1 AND grant_id=$2
          AND (expires_at IS NULL OR expires_at>clock_timestamp()))",
    )
    .bind(owner)
    .bind(access.grant_id)
    .fetch_one(&s.db)
    .await?;
    if !still_current {
        return Err(AppError::Forbidden);
    }
    if matches!(&outcome, Err(AppError::Forbidden | AppError::NotFound)) {
        // An installation listing can lose access on a later page. Hide cached
        // grants until each repository is checked, preserving its last snapshot.
        sqlx::query("UPDATE github_repos r SET repo_grant_until=NULL,alert_grant_until=NULL,
            lease_id=NULL,lease_until=NULL,generation=gen_random_uuid(),next_sync_at=now()
            FROM github_connections c WHERE r.owner_id=$1 AND c.owner_id=r.owner_id AND c.grant_id=$2")
            .bind(owner).bind(access.grant_id).execute(&s.db).await?;
        return outcome;
    }
    for (repo, install, generation, previous_alert_grant) in rows {
        let lost = match &outcome {
            Ok(list) => !list
                .iter()
                .any(|r| r.id == repo && r.installation == install),
            Err(_) => false,
        };
        if lost {
            known_loss(s, owner, repo, generation).await?;
        } else if previous_alert_grant
            && outcome.as_ref().is_ok_and(|items| {
                items.iter().any(|item| {
                    item.id == repo && item.installation == install && !item.can_read_alerts
                })
            })
        {
            sqlx::query("UPDATE github_repos SET alert_grant_until=NULL,lease_id=NULL,lease_until=NULL,
                generation=gen_random_uuid(),next_sync_at=now() WHERE owner_id=$1 AND repo_id=$2 AND generation=$3")
                .bind(owner).bind(repo).bind(generation).execute(&s.db).await?;
        }
    }
    outcome
}

// Verify the current installation and repository membership, even for public repos.
async fn live_repo(
    s: &AppState,
    p: &Provider,
    owner: Uuid,
    access: &Access,
    id: i64,
) -> AppResult<Repo> {
    let (install, generation): (i64, Uuid) = sqlx::query_as(
        "SELECT installation_id,generation FROM github_repos JOIN github_connections USING(owner_id)
        WHERE owner_id=$1 AND repo_id=$2 AND grant_id=$3 AND NOT billing_paused",
    )
    .bind(owner)
    .bind(id).bind(access.grant_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let checked = Utc::now();
    let repo = match p.repository(access, install, id).await {
        Ok(Some(repo)) => repo,
        Ok(None) | Err(AppError::Forbidden | AppError::NotFound) => {
            known_loss(s, owner, id, generation).await?;
            return Err(AppError::NotFound);
        }
        Err(error) => return Err(error),
    };
    ensure_recent(checked)?;
    let updated = sqlx::query(
        "UPDATE github_repos SET display_name=$4,repo_grant_until=$5::timestamptz+interval '4 minutes',
            alert_grant_until=CASE WHEN $6 THEN alert_grant_until END,
            generation=CASE WHEN NOT $6 AND alert_grant_until IS NOT NULL THEN gen_random_uuid() ELSE generation END,
            lease_id=CASE WHEN $6 OR alert_grant_until IS NULL THEN lease_id END,
            lease_until=CASE WHEN $6 OR alert_grant_until IS NULL THEN lease_until END,
            next_sync_at=CASE WHEN NOT $6 AND alert_grant_until IS NOT NULL THEN now() ELSE next_sync_at END
         WHERE owner_id=$1 AND repo_id=$2 AND generation=$3 AND NOT billing_paused")
        .bind(owner).bind(id).bind(generation).bind(&repo.name).bind(checked).bind(repo.can_read_alerts)
        .execute(&s.db).await?;
    if updated.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "Repository access changed. Reload and try again.".into(),
        ));
    }
    Ok(repo)
}

#[derive(Deserialize)]
struct Binding {
    csrf: String,
    project: Uuid,
    repo_id: i64,
    installation_id: i64,
}

async fn grant(db: &PgPool, owner: Uuid) -> AppResult<Uuid> {
    sqlx::query_scalar("SELECT grant_id FROM github_connections WHERE owner_id=$1")
        .bind(owner)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::Forbidden)
}

fn ensure_recent(checked: DateTime<Utc>) -> AppResult<()> {
    if checked + Duration::minutes(4) <= Utc::now() {
        return Err(AppError::Unavailable(
            "Repository verification timed out. Try again.".into(),
        ));
    }
    Ok(())
}

async fn link(
    State(s): State<AppState>,
    auth: Auth,
    Form(f): Form<Binding>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let p = configured(&s)?;
    bind_repo(&s, &p, &auth, f).await
}

async fn bind_repo(s: &AppState, p: &Provider, auth: &Auth, f: Binding) -> AppResult<Redirect> {
    store::writable(&s.db, auth.user_id, f.project).await?;
    let epoch = grant(&s.db, auth.user_id).await?;
    let token = credentials(s, p, auth.user_id).await?;
    let checked = Utc::now();
    let repo = catalog(s, p, auth.user_id, &token)
        .await?
        .into_iter()
        .find(|r| r.id == f.repo_id && r.installation == f.installation_id)
        .ok_or(AppError::Forbidden)?;
    let mut tx = s.db.begin().await?;
    let plan = owner_lock(&mut tx, auth.user_id).await?;
    ensure_recent(checked)?;
    let current = sqlx::query_scalar::<_, Uuid>(
        "SELECT grant_id FROM github_connections WHERE owner_id=$1 FOR SHARE",
    )
    .bind(auth.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if current != Some(epoch) {
        return Err(AppError::Forbidden);
    }
    store::lock_project(&mut tx, auth.user_id, f.project).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT FROM github_repos WHERE owner_id=$1 AND repo_id=$2)",
    )
    .bind(auth.user_id)
    .bind(repo.id)
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        return Err(AppError::Conflict(
            "This repository already has a project mapping. Use its project selector to change it."
                .into(),
        ));
    }
    let used: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM github_repos WHERE owner_id=$1 AND NOT billing_paused",
    )
    .bind(auth.user_id)
    .fetch_one(&mut *tx)
    .await?;
    if used >= store::limits(&plan).repos {
        return Err(AppError::Conflict(
            "Your plan's active repository limit has been reached.".into(),
        ));
    }
    // The seed check runs after commit. An installation removal received during the
    // first binding cannot expose a repository name before that independent check.
    sqlx::query(
        "INSERT INTO github_repos(owner_id,repo_id,installation_id,project_id)
        VALUES($1,$2,$3,$4)",
    )
    .bind(auth.user_id)
    .bind(repo.id)
    .bind(repo.installation)
    .bind(f.project)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Redirect::to("/integrations"))
}

#[derive(Deserialize)]
struct Map {
    csrf: String,
    project: Uuid,
}

async fn remap(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<Map>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let p = configured(&s)?;
    let (remote,install,generation,paused):(i64,i64,Uuid,bool)=sqlx::query_as("SELECT repo_id,installation_id,generation,billing_paused FROM github_repos WHERE owner_id=$1 AND id=$2")
        .bind(auth.user_id).bind(id).fetch_optional(&s.db).await?.ok_or(AppError::NotFound)?;
    let token = credentials(&s, &p, auth.user_id).await?;
    let checked = Utc::now();
    let available = catalog(&s, &p, auth.user_id, &token).await?;
    let Some(repo) = available
        .into_iter()
        .find(|r| r.id == remote && r.installation == install)
    else {
        known_loss(&s, auth.user_id, remote, generation).await?;
        return Err(AppError::Forbidden);
    };
    ensure_recent(checked)?;
    let mut tx = s.db.begin().await?;
    let plan = owner_lock(&mut tx, auth.user_id).await?;
    store::lock_project(&mut tx, auth.user_id, f.project).await?;
    if paused {
        let used: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM github_repos WHERE owner_id=$1 AND NOT billing_paused",
        )
        .bind(auth.user_id)
        .fetch_one(&mut *tx)
        .await?;
        if used >= store::limits(&plan).repos {
            return Err(AppError::Conflict(
                "Your plan's active repository limit has been reached.".into(),
            ));
        }
    }
    let changed=sqlx::query("UPDATE github_repos SET project_id=$3,generation=gen_random_uuid(),lease_until=NULL,
        billing_paused=false,display_name=$5,repo_grant_until=$7::timestamptz+interval '4 minutes',
        lease_id=NULL,next_sync_at=now(),alert_grant_until=NULL WHERE owner_id=$1 AND id=$2 AND generation=$4 AND billing_paused=$6")
        .bind(auth.user_id).bind(id).bind(f.project).bind(generation).bind(repo.name).bind(paused).bind(checked).execute(&mut *tx).await?;
    if changed.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "The repository mapping changed. Reload to continue.".into(),
        ));
    }
    tx.commit().await?;
    Ok(Redirect::to("/integrations"))
}

async fn refresh(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<Command>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    configured(&s)?;
    let remote: i64 = sqlx::query_scalar(
        "SELECT repo_id FROM github_repos WHERE owner_id=$1 AND id=$2 AND NOT billing_paused",
    )
    .bind(auth.user_id)
    .bind(id)
    .fetch_optional(&s.db)
    .await?
    .ok_or(AppError::NotFound)?;
    jobs::manual(&s, &auth, remote).await?;
    Ok(Redirect::to("/integrations"))
}

async fn remove(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(f): Form<Command>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let mut tx = s.db.begin().await?;
    owner_lock(&mut tx, auth.user_id).await?;
    let n = sqlx::query("DELETE FROM github_repos WHERE owner_id=$1 AND id=$2")
        .bind(auth.user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if n.rows_affected() != 1 {
        return Err(AppError::NotFound);
    }
    tx.commit().await?;
    Ok(Redirect::to("/integrations"))
}
async fn disconnect(
    State(s): State<AppState>,
    auth: Auth,
    Form(f): Form<Command>,
) -> AppResult<Redirect> {
    auth.check_csrf(&f.csrf)?;
    let mut tx = s.db.begin().await?;
    owner_lock(&mut tx, auth.user_id).await?;
    sqlx::query("DELETE FROM github_oauth_states WHERE owner_id=$1")
        .bind(auth.user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM github_connections WHERE owner_id=$1")
        .bind(auth.user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Redirect::to("/integrations"))
}
