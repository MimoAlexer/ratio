use crate::{AppError, AppResult};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Clone, Debug, FromRow)]
pub struct Project {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub name: String,
    pub key: String,
    pub archived: bool,
}

pub async fn project(db: &PgPool, owner: Uuid, id: Uuid) -> AppResult<Project> {
    sqlx::query_as("SELECT id,owner_id,name,key,archived FROM projects WHERE owner_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn projects(db: &PgPool, owner: Uuid) -> AppResult<Vec<Project>> {
    Ok(sqlx::query_as("SELECT id,owner_id,name,key,archived FROM projects WHERE owner_id=$1 ORDER BY archived,name,id")
        .bind(owner).fetch_all(db).await?)
}

pub async fn writable(db: &PgPool, owner: Uuid, id: Uuid) -> AppResult<Project> {
    let p = project(db, owner, id).await?;
    if p.archived {
        return Err(AppError::Conflict(
            "This project is archived. Restore it to make changes.".into(),
        ));
    }
    Ok(p)
}

/// Take locks in the same order as billing and import, and recheck write access.
pub async fn lock_project(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    id: Uuid,
) -> AppResult<()> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?;
    let archived = sqlx::query_scalar::<_, bool>(
        "SELECT archived FROM projects WHERE owner_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if archived {
        return Err(AppError::Conflict(
            "This project is archived. Restore it to make changes.".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub projects: i64,
    pub repos: i64,
    pub storage_bytes: i64,
    pub image_bytes: i64,
    pub history_days: Option<i64>,
}
pub fn limits(plan: &str) -> Limits {
    let (projects, storage_bytes, image_bytes, history_days) = match plan {
        "max" => (50, 5_000_000_000, 20_000_000, None),
        "plus" => (10, 1_000_000_000, 10_000_000, Some(365)),
        _ => (1, 100_000_000, 5_000_000, Some(30)),
    };
    Limits {
        projects,
        repos: projects,
        storage_bytes,
        image_bytes,
        history_days,
    }
}

// Measure customer content in UTF-8 bytes. Provider caches, event logs and backups are excluded.
const USAGE_SQL: &str = "SELECT COALESCE((SELECT content_bytes FROM users WHERE id=$1),0)::bigint";

pub async fn storage_used(db: &PgPool, owner: Uuid) -> AppResult<i64> {
    Ok(sqlx::query_scalar(USAGE_SQL)
        .bind(owner)
        .fetch_one(db)
        .await?)
}
pub async fn storage_used_tx(tx: &mut Transaction<'_, Postgres>, owner: Uuid) -> AppResult<i64> {
    Ok(sqlx::query_scalar(USAGE_SQL)
        .bind(owner)
        .fetch_one(&mut **tx)
        .await?)
}
pub async fn check_storage(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    growth: i64,
) -> AppResult<()> {
    let plan: String = sqlx::query_scalar("SELECT plan FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?;
    if growth > 0
        && storage_used_tx(tx, owner).await?.saturating_add(growth) > limits(&plan).storage_bytes
    {
        return Err(AppError::Conflict(
            "This would exceed your workspace storage limit. Free some space or change plan."
                .into(),
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_plan_has_free_limits() {
        assert_eq!(limits("unknown"), limits("free"));
        assert_eq!(limits("max").projects, 50);
    }
    #[sqlx::test]
    async fn locked_writes_isolate_archived_and_foreign_projects(db: PgPool) {
        let a: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('store@test.invalid') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let p:Uuid=sqlx::query_scalar("INSERT INTO projects(owner_id,name,key,archived) VALUES($1,'Demo','DM',true) RETURNING id").bind(a).fetch_one(&db).await.unwrap();
        assert!(matches!(
            project(&db, Uuid::new_v4(), p).await,
            Err(AppError::NotFound)
        ));
        let mut tx = db.begin().await.unwrap();
        assert!(matches!(
            lock_project(&mut tx, a, p).await,
            Err(AppError::Conflict(_))
        ));
        assert_eq!(storage_used_tx(&mut tx, a).await.unwrap(), 0);
        assert!(check_storage(&mut tx, a, 100_000_001).await.is_err());
    }
}
