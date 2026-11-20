use super::{configured, credentials, live_repo};
use crate::{AppError, AppResult, AppState, auth::Auth, store, ui};
use axum::{
    Router,
    extract::{Form, Path, State},
    response::Redirect,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/assets/github-resume.js", get(resume_script))
        .route("/issues/{id}/github/resume", get(resume_json))
        .route("/github/alerts/{repo}/{number}/fix", post(start_fix))
        .route("/issues/{id}/github/pulls", post(link_pull))
}

// Both counts and rows must use the same scope, including the token's own expiry.
// Incomplete scans never replace a previous snapshot with an apparent zero.
const SCOPE: &str = r#"
    WITH scoped AS (
      SELECT r.*,
        ($3 AND r.repo_grant_until > now() AND r.alert_grant_until > now()
          AND (c.expires_at IS NULL OR c.expires_at > now())
          AND r.failures = 0 AND r.last_success >= now() - interval '2 hours') IS TRUE AS checked
      FROM github_repos r JOIN github_connections c USING (owner_id)
      JOIN projects p ON p.id = r.project_id AND p.owner_id = r.owner_id
      WHERE r.owner_id = $1 AND ($2::uuid IS NULL OR r.project_id = $2)
        AND NOT p.archived AND NOT r.billing_paused
    ), counts AS (
      SELECT count(*) FILTER (WHERE checked) AS checked,
             count(*) FILTER (WHERE NOT checked) AS unavailable,
             (SELECT count(*) FROM github_alerts a JOIN scoped r
               USING (owner_id,repo_id) WHERE r.checked) AS critical
      FROM scoped
    )
"#;

pub async fn coverage(s: &AppState, auth: &Auth, project: Uuid) -> AppResult<(i64, i64, i64)> {
    coverage_db(&s.db, auth.user_id, project, configured(s).is_ok()).await
}

pub(super) async fn coverage_db(
    db: &PgPool,
    owner: Uuid,
    project: Uuid,
    enabled: bool,
) -> AppResult<(i64, i64, i64)> {
    let query = format!("{SCOPE} SELECT checked, unavailable, critical FROM counts");
    Ok(sqlx::query_as(&query)
        .bind(owner)
        .bind(project)
        .bind(enabled)
        .fetch_one(db)
        .await?)
}

#[derive(FromRow)]
struct FindingRow {
    checked: i64,
    unavailable: i64,
    critical: i64,
    repo_id: Option<i64>,
    display_name: Option<String>,
    number: Option<i64>,
    package: Option<String>,
    manifest: Option<String>,
    patched: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    last_success: Option<DateTime<Utc>>,
    issue_id: Option<Uuid>,
}

async fn findings(
    db: &PgPool,
    owner: Uuid,
    project: Option<Uuid>,
    enabled: bool,
) -> AppResult<Vec<FindingRow>> {
    let query = format!(
        r#"{SCOPE}
        SELECT c.*, f.* FROM counts c LEFT JOIN (
          SELECT r.repo_id, r.display_name, a.number, a.package, a.manifest,
                 a.patched, a.updated_at, r.last_success, x.issue_id
          FROM scoped r JOIN github_alerts a USING (owner_id,repo_id)
          LEFT JOIN github_fixes x USING (owner_id,repo_id,number)
          WHERE r.checked
        ) f ON true ORDER BY f.updated_at DESC, f.repo_id, f.number
    "#
    );
    Ok(sqlx::query_as(&query)
        .bind(owner)
        .bind(project)
        .bind(enabled)
        .fetch_all(db)
        .await?)
}

fn alert_url(repo: &str, number: i64) -> String {
    format!("https://github.com/{repo}/security/dependabot/{number}")
}

fn time_at(value: DateTime<Utc>) -> Markup {
    html! { time datetime=(value.to_rfc3339()) { (value.format("%b %-d, %Y · %H:%M UTC")) } }
}

pub async fn project_home(s: &AppState, auth: &Auth, project: Option<Uuid>) -> Markup {
    project_home_parts(s, auth, project).await.1
}

pub async fn project_home_parts(
    s: &AppState,
    auth: &Auth,
    project: Option<Uuid>,
) -> (String, Markup) {
    home_parts_db(&s.db, auth, project, configured(s).is_ok()).await
}

#[cfg(test)]
pub(super) async fn home_db(
    db: &PgPool,
    auth: &Auth,
    project: Option<Uuid>,
    enabled: bool,
) -> Markup {
    home_parts_db(db, auth, project, enabled).await.1
}

