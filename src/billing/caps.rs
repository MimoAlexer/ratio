use std::collections::HashMap;

use axum::{Form, extract::State, response::Redirect};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, store};

/// Called with the workspace quota lock, in the same transaction as the effective plan change.
pub(super) async fn enforce(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    plan: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"WITH excess AS (
            SELECT p.id FROM projects p
            LEFT JOIN billing_preferences pick
                ON pick.owner_id = p.owner_id AND pick.resource_type = 'project' AND pick.resource_id = p.id
            WHERE p.owner_id = $1 AND NOT p.archived
            ORDER BY (pick.resource_id IS NOT NULL) DESC, EXISTS (
                SELECT FROM github_repos r
                JOIN billing_preferences preferred_repo
                    ON preferred_repo.owner_id = r.owner_id AND preferred_repo.resource_type = 'repo'
                    AND preferred_repo.resource_id = r.id
                WHERE r.owner_id = p.owner_id AND r.project_id = p.id AND NOT r.billing_paused
            ) DESC, p.created_at, p.id
            OFFSET $2
        )
        UPDATE projects SET archived = true
        WHERE owner_id = $1 AND id IN (SELECT id FROM excess)"#,
    )
    .bind(owner)
    .bind(store::limits(plan).projects)
    .execute(&mut **tx)
    .await?;

    enforce_repo_cap(tx, owner, plan).await
}

/// Called after changing project archival or the workspace plan, under the quota lock.
pub(crate) async fn enforce_repo_cap(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    plan: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"WITH kept AS (
            SELECT r.id FROM github_repos r
            JOIN projects p ON p.owner_id = r.owner_id AND p.id = r.project_id
            LEFT JOIN billing_preferences pick
                ON pick.owner_id = r.owner_id AND pick.resource_type = 'repo' AND pick.resource_id = r.id
            WHERE r.owner_id = $1 AND NOT r.billing_paused AND NOT p.archived
            ORDER BY (pick.resource_id IS NOT NULL) DESC, r.repo_id, r.id
            LIMIT $2
        )
        UPDATE github_repos SET billing_paused = true, repo_grant_until = NULL,
            alert_grant_until = NULL, lease_until = NULL, lease_id = NULL,
            generation = gen_random_uuid(), next_sync_at = now()
        WHERE owner_id = $1 AND NOT billing_paused AND id NOT IN (SELECT id FROM kept)"#,
    )
    .bind(owner)
    .bind(store::limits(plan).repos)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn selection(
    State(state): State<AppState>,
    auth: Auth,
    Form(mut fields): Form<HashMap<String, String>>,
) -> AppResult<Redirect> {
    auth.check_csrf(&fields.remove("csrf").unwrap_or_default())?;
    let mut projects = Vec::new();
    let mut repos = Vec::new();
    for (field, value) in fields {
        let invalid = || AppError::BadRequest("Invalid billing selection.".into());
        if value != "on" {
            return Err(invalid());
        }
        let (resource, id) = field.split_once('_').ok_or_else(invalid)?;
        let id = Uuid::parse_str(id).map_err(|_| invalid())?;
        match resource {
            "project" => projects.push(id),
            "repo" => repos.push(id),
            _ => return Err(invalid()),
        }
    }

    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(auth.user_id)
        .fetch_one(&mut *tx)
        .await?;
    save_preferences(&mut tx, auth.user_id, &projects, &repos).await?;
    tx.commit().await?;
    Ok(Redirect::to("/billing"))
}

