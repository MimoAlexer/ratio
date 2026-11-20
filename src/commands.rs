use crate::{AppResult, AppState, auth::Auth, store};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/commands", get(search))
}

#[derive(Deserialize, Default)]
struct Search {
    #[serde(default)]
    q: String,
    project: Option<Uuid>,
}

#[derive(Serialize)]
struct Scope {
    id: Uuid,
    name: String,
    key: String,
    archived: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
enum Kind {
    Issue,
    Note,
    Project,
}

#[derive(FromRow, Serialize)]
struct Entry {
    kind: Kind,
    id: Uuid,
    title: String,
    detail: String,
    href: String,
}

#[derive(Serialize)]
struct Matches {
    scope: Option<Scope>,
    can_create: bool,
    results: Vec<Entry>,
}

async fn search(
    State(state): State<AppState>,
    auth: Auth,
    Query(query): Query<Search>,
) -> AppResult<Json<Matches>> {
    Ok(Json(lookup(&state.db, auth.user_id, query).await?))
}

async fn lookup(db: &PgPool, owner: Uuid, query: Search) -> AppResult<Matches> {
    let scope = if let Some(id) = query.project {
        let p = store::project(db, owner, id).await?;
        Some(Scope {
            id,
            name: p.name,
            key: p.key,
            archived: p.archived,
        })
    } else {
        None
    };
    let can_create = match &scope {
        Some(p) => !p.archived,
        None => {
            sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE owner_id=$1 AND NOT archived)",
            )
            .bind(owner)
            .fetch_one(db)
            .await?
        }
    };

    // Search only bounded, native display fields. Full descriptions and long
    // Markdown bodies belong to the explicit search pages, not each keystroke.
    let query_text: String = query.q.chars().take(120).collect();
    let term = query_text.split_whitespace().collect::<Vec<_>>().join(" ");
    let results = sqlx::query_as(
        r#"
        WITH visible_projects AS MATERIALIZED (
            SELECT id, name, key, archived, created_at FROM projects
            WHERE owner_id = $1 AND ($2::uuid IS NULL OR id = $2)
              AND (NOT archived OR id = $2 OR $3 <> '')
        ), matches AS (
            (SELECT 'issue'::text AS kind, i.id, i.title, identifier AS detail,
                    '/issues/' || i.id::text AS href, i.updated_at AS touched,
                    (i.archived OR p.archived) AS archived,
                    CASE WHEN lower(identifier) = lower($3) THEN 0
                         WHEN lower(i.title) = lower($3) THEN 1
                         WHEN starts_with(lower(i.title),lower($3)) THEN 2 ELSE 3 END AS rank
             FROM visible_projects p JOIN issues i ON i.project_id=p.id AND i.owner_id=$1
             CROSS JOIN LATERAL (SELECT p.key || '-' || i.number::text AS identifier) key
             WHERE (NOT i.archived OR p.archived OR $3 <> '')
               AND ($3='' OR strpos(lower(i.title),lower($3))>0 OR starts_with(lower(identifier),lower($3)))
             ORDER BY rank, archived, touched DESC, i.id LIMIT 6)
            UNION ALL
            (SELECT 'note', n.id, n.title,
                    CASE WHEN $2::uuid IS NULL THEN p.key || ' · ' || n.path ELSE n.path END,
                    '/notes/' || n.id::text, n.updated_at, p.archived,
                    CASE WHEN lower(n.title) = lower($3) OR lower(n.path) = lower($3) THEN 1
                         WHEN starts_with(lower(n.title),lower($3)) THEN 2 ELSE 3 END AS rank
             FROM visible_projects p JOIN notes n ON n.project_id=p.id AND n.owner_id=$1
             WHERE $3='' OR strpos(lower(n.title),lower($3))>0 OR strpos(lower(n.path),lower($3))>0
             ORDER BY rank, p.archived, n.updated_at DESC, n.id LIMIT 4)
            UNION ALL
            (SELECT 'project', p.id, p.name, p.key, '/projects/' || p.id::text, p.created_at, p.archived,
                    CASE WHEN lower(p.key) = lower($3) OR lower(p.name) = lower($3) THEN 1
                         WHEN starts_with(lower(p.name),lower($3)) THEN 2 ELSE 3 END AS rank
             FROM visible_projects p
             WHERE $3='' OR strpos(lower(p.name),lower($3))>0 OR starts_with(lower(p.key),lower($3))
             ORDER BY rank, p.archived, p.created_at DESC, p.id LIMIT 3)
        )
        SELECT kind,id,title,detail || CASE WHEN archived THEN ' · Archived' ELSE '' END AS detail,href
        FROM matches ORDER BY CASE WHEN $3='' THEN 0 ELSE rank END, archived, touched DESC, kind, id
        LIMIT CASE WHEN $3='' THEN 4 ELSE 12 END
        "#,
    )
    .bind(owner)
    .bind(query.project)
    .bind(term)
    .fetch_all(db)
    .await?;
    Ok(Matches {
        scope,
        can_create,
        results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn owner(db: &PgPool, email: &str) -> Uuid {
        sqlx::query_scalar("INSERT INTO users(email) VALUES($1) RETURNING id")
            .bind(email)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn project(db: &PgPool, owner: Uuid, key: &str) -> Uuid {
        sqlx::query_scalar("INSERT INTO projects(owner_id,key,name) VALUES($1,$2,$2) RETURNING id")
            .bind(owner)
            .bind(key)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn find(db: &PgPool, owner: Uuid, project: Option<Uuid>, text: &str) -> Matches {
        lookup(
            db,
            owner,
            Search {
                project,
                q: text.to_owned(),
            },
        )
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn commands_scope_all_kinds_rank_exact_ids_and_bound_recents(db: PgPool) {
        let me = owner(&db, "me@cmd.invalid").await;
        let them = owner(&db, "other@cmd.invalid").await;
        let here = project(&db, me, "RATIO").await;
        let elsewhere = project(&db, me, "OTHER").await;
        let foreign = project(&db, them, "PRIVATE").await;
        for (owner, project) in [(me, here), (me, elsewhere), (them, foreign)] {
            sqlx::query("INSERT INTO issues(owner_id,project_id,number,title) SELECT $1,$2,x,'Ratio issue '||x FROM generate_series(1,12) x")
                .bind(owner).bind(project).execute(&db).await.unwrap();
            sqlx::query("INSERT INTO notes(owner_id,project_id,title,path) VALUES($1,$2,'Release notes','design/ratio.md')")
                .bind(owner).bind(project).execute(&db).await.unwrap();
        }
        let recent = find(&db, me, None, "   ").await;
        assert!(recent.can_create);
        assert_eq!(recent.results.len(), 4);
        let results = find(&db, me, Some(here), "  rAtiO   ").await;
        assert_eq!(results.scope.unwrap().id, here);
        assert_eq!(results.results.len(), 8); // six issues, one project, one note
        assert!(
            results
                .results
                .iter()
                .all(|e| e.kind != Kind::Issue || e.detail.starts_with("RATIO-"))
        );
        let notes = find(&db, me, None, "release notes").await;
        let mut paths: Vec<_> = notes
            .results
            .iter()
            .map(|row| row.detail.as_str())
            .collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            ["OTHER · design/ratio.md", "RATIO · design/ratio.md"]
        );
        let exact = find(&db, me, None, "ratio-1").await;
        assert_eq!(exact.results[0].detail, "RATIO-1");
        assert!(
            find(&db, me, Some(elsewhere), "ratio-1")
                .await
                .results
                .is_empty()
        );
        assert!(find(&db, me, None, "private").await.results.is_empty());
        assert!(find(&db, me, None, "%_").await.results.is_empty());
        assert!(matches!(
            lookup(
                &db,
                me,
                Search {
                    project: Some(foreign),
                    ..Default::default()
                }
            )
            .await,
            Err(crate::AppError::NotFound)
        ));
        sqlx::query("UPDATE issues SET archived=true WHERE project_id=$1 AND number=7")
            .bind(here)
            .execute(&db)
            .await
            .unwrap();
        let archived_issue = find(&db, me, None, "ratio-7").await;
        assert_eq!(archived_issue.results[0].detail, "RATIO-7 · Archived");
        sqlx::query("UPDATE projects SET archived=true WHERE owner_id=$1")
            .bind(me)
            .execute(&db)
            .await
            .unwrap();
        let global = find(&db, me, None, "").await;
        assert!(!global.can_create && global.results.is_empty());
        let archived = find(&db, me, Some(here), "ratio-1").await;
        assert!(!archived.can_create && archived.scope.unwrap().archived);
        assert_eq!(archived.results[0].detail, "RATIO-1 · Archived");
        let global_archive = find(&db, me, None, "ratio-1").await;
        assert_eq!(global_archive.results[0].detail, "RATIO-1 · Archived");
    }
}