async fn home_parts_db(
    db: &PgPool,
    auth: &Auth,
    project: Option<Uuid>,
    enabled: bool,
) -> (String, Markup) {
    let found = findings(db, auth.user_id, project, enabled).await;
    let summary = match found.as_deref().ok().and_then(|rows| rows.first()) {
        _ if !enabled => "GitHub unavailable".into(),
        Some(row) if row.checked > 0 => {
            let mut label = format!("{} critical · {} checked", row.critical, row.checked);
            if row.unavailable > 0 {
                label.push_str(&format!(" · {} unavailable", row.unavailable));
            }
            label
        }
        Some(row) if row.unavailable > 0 => format!("{} unavailable", row.unavailable),
        Some(_) => "No repositories connected".into(),
        None => "Coverage unavailable".into(),
    };
    let panel = html! {
        section.panel id="github-security" aria-labelledby="github-security-title" {
            h2 id="github-security-title" { "GitHub dependency alerts" }
            @if !enabled {
                p.muted { "GitHub is awaiting server configuration." }
                a href="/integrations" { "View integration status" }
            } @else if let Ok(rows) = found {
                @if let Some(summary) = rows.first() {
                    @if summary.checked == 0 && summary.unavailable == 0 {
                        p.muted { "Connect a repository to check for critical dependency alerts." }
                        a href="/integrations" { "Connect GitHub" }
                    } @else {
                        @if summary.checked > 0 {
                            p { strong { (summary.critical) " open critical" } " in " (summary.checked)
                                @if summary.checked == 1 { " checked repository" } @else { " checked repositories" }
                            }
                        }
                        @if summary.unavailable > 0 {
                            p.notice { "Coverage unavailable for " (summary.unavailable)
                                @if summary.unavailable == 1 { " repository." } @else { " repositories." }
                                " " a href="/integrations" { "Review connections" }
                            }
                        }
                        @if summary.critical > 0 {
                            ul.issue-list {
                                @for row in &rows {
                                    @if let (Some(repo_id), Some(repo), Some(number), Some(package), Some(manifest)) =
                                        (row.repo_id, &row.display_name, row.number, &row.package, &row.manifest) {
                                        li {
                                            strong { (package) } " · " (repo) " "
                                            a href=(alert_url(repo, number)) rel="noreferrer" { "GitHub alert #" (number) }
                                            p.muted {
                                                "Manifest " code { (manifest) }
                                                @if let Some(patch) = &row.patched { " · Patched in " (patch) }
                                            }
                                            p.muted {
                                                @if let Some(t) = row.updated_at { "Updated " (time_at(t)) }
                                                @if let Some(t) = row.last_success { " · Checked " (time_at(t)) }
                                            }
                                            @if let Some(id) = row.issue_id {
                                                a.button.secondary href=(format!("/issues/{id}")) { "Open fix" }
                                            } @else {
                                                form method="post" action=(format!("/github/alerts/{repo_id}/{number}/fix")) {
                                                    (ui::csrf(auth))
                                                    button type="submit" { "Start fix" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } @else {
                p.notice { "GitHub coverage is unavailable." }
            }
        }
    };
    (summary, panel)
}

#[derive(Clone, FromRow)]
pub(super) struct Binding {
    pub(super) repo_id: i64,
    installation_id: i64,
    pub(super) project_id: Uuid,
    pub(super) generation: Uuid,
    checked_from: DateTime<Utc>,
}

async fn binding(db: &PgPool, owner: Uuid, repo: i64) -> AppResult<Binding> {
    sqlx::query_as(
        "SELECT r.repo_id,r.installation_id,r.project_id,r.generation,clock_timestamp() AS checked_from FROM github_repos r
         JOIN projects p ON p.owner_id=r.owner_id AND p.id=r.project_id
         WHERE r.owner_id=$1 AND r.repo_id=$2 AND NOT r.billing_paused AND NOT p.archived",
    )
    .bind(owner)
    .bind(repo)
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

async fn same_binding(db: &PgPool, owner: Uuid, key: &Binding) -> AppResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM github_repos r JOIN github_connections c USING (owner_id)
         JOIN projects p ON p.owner_id=r.owner_id AND p.id=r.project_id
         WHERE r.owner_id=$1 AND r.repo_id=$2 AND r.generation=$3
         AND r.project_id=$4 AND NOT r.billing_paused AND NOT p.archived
         AND (c.expires_at IS NULL OR c.expires_at>clock_timestamp())
         AND $5::timestamptz+interval '5 minutes'>clock_timestamp())",
    )
    .bind(owner)
    .bind(key.repo_id)
    .bind(key.generation)
    .bind(key.project_id)
    .bind(key.checked_from)
    .fetch_one(db)
    .await?)
}

async fn recheck_binding(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    key: &Binding,
) -> AppResult<()> {
    let current:Option<i64> = sqlx::query_scalar(
        "SELECT r.repo_id FROM github_repos r JOIN github_connections c USING(owner_id)
         WHERE r.owner_id=$1 AND r.repo_id=$2 AND r.generation=$3 AND r.project_id=$4 AND NOT r.billing_paused
         AND (c.expires_at IS NULL OR c.expires_at>clock_timestamp())
         AND $5::timestamptz+interval '5 minutes'>clock_timestamp() FOR UPDATE OF r",
    ).bind(owner).bind(key.repo_id).bind(key.generation).bind(key.project_id).bind(key.checked_from)
        .fetch_optional(&mut **tx).await?;
    current.map(|_| ()).ok_or(AppError::NotFound)
}

async fn alert_read_failed(
    db: &PgPool,
    owner: Uuid,
    key: &Binding,
    error: &AppError,
) -> AppResult<()> {
    let revoked = matches!(error, AppError::Forbidden | AppError::NotFound);
    sqlx::query(
        "WITH invalid AS (
           UPDATE github_repos SET alert_grant_until=NULL, lease_id=NULL, lease_until=NULL,
             generation=CASE WHEN $4 THEN gen_random_uuid() ELSE generation END,
             next_sync_at=now(), failures=GREATEST(failures,1)
           WHERE owner_id=$1 AND repo_id=$2 AND generation=$3 RETURNING owner_id,repo_id
         ) DELETE FROM github_alerts a USING invalid r
           WHERE $4 AND a.owner_id=r.owner_id AND a.repo_id=r.repo_id",
    )
    .bind(owner)
    .bind(key.repo_id)
    .bind(key.generation)
    .bind(revoked)
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
struct Command {
    csrf: String,
}

async fn start_fix(
    State(s): State<AppState>,
    auth: Auth,
    Path((repo, number)): Path<(i64, i64)>,
    Form(input): Form<Command>,
) -> AppResult<Redirect> {
    auth.check_csrf(&input.csrf)?;
    if repo <= 0 || number <= 0 {
        return Err(AppError::NotFound);
    }
    let key = binding(&s.db, auth.user_id, repo).await?;
    let provider = configured(&s)?;
    let access = credentials(&s, &provider, auth.user_id).await?;
    let remote = live_repo(&s, &provider, auth.user_id, &access, repo).await?;
    let alerts = match provider.alerts(&access, &remote).await {
        Ok(alerts) => alerts,
        Err(error) => {
            alert_read_failed(&s.db, auth.user_id, &key, &error).await?;
            return Err(error);
        }
    };
    if !alerts.iter().any(|a| a.number == number) {
        return Err(AppError::NotFound);
    }
    let id = record_fix(&s.db, auth.user_id, &key, number).await?;
    Ok(Redirect::to(&format!("/issues/{id}")))
}

async fn record_fix(db: &PgPool, owner: Uuid, key: &Binding, number: i64) -> AppResult<Uuid> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_one(&mut *tx)
        .await?;
    store::lock_project(&mut tx, owner, key.project_id).await?;
    recheck_binding(&mut tx, owner, key).await?;
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT issue_id FROM github_fixes WHERE owner_id=$1 AND repo_id=$2 AND number=$3",
    )
    .bind(owner)
    .bind(key.repo_id)
    .bind(number)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = existing {
        tx.commit().await?;
        return Ok(id);
    }
    let issue = crate::issues::create_in(
        &mut tx,
        owner,
        Uuid::new_v4(),
        crate::issues::NewIssue {
            project: key.project_id,
            title: "Review follow-up".into(),
            body: String::new(),
            status: "ready".into(),
            priority: "normal".into(),
            kind: "maintenance".into(),
            labels: String::new(),
            next_action: String::new(),
            severity: "none".into(),
        },
    )
    .await?;
    sqlx::query("INSERT INTO github_fixes(owner_id,repo_id,number,issue_id) VALUES($1,$2,$3,$4)")
        .bind(owner)
        .bind(key.repo_id)
        .bind(number)
        .bind(issue)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(issue)
}

pub(super) struct PullUrl {
    pub(super) repo: String,
    pub(super) number: i64,
}

pub(super) fn parse_pull_url(input: &str) -> AppResult<PullUrl> {
    let bad = || AppError::BadRequest("Enter a GitHub pull request URL.".into());
    let url = reqwest::Url::parse(input.trim()).map_err(|_| bad())?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(bad());
    }
    let path: Vec<_> = url
        .path()
        .trim_end_matches('/')
        .split('/')
        .skip(1)
        .collect();
    let [owner, repo, "pull", n, tail @ ..] = &path[..] else {
        return Err(bad());
    };
    let valid = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    if !valid(owner)
        || !valid(repo)
        || !n.bytes().all(|b| b.is_ascii_digit())
        || !matches!(tail, [] | ["files"] | ["commits"] | ["checks"])
    {
        return Err(bad());
    }
    let number: i64 = n.parse().map_err(|_| bad())?;
    if number <= 0 {
        return Err(bad());
    }
    Ok(PullUrl {
        repo: format!("{owner}/{repo}"),
        number,
    })
}

#[derive(Deserialize)]
struct PullForm {
    csrf: String,
    url: String,
}

pub(super) async fn resolve_pull(
    s: &AppState,
    owner: Uuid,
    project: Option<Uuid>,
    input: &PullUrl,
) -> AppResult<(Binding, super::provider::Pull)> {
    let provider = configured(s)?;
    resolve_pull_with(s, owner, project, input, &provider).await
}

async fn resolve_pull_with(
    s: &AppState,
    owner: Uuid,
    project: Option<Uuid>,
    input: &PullUrl,
    provider: &super::provider::Provider,
) -> AppResult<(Binding, super::provider::Pull)> {
    // GitHub can rename a repository while retaining its numeric identity.
    // Match the current catalog against the workspace's stable IDs before
    // requesting PR data, even if the stored display name is out of date.
    let keys: Vec<Binding> = sqlx::query_as(
        "SELECT r.repo_id,r.installation_id,r.project_id,r.generation,clock_timestamp() AS checked_from
         FROM github_repos r JOIN projects p ON p.id=r.project_id AND p.owner_id=r.owner_id
         WHERE r.owner_id=$1 AND ($2::uuid IS NULL OR r.project_id=$2)
           AND NOT p.archived AND NOT r.billing_paused",
    ).bind(owner).bind(project).fetch_all(&s.db).await?;
    if keys.is_empty() {
        return Err(AppError::NotFound);
    }
    let access = credentials(s, provider, owner).await?;
    let catalog = super::catalog(s, provider, owner, &access).await?;
    let repo = catalog
        .into_iter()
        .find(|repo| repo.name.eq_ignore_ascii_case(&input.repo))
        .ok_or(AppError::NotFound)?;
    let key = keys
        .into_iter()
        .find(|key| key.repo_id == repo.id && key.installation_id == repo.installation)
        .ok_or(AppError::NotFound)?;
    if !same_binding(&s.db, owner, &key).await? {
        return Err(AppError::NotFound);
    }
    let pr = provider.pull(&access, &repo, input.number).await?;
    if !same_binding(&s.db, owner, &key).await? {
        return Err(AppError::NotFound);
    }
    Ok((key, pr))
}

async fn link_pull(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Form(form): Form<PullForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let input = parse_pull_url(&form.url)?;
    let project: Uuid = sqlx::query_scalar(
        "SELECT project_id FROM issues WHERE owner_id=$1 AND id=$2 AND NOT archived",
    )
    .bind(auth.user_id)
    .bind(id)
    .fetch_optional(&s.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let (key, _pr) = resolve_pull(&s, auth.user_id, Some(project), &input).await?;
    let mut tx = s.db.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(auth.user_id)
        .fetch_one(&mut *tx)
        .await?;
    store::lock_project(&mut tx, auth.user_id, key.project_id).await?;
    recheck_binding(&mut tx, auth.user_id, &key).await?;
    let present: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM issues WHERE id=$1 AND owner_id=$2 AND project_id=$3 AND NOT archived FOR UPDATE",
    ).bind(id).bind(auth.user_id).bind(key.project_id).fetch_optional(&mut *tx).await?;
    present.ok_or(AppError::NotFound)?;
    sqlx::query("INSERT INTO github_pulls(owner_id,repo_id,number,issue_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(auth.user_id).bind(key.repo_id).bind(input.number).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/issues/{id}#github-context")))
}

async fn links(
    s: &AppState,
    auth: &Auth,
    id: Uuid,
    provider: Option<&super::provider::Provider>,
) -> AppResult<(bool, Vec<Markup>)> {
    let owner = auth.user_id;
    let writable: bool = sqlx::query_scalar(
        "SELECT NOT i.archived AND NOT p.archived FROM issues i JOIN projects p
         ON i.project_id=p.id AND i.owner_id=p.owner_id WHERE i.id=$1 AND i.owner_id=$2",
    )
    .bind(id)
    .bind(owner)
    .fetch_optional(&s.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let Some(provider) = provider else {
        return Ok((false, vec![]));
    };
    let rows: Vec<(i64, i64, bool)> = sqlx::query_as(
        "SELECT repo_id,number,true FROM github_fixes WHERE owner_id=$1 AND issue_id=$2
         UNION ALL SELECT repo_id,number,false FROM github_pulls WHERE owner_id=$1 AND issue_id=$2
         ORDER BY 1,3 DESC,2",
    )
    .bind(owner)
    .bind(id)
    .fetch_all(&s.db)
    .await?;
    if rows.is_empty() {
        return Ok((writable, vec![]));
    }
    let mut groups = Vec::new();
    // PR access is independent of alert access. After a denied alert read,
    // resolve the fresh binding for this repository's PRs.
    for group in rows.chunk_by(|a, b| a.0 == b.0 && a.2 == b.2) {
        let repo_id = group[0].0;
        let Ok(key) = binding(&s.db, owner, repo_id).await else {
            continue;
        };
        let Ok(access) = credentials(s, provider, owner).await else {
            continue;
        };
        let Ok(repo) = live_repo(s, provider, owner, &access, repo_id).await else {
            continue;
        };
        let mut group_cards = Vec::new();
        for &(_, number, is_fix) in group {
            if is_fix {
                let alerts = match provider.alerts(&access, &repo).await {
                    Ok(alerts) => alerts,
                    Err(error) => {
                        alert_read_failed(&s.db, owner, &key, &error).await?;
                        continue;
                    }
                };
                if let Some(a) = alerts.into_iter().find(|a| a.number == number) {
                    group_cards.push(html! {
                        li {
                            strong { "Critical dependency alert" } " · " (a.package)
                            p { (repo.name) " · " code { (a.manifest) } }
                            @if let Some(v)=a.patched { p { "Patched in " (v) } }
                            p.muted { "Updated " (time_at(a.updated_at)) }
                            a href=(alert_url(&repo.name,number)) rel="noreferrer" { "Open GitHub alert #" (number) }
                        }
                    });
                }
            } else if let Ok(pr) = provider.pull(&access, &repo, number).await {
                let url = format!("https://github.com/{}/pull/{}", repo.name, pr.number);
                group_cards.push(html! {
                    li {
                        a href=(&url) rel="noreferrer" { "#" (pr.number) " " (pr.title) }
                        " · " @if pr.merged { "Merged" } @else { (pr.state) }
                        p.muted { (repo.name) " · "
                            a href=(format!("{url}/files")) rel="noreferrer" { "Files changed" }
                        }
                    }
                });
            }
        }
        if !group_cards.is_empty() {
            groups.push((key, group_cards));
        }
    }
    let mut cards = Vec::new();
    for (key, group_cards) in groups {
        if same_binding(&s.db, owner, &key).await? {
            cards.extend(group_cards);
        }
    }
    Ok((writable, cards))
}

pub async fn issue_panel(s: &AppState, auth: &Auth, id: Uuid) -> Markup {
    let provider = configured(s).ok();
    let Ok((writable, cards)) = links(s, auth, id, provider.as_ref()).await else {
        return html! { section.panel { h2 { "GitHub" } p.muted { "GitHub context is unavailable." } } };
    };
    html! {
        section.panel id="github-context" aria-labelledby="github-context-title" {
            h2 id="github-context-title" { "GitHub" }
            @if !cards.is_empty() { ul.issue-list { @for card in &cards { (card) } } }
            @if writable {
                form.stack method="post" action=(format!("/issues/{id}/github/pulls")) {
                    (ui::csrf(auth))
                    label.field { "Pull request URL"
                        input name="url" type="url" required placeholder="https://github.com/owner/repo/pull/123";
                    }
                    button.secondary type="submit" { "Link pull request" }
                }
            } @else if cards.is_empty() {
                p.muted { "No GitHub details available." }
            }
        }
    }
}

// Keep the Home response independent of GitHub latency. Before authorization the
// fallback links only to the owner's local issue, regardless of any PR association.
pub async fn resume_pr(s: &AppState, _auth: &Auth, id: Uuid) -> Markup {
    if configured(s).is_err() {
        return html! {};
    }
    html! {
        span data-resume-pr=(format!("/issues/{id}/github/resume")) {
            a href=(format!("/issues/{id}#github-context")) { "GitHub context" }
        }
        script defer src="/assets/github-resume.js" {}
    }
}

#[derive(Serialize)]
struct ResumePull {
    number: i64,
    title: String,
    url: String,
}

async fn resume_source(
    s: &AppState,
    auth: &Auth,
    id: Uuid,
    provider: Option<&super::provider::Provider>,
) -> AppResult<Option<ResumePull>> {
    let Some(provider) = provider else {
        return Ok(None);
    };
    let owner = auth.user_id;
    let candidate: Option<(i64, i64)> = sqlx::query_as(
        "SELECT x.repo_id,x.number FROM github_pulls x
         JOIN issues i ON i.id=x.issue_id AND i.owner_id=x.owner_id
         JOIN projects p ON p.id=i.project_id AND p.owner_id=i.owner_id
         JOIN github_repos r ON r.repo_id=x.repo_id AND r.owner_id=x.owner_id
         JOIN projects mapped ON mapped.id=r.project_id AND mapped.owner_id=r.owner_id
         WHERE i.owner_id=$1 AND i.id=$2 AND NOT i.archived AND NOT p.archived
         AND NOT r.billing_paused AND NOT mapped.archived
         ORDER BY x.linked_at DESC,x.repo_id,x.number DESC LIMIT 1",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(&s.db)
    .await?;
    let Some((repo_id, number)) = candidate else {
        return Ok(None);
    };
    let key = binding(&s.db, owner, repo_id).await?;
    let access = credentials(s, provider, owner).await?;
    let repo = live_repo(s, provider, owner, &access, repo_id).await?;
    let pull = provider.pull(&access, &repo, number).await?;
    if !same_binding(&s.db, owner, &key).await? {
        return Ok(None);
    }
    Ok(Some(ResumePull {
        number: pull.number,
        title: pull.title,
        url: format!("https://github.com/{}/pull/{}", repo.name, pull.number),
    }))
}

async fn resume_json(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> axum::Json<Option<ResumePull>> {
    let provider = configured(&s).ok();
    axum::Json(
        resume_source(&s, &auth, id, provider.as_ref())
            .await
            .unwrap_or_default(),
    )
}

async fn resume_script() -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        r##"
for (const slot of document.querySelectorAll("[data-resume-pr]")) {
    if (slot.dataset.started) continue;
    slot.dataset.started = "1";
    const fallback = slot.firstElementChild.cloneNode(true);
    let revision = 0;
    let timer;
    async function refresh() {
        const request = ++revision;
        clearTimeout(timer);
        slot.replaceChildren(fallback.cloneNode(true));
        try {
            const response = await fetch(slot.dataset.resumePr, { credentials: "same-origin", cache: "no-store" });
            if (!response.ok || !response.headers.get("content-type")?.includes("application/json")) return;
            const pull = await response.json();
            if (request !== revision || document.visibilityState === "hidden") return;
            if (!pull) { slot.replaceChildren(); return; }
            const link = document.createElement("a");
            link.href = pull.url;
            link.rel = "noreferrer";
            link.textContent = "#" + pull.number + " " + pull.title;
            slot.replaceChildren(link);
            timer = setTimeout(refresh, 240000);
        } catch {}
    }
    refresh();
    window.addEventListener("pageshow", event => { if (event.persisted) refresh(); });
    document.addEventListener("visibilitychange", () => {
        if (document.visibilityState === "visible") refresh();
        else {
            ++revision;
            clearTimeout(timer);
            slot.replaceChildren(fallback.cloneNode(true));
        }
    });
}
"##,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[sqlx::test(migrations = "./migrations")]
    async fn racing_fixes_survive_upstream_close_and_remap(db: PgPool) {
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES ('fix@example.invalid') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Test','TEST') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO github_connections(owner_id,user_remote_id,access_cipher) VALUES ($1,44,$2)")
            .bind(owner).bind([0u8;32].as_slice()).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO github_repos(owner_id,repo_id,project_id,installation_id) VALUES ($1,13,$2,88)")
            .bind(owner).bind(project).execute(&db).await.unwrap();
        let key = binding(&db, owner, 13).await.unwrap();
        let (a, b) = tokio::join!(
            record_fix(&db, owner, &key, 777),
            record_fix(&db, owner, &key, 777)
        );
        let issue = a.unwrap();
        assert_eq!(issue, b.unwrap());
        let record:(i64,i64,bool,String)=sqlx::query_as(
            "SELECT (SELECT count(*) FROM issues),(SELECT count(*) FROM issue_events),security_critical,title FROM issues WHERE id=$1"
        ).bind(issue).fetch_one(&db).await.unwrap();
        assert_eq!(record, (1, 1, false, "Review follow-up".to_owned()));
        let actor = Auth {
            user_id: owner,
            email: "fix@example.invalid".into(),
            csrf: String::new(),
        };
        sqlx::query("UPDATE github_repos SET display_name='org/fixture',repo_grant_until=now()+interval '4 minutes',
             alert_grant_until=now()+interval '4 minutes',last_success=now() WHERE owner_id=$1 AND repo_id=13")
            .bind(owner).execute(&db).await.unwrap();
        let reopen =
            "INSERT INTO github_alerts(owner_id,repo_id,number,package,manifest,updated_at)
                    VALUES($1,13,777,'test-package','Cargo.lock',now())";
        sqlx::query(reopen).bind(owner).execute(&db).await.unwrap();
        sqlx::query("UPDATE issues SET status='done',completed_at=now() WHERE id=$1")
            .bind(issue)
            .execute(&db)
            .await
            .unwrap();
        assert!(
            home_db(&db, &actor, Some(project), true)
                .await
                .into_string()
                .contains("Open fix")
        );
        sqlx::query("DELETE FROM github_alerts WHERE owner_id=$1")
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            coverage_db(&db, owner, project, true).await.unwrap(),
            (1, 0, 0)
        );
        assert!(
            !home_db(&db, &actor, Some(project), true)
                .await
                .into_string()
                .contains("Open fix")
        );
        sqlx::query(reopen).bind(owner).execute(&db).await.unwrap();
        assert!(
            home_db(&db, &actor, Some(project), true)
                .await
                .into_string()
                .contains(&format!("/issues/{issue}"))
        );
        let moved: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Today','TODAY') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("UPDATE github_repos SET project_id=$1,generation=gen_random_uuid() WHERE owner_id=$2 AND repo_id=13")
            .bind(moved).bind(owner).execute(&db).await.unwrap();
        assert!(record_fix(&db, owner, &key, 777).await.is_err());
        let current = binding(&db, owner, 13).await.unwrap();
        assert_eq!(record_fix(&db, owner, &current, 777).await.unwrap(), issue);
        sqlx::query("DELETE FROM github_connections WHERE owner_id=$1")
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM issues WHERE id=$1 AND project_id=$2"
            )
            .bind(issue)
            .bind(project)
            .fetch_one(&db)
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM github_fixes")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn denied_private_alert_hides_home_details_while_authorized_pr_stays_visible(db: PgPool) {
        use axum::{Json, extract::Request, http::StatusCode, routing::get};
        use serde_json::json;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base =
            reqwest::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let provider = super::super::provider::Provider::fixture_at(&base);
        let api=Router::new().fallback(get(|req:Request|async move {
            if req.headers().get("authorization").and_then(|s|s.to_str().ok())!=Some("Bearer fixture-user-token") {
                return (StatusCode::UNAUTHORIZED,Json(json!({})));
            }
            let data=match req.uri().path() {
                "/user/installations"=>json!({"installations":[{"id":88,"app_slug":"workspace","permissions":{"metadata":"read","pull_requests":"read","vulnerability_alerts":"read"}}]}),
                "/user/installations/88/repositories"=>json!({"repositories":[{"id":13,"full_name":"org/public-project"}]}),
                "/repos/org/public-project/dependabot/alerts"=>return (StatusCode::FORBIDDEN,Json(json!({"message":"Private alerts"}))),
                "/repos/org/public-project/pulls/3"=>json!({"number":3,"title":"Normal maintenance PR","state":"closed","body":"Private PR body","merged":true,"base":{"repo":{"id":13}}}),
                _=>return (StatusCode::NOT_FOUND,Json(json!({}))),
            };
            (StatusCode::OK,Json(data))
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, api).await.unwrap();
        });
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('alert@example.invalid') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let actor = Auth {
            user_id: owner,
            email: "alert@example.invalid".into(),
            csrf: String::new(),
        };
        let state = AppState::for_test(db.clone());
        let project:Uuid=sqlx::query_scalar("INSERT INTO projects(owner_id,name,key) VALUES($1,'Public project','OPEN') RETURNING id").bind(owner).fetch_one(&db).await.unwrap();
        sqlx::query(
            "INSERT INTO github_connections(owner_id,user_remote_id,access_cipher) VALUES($1,1,$2)",
        )
        .bind(owner)
        .bind(provider.seal(owner, "fixture-user-token").unwrap())
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO github_repos(owner_id,repo_id,project_id,installation_id,display_name,repo_grant_until,alert_grant_until,last_success) VALUES($1,13,$2,88,'org/public-project',now()+interval '4 minutes',now()+interval '4 minutes',now())").bind(owner).bind(project).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO github_alerts(owner_id,repo_id,number,package,manifest,updated_at) VALUES($1,13,27,'private-package','private/path',now())").bind(owner).execute(&db).await.unwrap();
        let key = binding(&db, owner, 13).await.unwrap();
        let issue = record_fix(&db, owner, &key, 27).await.unwrap();
        sqlx::query(
            "INSERT INTO github_pulls(owner_id,repo_id,number,issue_id) VALUES($1,13,3,$2)",
        )
        .bind(owner)
        .bind(issue)
        .execute(&db)
        .await
        .unwrap();
        assert!(
            home_db(&db, &actor, Some(project), true)
                .await
                .into_string()
                .contains("private-package")
        );
        // Pasted URLs follow the stable repository identity, including renames.
        sqlx::query(
            "UPDATE github_repos SET display_name='org/previous' WHERE owner_id=$1 AND repo_id=13",
        )
        .bind(owner)
        .execute(&db)
        .await
        .unwrap();
        let current = parse_pull_url("https://github.com/org/public-project/pull/3").unwrap();
        let (mapped, _) = resolve_pull_with(&state, owner, Some(project), &current, &provider)
            .await
            .unwrap();
        assert_eq!(mapped.repo_id, 13);
        let former = parse_pull_url("https://github.com/org/previous/pull/3").unwrap();
        assert!(matches!(
            resolve_pull_with(&state, owner, Some(project), &former, &provider).await,
            Err(AppError::NotFound)
        ));
        let (_, cards) = links(&state, &actor, issue, Some(&provider)).await.unwrap();
        let html = html! {@for card in cards { (card) }}.into_string();
        for visible in ["Normal maintenance PR", "Merged", "/pull/3/files"] {
            assert!(html.contains(visible), "{html}");
        }
        for hidden in [
            "Private PR body",
            "private-package",
            "private/path",
            "dependabot/27",
            "Critical dependency alert",
        ] {
            assert!(!html.contains(hidden));
        }
        assert_eq!(
            coverage_db(&db, owner, project, true).await.unwrap(),
            (0, 1, 0)
        );
        let home = home_db(&db, &actor, None, true).await.into_string();
        for hidden in [
            "org/public-project",
            "private-package",
            "private/path",
            "dependabot/27",
            "Open fix",
        ] {
            assert!(!home.contains(hidden));
        }
        let resume = resume_source(&state, &actor, issue, Some(&provider))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resume.title, "Normal maintenance PR");
        assert_eq!(resume.url, "https://github.com/org/public-project/pull/3");
        server.abort();
    }

    #[test]
    fn parse_only_github_pull_urls() {
        for input in [
            "https://github.com/org/app/pull/1",
            "https://github.com/org/app/pull/1/files#fragment",
            "https://github.com/org/app/pull/1/?tab=overview",
        ] {
            let parsed = parse_pull_url(input).unwrap();
            assert_eq!((parsed.repo.as_str(), parsed.number), ("org/app", 1));
        }
        for input in [
            "http://github.com/org/app/pull/1",
            "https://github.com.evil.test/org/app/pull/1",
            "https://github.com@evil.test/org/app/pull/1",
            "https://me@github.com/org/app/pull/1",
            "https://github.com/org/app/issues/1",
            "https://github.com/org/app/pull/0",
            "https://github.com/org/app/pull/-1",
            "https://github.com/o%2fr/app/pull/1",
            "https://github.com/org/app/pull/12/files/anything",
        ] {
            assert!(parse_pull_url(input).is_err(), "{input}");
        }
    }
}
