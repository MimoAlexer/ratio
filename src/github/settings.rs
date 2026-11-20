use super::{
    Command, configured, credentials,
    provider::{Provider, Repo},
    references,
};
use crate::{
    AppResult, AppState,
    auth::{Auth, is_desktop_client},
    store, ui,
};
use axum::{
    Form, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::FromRow;
use uuid::Uuid;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/integrations", get(index))
        .route("/integrations/github/browser", get(browser_handoff))
        .route("/integrations/github/repositories", post(discover))
}

#[derive(FromRow)]
struct Mapping {
    id: Uuid,
    project_id: Uuid,
    name: Option<String>,
    billing_paused: bool,
    project_archived: bool,
    last_success: Option<DateTime<Utc>>,
    last_attempt: Option<DateTime<Utc>>,
    available: bool,
    failures: i32,
}

async fn index(
    State(s): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    render(
        &s,
        &auth,
        None,
        is_desktop_client(&headers),
        configured(&s).ok().as_ref(),
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserTarget {
    owner: Uuid,
}

fn handoff_path(owner: Uuid) -> String {
    format!("/integrations/github/browser?owner={owner}")
}

fn authorize_control(auth: &Auth, desktop: bool, label: &str, primary: bool) -> Markup {
    let style = if primary {
        "action-primary"
    } else {
        "action-secondary"
    };
    if desktop {
        html! {
            a class=(style) href=(handoff_path(auth.user_id)) target="_blank" rel="noopener noreferrer" {
                "Manage GitHub in browser"
            }
        }
    } else {
        html! {
            form method="post" action="/integrations/github/connect" {
                (ui::csrf(auth)) button type="submit" class=(style) { (label) }
            }
        }
    }
}

async fn browser_handoff(
    auth: Auth,
    Query(target): Query<BrowserTarget>,
    headers: HeaderMap,
) -> Response {
    if target.owner != auth.user_id {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("next", &handoff_path(target.owner))
            .finish();
        return (StatusCode::FORBIDDEN, ui::public_page("Switch Ratio account", html! {
            h1 { "Switch Ratio account" }
            p { "This browser is signed in as " strong { (auth.email) } ". To connect GitHub, sign in with the account open in your desktop app." }
            a.action-primary href=(format!("/login?{query}")) { "Sign in with another account" }
        })).into_response();
    }
    // The browser creates its own OAuth state only after the user confirms this account.
    ui::public_page(
        "Connect GitHub",
        html! {
            h1 { "Connect GitHub" }
            p { "Authorize GitHub for the Ratio account " strong { (auth.email) } "." }
            (authorize_control(&auth, is_desktop_client(&headers), "Continue to GitHub", true))
        },
    )
    .into_response()
}

async fn discover(
    State(s): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Form(f): Form<Command>,
) -> AppResult<Html<String>> {
    auth.check_csrf(&f.csrf)?;
    let p = configured(&s)?;
    let epoch = super::grant(&s.db, auth.user_id).await?;
    let access = credentials(&s, &p, auth.user_id).await?;
    let repos = super::catalog(&s, &p, auth.user_id, &access).await?;
    if super::grant(&s.db, auth.user_id).await? != epoch {
        return Err(crate::AppError::Forbidden);
    }
    render(
        &s,
        &auth,
        Some(repos),
        is_desktop_client(&headers),
        Some(&p),
    )
    .await
}

fn projects(projects: &[store::Project], selected: Option<Uuid>, repository: &str) -> Markup {
    html! {label { "Workspace project" select name="project" aria-label=(format!("Workspace project for {repository}")) required {
        @for p in projects.iter().filter(|p|!p.archived) {
            option value=(p.id) selected[selected==Some(p.id)] {(p.name)}
        }
    } }}
}

async fn render(
    s: &AppState,
    auth: &Auth,
    choices: Option<Vec<Repo>>,
    desktop: bool,
    provider: Option<&Provider>,
) -> AppResult<Html<String>> {
    type ConnectionExpiry = (Option<DateTime<Utc>>, Option<DateTime<Utc>>, bool);
    let connection: Option<ConnectionExpiry> = sqlx::query_as(
        "SELECT expires_at,refresh_expires_at,refresh_cipher IS NOT NULL FROM github_connections WHERE owner_id=$1")
        .bind(auth.user_id).fetch_optional(&s.db).await?;
    let mappings:Vec<Mapping>=sqlx::query_as(
        "SELECT r.id,r.project_id,
        CASE WHEN r.repo_grant_until>now() AND (c.expires_at IS NULL OR c.expires_at>now()) AND NOT r.billing_paused
             THEN NULLIF(r.display_name,'') END AS name, r.billing_paused,p.archived AS project_archived,
        r.last_success,r.last_attempt,
        COALESCE(r.repo_grant_until>now() AND r.alert_grant_until>now() AND (c.expires_at IS NULL OR c.expires_at>now()) AND r.failures=0
          AND r.last_success>=now()-interval '2 hours',false) AS available,r.failures
        FROM github_repos r JOIN github_connections c USING(owner_id)
        JOIN projects p ON p.owner_id=r.owner_id AND p.id=r.project_id
        WHERE r.owner_id=$1 ORDER BY r.project_id,r.id")
        .bind(auth.user_id).fetch_all(&s.db).await?;
    let projects_list = store::projects(&s.db, auth.user_id).await?;
    let plan: String = sqlx::query_scalar("SELECT plan FROM users WHERE id=$1")
        .bind(auth.user_id)
        .fetch_one(&s.db)
        .await?;
    let connected = connection.is_some();
    let expired = connection.is_some_and(|(access, refresh, has_refresh)| {
        access.is_some_and(|t| t <= Utc::now())
            && (!has_refresh || refresh.is_some_and(|t| t <= Utc::now()))
    });
    let active = mappings.iter().filter(|v| !v.billing_paused).count();
    let any_project = projects_list.iter().any(|p| !p.archived);
    let assignments: Vec<i64> = if choices.is_some() {
        sqlx::query_scalar("SELECT repo_id FROM github_repos WHERE owner_id=$1")
            .bind(auth.user_id)
            .fetch_all(&s.db)
            .await?
    } else {
        vec![]
    };
    let repo_limit = store::limits(&plan).repos;
    let at_limit = active as i64 >= repo_limit;
    let ready = provider.is_some() && connected && !expired;
    let body = html! {
        div.github-settings {
            header.view-head {
                div {
                    h1 { "GitHub" }
                    p { "Link pull requests to issues and check critical Dependabot alerts." }
                }
            }
            section.github-section aria-labelledby="github-account" {
                h2 id="github-account" { "Account connection" }
                div.github-section-body {
                    @if let Some(provider) = provider {
                        @if expired {
                            h3 { "Authorization expired" }
                            p { "Reconnect your GitHub account to resume repository checks." }
                            (authorize_control(auth, desktop, "Reconnect GitHub", true))
                        } @else if connected {
                            h3 { "Account authorized" }
                            @if choices.is_none() {
                                p { "Choose from the repositories shared with the GitHub App." }
                                form method="post" action="/integrations/github/repositories" {
                                    (ui::csrf(auth)) button.action-primary type="submit" { "Choose repositories" }
                                }
                            }
                        } @else {
                            h3 { "Account disconnected" }
                            p {
                                a href=(provider.install_url()) target="_blank" rel="noopener noreferrer" { "Install the GitHub App" }
                                " on the repositories you want to use, then authorize your account."
                            }
                            (authorize_control(auth, desktop, "Authorize with GitHub", true))
                        }
                        @if connected {
                            details.github-manage {
                                summary { "Manage authorization" }
                                div.github-actions {
                                    a href=(provider.install_url()) target="_blank" rel="noopener noreferrer" { "Manage installation on GitHub" }
                                    @if !expired { (authorize_control(auth, desktop, "Authorize again", false)) }
                                }
                                (disconnect_action(auth))
                            }
                        }
                    } @else {
                        h3 { "Server setup required" }
                        p { "A server operator must configure the read-only GitHub App before Ratio can connect or check repositories." }
                        @if connected {
                            details.github-manage { summary { "Manage authorization" } (disconnect_action(auth)) }
                        }
                    }
                }
            }
            @if let Some(repos) = &choices {
                section.github-section aria-labelledby="github-catalog" {
                    h2 id="github-catalog" { "Choose a repository" }
                    div.github-section-body {
                        @if !any_project {
                            p { "Create an active Ratio project to connect a repository." }
                            a.action-primary href="/projects" { "Open projects" }
                        } @else if repos.is_empty() {
                            h3 { "No repositories shared" }
                            p { "Select repositories in the GitHub App installation to make them available here." }
                            @if let Some(provider) = provider {
                                a.action-primary href=(provider.install_url()) target="_blank" rel="noopener noreferrer" { "Manage installation on GitHub" }
                            }
                        } @else {
                            @if at_limit {
                                p { "Your plan’s active repository limit has been reached. " a href="/billing" { "Review plan limits" } }
                            }
                            ul.github-records {
                                @for repo in repos {
                                    li {
                                        h3 { (&repo.name) }
                                        @if !repo.can_read_alerts { p { "This installation lacks Dependabot alert access." } }
                                        @if assignments.contains(&repo.id) {
                                            p { "Already connected to this workspace." }
                                        } @else if !at_limit {
                                            form.github-assignment method="post" action="/integrations/github/link" {
                                                (ui::csrf(auth))
                                                input type="hidden" name="repo_id" value=(repo.id);
                                                input type="hidden" name="installation_id" value=(repo.installation);
                                                (projects(&projects_list, None, &repo.name))
                                                button.action-secondary type="submit" aria-label=(format!("Connect repository {}", repo.name)) { "Connect repository" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            @if connected {
                section.github-section aria-labelledby="github-coverage" {
                    div {
                        h2 id="github-coverage" { "Repository coverage" }
                        p.github-capacity { (active) " of " (repo_limit) " active" }
                    }
                    div.github-section-body {
                        @if mappings.is_empty() {
                            h3 { "No connected repositories" }
                            @if ready && choices.is_none() { p { "Choose a repository and assign it to a Ratio project to start its first check." } }
                        } @else {
                            ul.github-records {
                                @for r in &mappings {
                                    @let base = format!("/integrations/github/repositories/{}", r.id);
                                    @let fallback = format!("Repository {}", &r.id.to_string()[..8]);
                                    @let repo = if provider.is_some() { r.name.as_deref().unwrap_or(&fallback) } else { &fallback };
                                    @let project = projects_list.iter().find(|project| project.id == r.project_id);
                                    li {
                                        h3 { (repo) }
                                        @if let Some(project) = project {
                                            a.github-project href=(format!("/projects/{}", project.id)) { (&project.name) }
                                        }
                                        p.github-coverage-state { (coverage_state(r, ready)) }
                                        @if let Some(t) = r.last_success {
                                            p.github-check-time { "Last complete check "
                                                time datetime=(t.to_rfc3339()) { (t.format("%b %-d, %Y · %H:%M UTC")) }
                                            }
                                        }
                                        @if ready && !r.billing_paused && !r.project_archived {
                                            form method="post" action=(format!("{base}/refresh")) {
                                                (ui::csrf(auth))
                                                button.action-secondary type="submit" aria-label=(format!("Refresh check for {repo}")) { "Refresh check" }
                                            }
                                        }
                                        details.github-manage {
                                            summary { "Manage mapping" }
                                            @if ready && any_project && (!r.billing_paused || !at_limit) {
                                                form.github-assignment method="post" action=(format!("{base}/map")) {
                                                    (ui::csrf(auth)) (projects(&projects_list, Some(r.project_id), repo))
                                                    button.action-secondary type="submit" { @if r.billing_paused { "Resume in project" } @else { "Save project mapping" } }
                                                }
                                            } @else if ready && r.billing_paused && at_limit {
                                                p { "Resuming this connection requires an available repository slot. " a href="/billing" { "Review plan limits" } }
                                            }
                                            form.github-remove method="post" action=(format!("{base}/remove")) {
                                                (ui::csrf(auth)) p { "Removing this connection deletes imported repository data. Your local issues are preserved." }
                                                button.danger type="submit" { "Remove repository" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                @if ready && mappings.iter().any(|r| !r.billing_paused && !r.project_archived) {
                    div.github-references { (references::form(auth)) }
                }
            }
        }
    };
    Ok(ui::section_page(auth, "GitHub", "integrations", body, None))
}

fn coverage_state(mapping: &Mapping, ready: bool) -> &'static str {
    if mapping.billing_paused {
        "Paused by plan limits"
    } else if mapping.project_archived {
        "Paused · project archived"
    } else if !ready {
        "Checks unavailable"
    } else if mapping.available {
        "Alert coverage verified"
    } else if mapping.last_attempt.is_none() {
        "Awaiting first check"
    } else if mapping.failures > 0 && mapping.name.is_some() {
        "Repository verified · alert check unavailable"
    } else if mapping.failures > 0 {
        "Check unavailable"
    } else if mapping
        .last_success
        .is_some_and(|t| t < Utc::now() - chrono::Duration::hours(2))
    {
        "Last complete check is stale"
    } else {
        "Awaiting access verification"
    }
}

fn disconnect_action(auth: &Auth) -> Markup {
    html! {form.github-remove method="post" action="/integrations/github/disconnect" {
        (ui::csrf(auth))
        p {"Disconnecting removes stored GitHub credentials, source details and links. Your issues and notes stay in your workspace."}
        button.danger type="submit" {"Disconnect GitHub"}
    }}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn settings_distinguish_partial_checks_and_hide_source_names_after_access_expires(
        db: sqlx::PgPool,
    ) {
        let email = "github-settings@example.invalid";
        let owner: Uuid = sqlx::query_scalar("INSERT INTO users(email) VALUES($1) RETURNING id")
            .bind(email)
            .fetch_one(&db)
            .await
            .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES($1,'Fixture','GH') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO github_connections(owner_id,user_remote_id,access_cipher,expires_at)
                     VALUES($1,41,'fixture',now()+interval '1 hour')",
        )
        .bind(owner)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO github_repos(owner_id,project_id,repo_id,installation_id,display_name,
                     repo_grant_until,alert_grant_until,last_attempt,last_success,failures)
                     VALUES($1,$2,51,61,'owner/private',now()+interval '4 minutes',NULL,now(),now(),1)")
            .bind(owner).bind(project).execute(&db).await.unwrap();
        let auth = Auth {
            user_id: owner,
            email: email.into(),
            csrf: "a".repeat(64),
        };
        let state = AppState::for_test(db.clone());
        let provider = Provider::fixture_at(&url::Url::parse("http://127.0.0.1").unwrap());
        let page = render(&state, &auth, None, false, Some(&provider))
            .await
            .unwrap()
            .0;
        assert!(page.contains("owner/private"));
        assert!(page.contains("Repository verified · alert check unavailable"));
        assert!(!page.contains("Alert coverage verified"));
        let unavailable = render(&state, &auth, None, false, None).await.unwrap().0;
        assert!(unavailable.contains("Server setup required"));
        assert!(!unavailable.contains("owner/private"));
        sqlx::query("UPDATE github_repos SET repo_grant_until=NULL,alert_grant_until=now()+interval '4 minutes',failures=0 WHERE owner_id=$1")
            .bind(owner).execute(&db).await.unwrap();
        let page = render(&state, &auth, None, false, Some(&provider))
            .await
            .unwrap()
            .0;
        assert!(!page.contains("owner/private"));
        assert!(!page.contains("Alert coverage verified"));
        sqlx::query(
            "UPDATE github_connections SET expires_at=now()-interval '1 hour' WHERE owner_id=$1",
        )
        .bind(owner)
        .execute(&db)
        .await
        .unwrap();
        let page = render(&state, &auth, None, false, Some(&provider))
            .await
            .unwrap()
            .0;
        assert!(page.contains("Authorization expired"));
        assert!(page.contains("Reconnect GitHub"));
        assert!(!page.contains("Choose repositories"));
        assert!(!page.contains("owner/private"));
    }
}
