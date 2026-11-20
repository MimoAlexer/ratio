use crate::auth::Auth;
use axum::response::Html;
use maud::{DOCTYPE, Markup, html};
use uuid::Uuid;

type NavItem = (&'static str, &'static str, &'static str);
type NavGroup = (&'static str, &'static [NavItem]);

const GROUPS: &[NavGroup] = &[
    (
        "Workspace",
        &[
            ("home", "/", "Home"),
            ("inbox", "/issues?status=backlog", "Inbox"),
            ("issues", "/issues", "Issues"),
            ("notes", "/notes", "Notes"),
            ("projects", "/projects", "Projects"),
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
        "Manage",
        &[
            ("integrations", "/integrations", "GitHub integration"),
            ("transfer", "/transfer", "Import & export"),
            ("workflows", "/workflows", "Workflows"),
            ("billing", "/billing", "Plans & billing"),
            ("settings", "/settings", "Settings"),
        ],
    ),
];

pub(crate) fn format_bytes(size: i64) -> String {
    for (unit, name) in [(1_000_000_000, "GB"), (1_000_000, "MB"), (1_000, "kB")] {
        if size >= unit {
            let formatted = format!("{:.1}", size as f64 / unit as f64);
            return format!("{} {name}", formatted.trim_end_matches(".0"));
        }
    }
    format!("{size} B")
}

pub fn csrf(auth: &Auth) -> Markup {
    html! { input type="hidden" name="csrf" value=(&auth.csrf); }
}

/// Fixed, single-stroke interface symbols. Each control supplies its own accessible name.
pub(crate) fn icon(name: &str) -> Markup {
    let path = match name {
        "inbox" => "M3 13 6.5 5h11l3.5 8v6H3zM3 13h5l1.8 2h4.4l1.8-2h5",
        "home" => "M3.5 8.7 12 2.8l8.5 5.9v11H15v-6H9v6H3.5z",
        "issues" => "M20 12a8 8 0 1 1-16 0 8 8 0 0 1 16 0ZM9 12h6",
        "notes" => "M5 3h10l4 4v14H5zM15 3v5h4M8.5 12h7M8.5 16h5",
        "projects" => "M2.8 7V5.5h6.4l2 2h9.5v11.3H2.8z",
        "roadmap" => "M4 4v16h17M7 7h7M11 11h8M8 15h6",
        "releases" => "m3 4 8.5.1 9 9-7.5 7.5-9.7-9.7zM7.4 8h.2",
        "insights" => "M3 4v15.5h18M6 14l4.8-5 3.6 2.8 5.6-7",
        "health" => "M2 12h5l3-7 4.5 14 3-7H22",
        "integrations" => {
            "M7 6.8v9.3M8.8 14c4 0 7.5-2.4 7.5-6.2M9.5 4.5a2.5 2.5 0 1 1-5 0 2.5 2.5 0 1 1 5 0ZM18.8 5.4a2.5 2.5 0 1 1-5 0 2.5 2.5 0 1 1 5 0ZM9.5 18.5a2.5 2.5 0 1 1-5 0 2.5 2.5 0 1 1 5 0Z"
        }
        "transfer" => "M3 7h15m-4-4 4 4-4 4M21 17H6m4-4-4 4 4 4",
        "workflows" => "M5 3.5h5v5H5zM14 15.5h5v5h-5zM7.5 8.5v7a2.5 2.5 0 0 0 2.5 2.5h4M10 6h7",
        "billing" => "M3 5h18v14H3zM3 10h18M6.5 15h4",
        "settings" => "M3 7h5m5 0h8M3 17h9m5 0h4M8 4.5h5v5H8zM12 14.5h5v5h-5z",
        "search" => "M15.5 9.5a6 6 0 1 1-12 0 6 6 0 1 1 12 0Zm-1.5 4.6 6.2 6.2",
        "arrow" => "M4 12h15m-6-6 6 6-6 6",
        "down" => "M12 4v15m-5-5 5 5 5-5",
        "corner" => "M6 5v7a3 3 0 0 0 3 3h10m-4-4 4 4-4 4",
        "resume" => "M5 4v16M10 5l10 7-10 7z",
        "flag" => "M5 21V3m0 1c4-3 9 3 14 0v9c-5 3-10-3-14 0",
        "pause" => "M6 4h3.5v16H6zM14.5 4H18v16h-3.5z",
        "menu" => "M3 6h18M3 12h18M3 18h18",
        "chevron" => "m8 10 4 4 4-4",
        "help" => {
            "M20.5 12a8.5 8.5 0 1 1-17 0 8.5 8.5 0 1 1 17 0ZM9.5 9.5a2.5 2.5 0 1 1 3.8 2.1c-.9.6-1.3 1-1.3 2.3M12 16.8h.01"
        }
        "logout" => "M10 4H4v16h6M9 12h11m-5-5 5 5-5 5",
        "user" => "M15.5 8a3.5 3.5 0 1 1-7 0 3.5 3.5 0 1 1 7 0ZM4.5 20a7.5 7.5 0 0 1 15 0",
        "theme" => "M20.5 12a8.5 8.5 0 1 1-17 0 8.5 8.5 0 1 1 17 0ZM12 3.5v17",
        "edit" => "m14.5 5.5 4 4M4 20l4.4-.9L20 7.5l-4-4L4.9 15.1z",
        "plus" => "M12 4v16M4 12h16",
        "shell" => "M3 4.5h18v15H3zM8.2 4.5v15",
        _ => return html! {},
    };
    html! {
        svg.ui-icon viewBox="0 0 24 24" width="18" height="18" fill="none"
            stroke="currentColor" stroke-width="1.6" stroke-linecap="round"
            stroke-linejoin="round" aria-hidden="true" focusable="false" { path d=(path) {} }
    }
}

fn brand(href: &str) -> Markup {
    html! {
        a.brand href=(href) aria-label="Ratio home" {
            img src="/assets/mark.svg" width="28" height="28" alt="";
            span { "Ratio" }
        }
    }
}

fn nav_items(items: &[NavItem], active: &str) -> Markup {
    html! {
        @for &(key, path, label) in items {
            a.nav-link href=(path) aria-current=[(key == active).then_some("page")] {
                (icon(key)) span { (label) }
            }
            @if key == "projects" {
                div.project-links data-sidebar-projects hidden {}
            }
        }
    }
}

fn navigation(auth: &Auth, active: &str) -> Markup {
    let email = auth.email.as_str();
    let (name, domain) = email.split_once('@').unwrap_or((email, ""));
    let initial = name
        .chars()
        .next()
        .unwrap_or('R')
        .to_uppercase()
        .to_string();
    html! {
        nav.nav aria-label="Workspace navigation" {
            @for &(group, items) in GROUPS {
                @if group == "Manage" {
                    details.nav-manage open[items.iter().any(|(key, _, _)| *key == active)] {
                        summary.nav-heading { span { (group) } (icon("chevron")) }
                        (nav_items(items, active))
                    }
                } @else {
                    section.nav-group aria-label=(group) {
                        p.nav-heading { (group) }
                        (nav_items(items, active))
                    }
                }
            }
        }
        div.workspace-account {
            details.account-menu {
                summary.account-summary aria-label=(format!("Account options for {email}")) title=(email) {
                    span.account-icon aria-hidden="true" { (initial) }
                    span.account-identity {
                        strong { (name) }
                        @if !domain.is_empty() { small { (domain) } }
                    }
                    (icon("chevron"))
                }
                div.account-actions {
                    p.account-address { (email) }
                    button.account-action type="button" data-theme-toggle hidden
                        title="Monochrome theme" aria-pressed="false" {
                        (icon("theme")) "Monochrome theme"
                    }
                    form method="post" action="/logout" {
                        (csrf(auth))
                        button.account-action type="submit" { (icon("logout")) "Sign out" }
                    }
                }
            }
            button.icon-button.shortcut-help type="button" data-shortcuts-help hidden
                title="Keyboard shortcuts" aria-label="Keyboard shortcuts" { (icon("help")) }
        }
    }
}

fn head(title: &str) -> Markup {
    html! {
        meta charset="utf-8";
        meta name="viewport" content="width=device-width, initial-scale=1";
        title { (title) " · Ratio" }
        link rel="icon" type="image/svg+xml" href="/assets/mark.svg";
        link rel="stylesheet" href="/assets/app.css";
    }
}

fn view_styles(active: &str) -> &'static [&'static str] {
    match active {
        "home" => &["home.css"],
        "issues" | "inbox" => &["issues.css", "issue-detail.css", "relationships.css"],
        "projects" => &["projects.css", "relationships.css"],
        "notes" => &["notes.css", "relationships.css"],
        "roadmap" | "insights" => &["planning.css"],
        "releases" => &["planning.css", "release-view.css", "release-history.css"],
        "health" => &["planning.css", "health-view.css"],
        _ => &["support.css"],
    }
}

/// Render a section's index. A child page uses `page_in_project` for a link back to its section.
pub fn section_page(
    auth: &Auth,
    title: &str,
    active: &str,
    body: Markup,
    project: Option<Uuid>,
) -> Html<String> {
    workspace_page(auth, title, active, body, project, true)
}

pub fn page(auth: &Auth, title: &str, active: &str, body: Markup) -> Html<String> {
    page_in_project(auth, title, active, body, None)
}

pub fn page_in_project(
    auth: &Auth,
    title: &str,
    active: &str,
    body: Markup,
    project: Option<Uuid>,
) -> Html<String> {
    workspace_page(auth, title, active, body, project, false)
}

fn location(
    group: &str,
    section: &str,
    path: &str,
    at_index: bool,
    project: Option<Uuid>,
) -> Markup {
    let group_label = html! { (icon("shell")) span { (group) } };
    if at_index {
        return html! { div.location { (group_label) } };
    }
    let parent = match (path, project) {
        (
            "/issues" | "/notes" | "/releases" | "/roadmap" | "/health" | "/insights" | "/transfer",
            Some(id),
        ) => {
            format!("{path}?project={id}")
        }
        _ => path.to_owned(),
    };
    html! {
        nav.location aria-label="Workspace location" {
            (group_label)
            span.location-divider aria-hidden="true" { "/" }
            a.location-current href=(parent) { (section) }
        }
    }
}

fn workspace_page(
    auth: &Auth,
    title: &str,
    active: &str,
    body: Markup,
    project: Option<Uuid>,
    at_index: bool,
) -> Html<String> {
    let search = project.map_or_else(
        || "/issues#search".to_owned(),
        |id| format!("/issues?project={id}#search"),
    );
    let route = GROUPS.iter().find_map(|(group, items)| {
        items
            .iter()
            .find(|(key, _, _)| *key == active)
            .map(|(_, path, label)| (*group, *path, *label))
    });
    let (group, path, section) = route.unwrap_or(("Workspace", "/", title));
    let sample = auth.user_id == crate::sample::DEMO_OWNER;
    Html(html! {
        (DOCTYPE)
        html lang="en" {
            head {
                (head(title))
                @for file in view_styles(active) {
                    link rel="stylesheet" href=(format!("/assets/{file}"));
                }
                link rel="stylesheet" href="/assets/commands.css";
                (crate::sync::head(auth))
                script defer src="/assets/workspace.js" {}
                script defer src="/assets/commands.js" {}
                @if matches!(active, "issues" | "inbox") { script defer src="/assets/issue-workbench.js" {} }
            }
            body {
                a.skip-link href="#main" { "Skip to content" }
                div.app-shell {
                    aside.sidebar {
                        div.brandbar {
                            (brand("/"))
                            @if sample { span.demo-label title="Workspace with sample data" { "Sample" } }
                        }
                        (navigation(auth, active))
                    }
                    div.workspace-window {
                        header.window-bar {
                            (location(group, section, path, at_index, project))
                            div.mobile-brand {
                                (brand("/"))
                                @if sample { span.demo-label title="Workspace with sample data" { "Sample" } }
                            }
                            div.window-tools {
                                a.search-command data-command-trigger href=(&search) aria-label="Search issues" {
                                    (icon("search"))
                                    span.command-label data-command-label { "Search issues" }
                                    kbd data-command-key hidden { "Ctrl K" }
                                }
                                details.mobile-menu {
                                    summary.icon-button aria-label="Workspace navigation" { (icon("menu")) }
                                    div.mobile-sheet { (navigation(auth, active)) }
                                }
                            }
                        }
                        main.main id="main" tabindex="-1" {
                            div.page-content data-command-project=[project] data-view=(if active == "inbox" { "issues" } else { active }) { (body) }
                        }
                    }
                }
            }
        }
    }.into_string())
}

pub fn public_page(title: &str, body: Markup) -> Html<String> {
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head { (head(title)) link rel="stylesheet" href="/assets/support.css"; script defer src="/assets/workspace.js" {} }
                body.public-page {
                    header.public-brandbar { (brand("/welcome")) }
                    main.page-content { (body) }
                }
            }
        }
        .into_string(),
    )
}
