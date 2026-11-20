use crate::auth::Auth;
use axum::response::Html;
use maud::{DOCTYPE, Markup, html};

pub fn csrf(auth: &Auth) -> Markup {
    html! { input type="hidden" name="csrf" value=(&auth.csrf); }
}

pub fn page(auth: &Auth, title: &str, active: &str, body: Markup) -> Html<String> {
    type NavItem = (&'static str, &'static str, &'static str);
    type NavGroup = (&'static str, &'static [NavItem]);
    let groups: &[NavGroup] = &[
        (
            "Workspace",
            &[
                ("home", "/", "Home"),
                ("projects", "/projects", "Projects"),
                ("issues", "/issues", "Issues"),
                ("notes", "/notes", "Notes"),
            ],
        ),
        (
            "Planning",
            &[
                ("roadmap", "/roadmap", "Roadmap"),
                ("releases", "/releases", "Releases"),
                ("insights", "/insights", "Insights"),
                ("health", "/health", "Project health"),
            ],
        ),
        (
            "Account",
            &[
                ("integrations", "/integrations", "GitHub"),
                ("transfer", "/transfer", "Import & export"),
                ("workflows", "/workflows", "Workflows"),
                ("billing", "/billing", "Plans & billing"),
                ("settings", "/settings", "Settings"),
            ],
        ),
    ];
    Html(html!{
        (DOCTYPE) html lang="en" { head { meta charset="utf-8"; meta name="viewport" content="width=device-width, initial-scale=1"; title {(title) " · Workspace"} link rel="stylesheet" href="/assets/app.css"; script defer src="/assets/workspace.js" {} link rel="icon" type="image/svg+xml" href="/assets/mark.svg"; }
        body { a class="skip-link" href="#main" {"Skip to content"}
          div class="app-shell" {
            aside class="sidebar" {
               a class="brand" href="/" aria-label="Workspace home" { img src="/assets/mark.svg" alt="" width="28" height="28"; span {"Workspace"} }
               nav class="nav" aria-label="Workspace" {
                  @for (group,items) in groups { div class="nav-group" { p class="nav-heading" {(group)}
                    @for (key,path,name) in *items {
                       @if *key==active {a class="nav-link" href=(path) aria-current="page" {(name)}} @else {a class="nav-link" href=(path) {(name)}}
                    }
                  } }
               }
               div class="workspace-account" { span { (&auth.email) } button type="button" data-shortcuts-help hidden {"Keyboard shortcuts"} form method="post" action="/logout" { (csrf(auth)) button class="secondary" type="submit" {"Sign out"} } }
            }
            main id="main" class="main" {div class="page-content" {@if auth.user_id==crate::sample::DEMO_OWNER {p class="notice" {"Demo workspace · Sample data"}} (body)}}
          }
        }}
    }.into_string())
}

pub fn public_page(title: &str, body: Markup) -> Html<String> {
    Html(html!{(DOCTYPE) html lang="en" {head{meta charset="utf-8";meta name="viewport" content="width=device-width,initial-scale=1";title{(title)" · Workspace"}link rel="stylesheet" href="/assets/app.css";}body{main class="page-content" {header {a class="brand" href="/welcome"{img src="/assets/mark.svg" alt="" width="28" height="28";"Workspace"}}(body)}}}}.into_string())
}