async fn save_preferences(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    projects: &[Uuid],
    repos: &[Uuid],
) -> AppResult<()> {
    let valid: bool = sqlx::query_scalar(
        r#"SELECT (SELECT count(*) FROM projects
            WHERE owner_id = $1 AND NOT archived AND id = ANY($2::uuid[])) = cardinality($2::uuid[])
        AND (SELECT count(*) FROM github_repos r
            JOIN projects p ON p.owner_id = r.owner_id AND p.id = r.project_id
            WHERE r.owner_id = $1 AND NOT r.billing_paused AND NOT p.archived
                AND r.id = ANY($3::uuid[])) = cardinality($3::uuid[])"#,
    )
    .bind(owner)
    .bind(projects)
    .bind(repos)
    .fetch_one(&mut **tx)
    .await?;
    if !valid {
        return Err(AppError::BadRequest(
            "Choose active projects and repositories from your workspace.".into(),
        ));
    }

    sqlx::query("DELETE FROM billing_preferences WHERE owner_id = $1")
        .bind(owner)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        r#"INSERT INTO billing_preferences (owner_id, resource_type, resource_id)
        SELECT $1::uuid, 'project', id FROM unnest($2::uuid[]) AS p(id)
        UNION ALL
        SELECT $1::uuid, 'repo', id FROM unnest($3::uuid[]) AS r(id)"#,
    )
    .bind(owner)
    .bind(projects)
    .bind(repos)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn preferred_repo_keeps_its_project_and_downgrade_invalidates_excess_grants(
        db: PgPool,
    ) -> AppResult<()> {
        let mut tx = db.begin().await?;
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users (email) VALUES ('owner@billing.test') RETURNING id",
        )
        .fetch_one(&mut *tx)
        .await?;
        let other: Uuid = sqlx::query_scalar(
            "INSERT INTO users (email) VALUES ('other@billing.test') RETURNING id",
        )
        .fetch_one(&mut *tx)
        .await?;
        let mut project_ids = Vec::new();
        for (workspace, key, days) in [
            (owner, "OLD", 4_i32),
            (owner, "KEEP", 1),
            (other, "OTHER", 3),
        ] {
            let id: Uuid = sqlx::query_scalar("INSERT INTO projects (owner_id,name,key,created_at) VALUES ($1,$2,$2,now()-make_interval(days=>$3)) RETURNING id")
                .bind(workspace).bind(key).bind(days).fetch_one(&mut *tx).await?;
            project_ids.push(id);
        }
        sqlx::query("INSERT INTO github_connections(owner_id,user_remote_id,access_cipher) VALUES ($1,5,'fixture'),($2,6,'fixture')")
            .bind(owner).bind(other).execute(&mut *tx).await?;
        let mut repo_ids = Vec::new();
        for (workspace, index, repo) in [
            (owner, 0, 10_i64),
            (owner, 1, 11),
            (owner, 1, 12),
            (other, 2, 13),
        ] {
            let id: Uuid = sqlx::query_scalar("INSERT INTO github_repos(owner_id,project_id,repo_id,installation_id,repo_grant_until,alert_grant_until,lease_until,lease_id) VALUES ($1,$2,$3,1,now()+interval '1 hour',now()+interval '1 hour',now()+interval '1 minute',gen_random_uuid()) RETURNING id")
                .bind(workspace).bind(project_ids[index]).bind(repo).fetch_one(&mut *tx).await?;
            repo_ids.push(id);
        }
        save_preferences(&mut tx, owner, &[], &[repo_ids[2]]).await?;
        assert!(
            save_preferences(&mut tx, owner, &[project_ids[2]], &[])
                .await
                .is_err()
        );
        assert!(
            save_preferences(&mut tx, owner, &[], &[repo_ids[3]])
                .await
                .is_err()
        );

        let before: Uuid = sqlx::query_scalar("SELECT generation FROM github_repos WHERE id=$1")
            .bind(repo_ids[0])
            .fetch_one(&mut *tx)
            .await?;
        enforce(&mut tx, owner, "free").await?;
        let active: Vec<String> =
            sqlx::query_scalar("SELECT key FROM projects WHERE NOT archived ORDER BY key")
                .fetch_all(&mut *tx)
                .await?;
        assert_eq!(active, ["KEEP", "OTHER"]);
        let syncing: Vec<i64> = sqlx::query_scalar(
            "SELECT repo_id FROM github_repos WHERE NOT billing_paused ORDER BY repo_id",
        )
        .fetch_all(&mut *tx)
        .await?;
        assert_eq!(syncing, [12, 13]);
        let stale: i64 = sqlx::query_scalar("SELECT count(*) FROM github_repos WHERE billing_paused AND (repo_grant_until IS NOT NULL OR alert_grant_until IS NOT NULL OR lease_until IS NOT NULL OR lease_id IS NOT NULL)")
            .fetch_one(&mut *tx).await?;
        assert_eq!(stale, 0);
        let after: Uuid = sqlx::query_scalar("SELECT generation FROM github_repos WHERE id=$1")
            .bind(repo_ids[0])
            .fetch_one(&mut *tx)
            .await?;
        assert_ne!(before, after);
        tx.rollback().await?;
        Ok(())
    }
}
