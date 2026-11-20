use super::{Command, configured, credentials, provider::Repo, references};
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
    render(&s, &auth, None, is_desktop_client(&headers)).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserTarget {
    owner: Uuid,
}

fn handoff_path(owner: Uuid) -> String {
    format!("/integrations/github/browser?owner={owner}")
}

fn authorize_control(auth: &Auth, desktop: bool, label: &str) -> Markup {
    if desktop {
        html! { a.button href=(handoff_path(auth.user_id)) target="_blank" rel="noopener noreferrer" { "Manage GitHub in browser" } }
    } else {
        html! { form method="post" action="/integrations/github/connect" { (ui::csrf(auth)) button { (label) } } }
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
        return (StatusCode::FORBIDDEN, ui::public_page("Switch Working Set account", html! {
            h1 { "Switch Working Set account" }
            p { "This browser is signed in as " strong { (auth.email) } ". To connect GitHub, sign in with the account open in your desktop app." }
            a.button href=(format!("/login?{query}")) { "Sign in with another account" }
        })).into_response();
    }
    // The browser creates its own OAuth state only after the user confirms this account.
    ui::public_page(
        "Connect GitHub",
        html! {
            h1 { "Connect GitHub" }
            p { "Authorize GitHub for the Working Set account " strong { (auth.email) } "." }
            (authorize_control(&auth, is_desktop_client(&headers), "Continue to GitHub"))
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
    render(&s, &auth, Some(repos), is_desktop_client(&headers)).await
}

fn projects(projects: &[store::Project], selected: Option<Uuid>) -> Markup {
    html! {label { "Workspace project" select name="project" required {
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
) -> AppResult<Html<String>> {
    let p = configured(s).ok();
    type ConnectionExpiry = (Option<DateTime<Utc>>, Option<DateTime<Utc>>, bool);
    let connection: Option<ConnectionExpiry> = sqlx::query_as(
        "SELECT expires_at,refresh_expires_at,refresh_cipher IS NOT NULL FROM github_connections WHERE owner_id=$1")
        .bind(auth.user_id).fetch_optional(&s.db).await?;
    let mappings:Vec<Mapping>=sqlx::query_as(
        "SELECT r.id,r.project_id,
        CASE WHEN r.repo_grant_until>now() AND (c.expires_at IS NULL OR c.expires_at>now()) AND NOT r.billing_paused
             THEN NULLIF(r.display_name,'') END AS name, r.billing_paused,r.last_success,r.last_attempt,
        COALESCE(r.alert_grant_until>now() AND (c.expires_at IS NULL OR c.expires_at>now()) AND r.failures=0
          AND r.last_success>now()-interval '2 hours',false) AS available,r.failures
        FROM github_repos r JOIN github_connections c USING(owner_id)
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
    let body = html! {
        h1 {"Integrations"}
        section.panel {
            div.row { h2 {"GitHub"} span.status {
                @if p.is_none() {"Unavailable"} @else if expired {"Reconnect required"} @else if connected {"Authorized"} @else {"Disconnected"}
            } }
            p {"Connect selected repositories to check critical Dependabot alerts and link pull requests to issues."}
            @if let Some(provider)=p.as_ref() {
                @if connected {
                    div.toolbar {
                        form method="post" action="/integrations/github/repositories" { (ui::csrf(auth)) button {"Choose repositories"} }
                        a href=(provider.install_url()) target="_blank" rel="noopener noreferrer" {"Manage installation on GitHub"}
                    }
                    details { summary {"Manage authorization"}
                        (authorize_control(auth, desktop, "Authorize again"))
                        (disconnect_action(auth))
                    }
                } @else {
                    div.toolbar {
                        a class="button secondary" href=(provider.install_url()) target="_blank" rel="noopener noreferrer" {"Install the GitHub App"}
                        (authorize_control(auth, desktop, "Authorize with GitHub"))
                    }
                    p.muted {"Install the app on the repositories you want to connect, then authorize your GitHub account."}
                }
            } @else {
                p.notice {"The server operator must configure the read-only GitHub App before repositories can be connected or checked."}
                @if connected {(disconnect_action(auth))}
            }
        }
        @if let Some(repos)=choices {
            section.panel {
                h2 {"Choose a repository"}
                @if !any_project { p { "Create an active project before connecting a repository. " a href="/projects" {"Open projects"} } }
                @else if repos.is_empty(){ p {"Your GitHub user grant currently includes no repositories from this app. Check the installation’s selected repositories."} }
                @else {
                    @for repo in repos {
                        article {
                            h3 {(repo.name)}
                            @if assignments.contains(&repo.id) {p.muted {"Already mapped in this workspace."}}
                            @else {
                                form method="post" action="/integrations/github/link" {
                                    (ui::csrf(auth))
                                    input type="hidden" name="repo_id" value=(repo.id);
                                    input type="hidden" name="installation_id" value=(repo.installation);
                                    (projects(&projects_list,None))
                                    button {"Connect repository"}
                                }
                            }
                        }
                    }
                }
            }
        }
        @if connected {
            section.panel { h2 {"Repository coverage"}
                p.muted {(active) " of " (store::limits(&plan).repos) " active repository connections"}
                @if mappings.is_empty() {p {"Choose a repository to start its first check."}}
                @for r in mappings {
                    @let base=format!("/integrations/github/repositories/{}",r.id);
                    article {
                        h3 {
                            @if p.is_some(){(r.name.as_deref().unwrap_or("Repository"))} @else {"Repository"}
                        }
                        p { (projects_list.iter().find(|pr|pr.id==r.project_id).map(|pr|pr.name.as_str()).unwrap_or("Workspace project"))
                            " · "
                            @if r.billing_paused {"Paused"}
                            @else if p.is_none() {"Unavailable"}
                            @else if r.available {"Checked"}
                            @else if r.last_success.is_some_and(|v|v<Utc::now()-chrono::Duration::hours(2)) {"Stale"}
                            @else if r.last_attempt.is_none() {"Awaiting first check"}
                            @else if r.failures>0 {"Check unavailable"}
                            @else {"Awaiting access verification"}
                        }
                        @if let Some(t)=r.last_success {p.muted {"Last complete check " time datetime=(t.to_rfc3339()) {(t.format("%Y-%m-%d %H:%M UTC"))}}}
                        @if p.is_some() && !r.billing_paused {
                            form method="post" action=(format!("{base}/refresh")) { (ui::csrf(auth)) button.secondary {"Refresh"} }
                        }
                        details {
                            summary {"Manage mapping"}
                            @if p.is_some() && any_project {
                                form method="post" action=(format!("{base}/map")) {
                                    (ui::csrf(auth)) (projects(&projects_list,Some(r.project_id)))
                                    button.secondary { @if r.billing_paused {"Resume in project"} @else {"Save project mapping"} }
                                }
                            }
                            form method="post" action=(format!("{base}/remove")) {
                                (ui::csrf(auth)) p.muted {"Removing this connection deletes its imported data and preserves your local issues."}
                                button.danger {"Remove repository"}
                            }
                        }
                    }
                }
            }
            @if p.is_some() && active>0 { (references::form(auth)) }
        }
    };
    Ok(ui::page(auth, "Integrations", "integrations", body))
}

fn disconnect_action(auth: &Auth) -> Markup {
    html! {form method="post" action="/integrations/github/disconnect" {
        (ui::csrf(auth))
        p.muted {"Disconnecting removes stored GitHub credentials, source details and links. Your issues and notes stay in your workspace."}
        button.danger {"Disconnect GitHub"}
    }}
}
