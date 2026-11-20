use super::{Board, BoardSummary};
use crate::{auth::Auth, store::Project, ui};
use axum::response::Html;
use maud::{Markup, html};
use uuid::Uuid;

mod fallback;

fn icon(name: &str) -> Markup {
    let path = match name {
        "board" => "M3 4h18v16H3zM6 8h5v3H6zM14 13h4v3h-4zM11 9.5h3v5",
        "select" => "m5 3 13.5 9-6.1 1.2-2.9 6.3L5 3Z",
        "pan" => {
            "M8 12V6.5a1.5 1.5 0 013 0V11 4.5a1.5 1.5 0 013 0V11 6a1.5 1.5 0 013 0v5-2.5a1.5 1.5 0 013 0v6.2c0 4.1-2.6 6.3-6.3 6.3h-1.4c-2.2 0-3.5-1-4.9-2.9l-4-5.4a1.5 1.5 0 012.3-1.9L8 13z"
        }
        "shapes" => "M3 4h10v10H3zM16 10.5l5 4.8-5 4.8-5-4.8",
        "process" => "M3.5 6h17v12h-17z",
        "decision" => "m12 2.5 9.5 9.5-9.5 9.5L2.5 12z",
        "service" => "M4 4.5h16v6H4zM4 13.5h16v6H4zM7 7.5h.1M7 16.5h.1",
        "database" => {
            "M4.5 6.5a7.5 3 0 1015 0 7.5 3 0 10-15 0m0 0v11c0 1.7 3.4 3 7.5 3s7.5-1.3 7.5-3v-11m-15 5.5c0 1.7 3.4 3 7.5 3s7.5-1.3 7.5-3"
        }
        "note" => "M4 3.5h15.5V15l-5.5 5.5H4zM14 20v-5h5.1M7.5 8h8M7.5 11h5",
        "connect" => "M3 4h5v5H3zM16 15h5v5h-5zM8 6.5h3a2 2 0 012 2v7a2 2 0 002 2h1",
        "pen" => {
            "m3 17 11.8-12 4.2 4.2L7.2 21 2 22zM12.5 7.5l4 4M17.4 2.6a2 2 0 012.8 0l1.2 1.2a2 2 0 010 2.8l-1 1-4-4z"
        }
        "fit" => "M3 9V4h5m8 0h5v5m0 6v5h-5M8 20H3v-5M8 9h8v6H8z",
        "snap" => "M5 3v18M12 3v18M19 3v18M3 5h18M3 12h18M3 19h18",
        "undo" => "M8 4 3 9l5 5M4 9h10a6 6 0 010 12h-2",
        "redo" => "m16 4 5 5-5 5m4-5H10a6 6 0 000 12h2",
        "delete" => "M4 6h16M9 6V3h6v3M6 6l1 14h10l1-14M10 9v8m4-8v8",
        "minus" => "M5 12h14",
        "blank" => "M4 4h16v16H4z",
        "architecture" => "M3 9h5v6H3zM15 3h6v5h-6zM15 16h6v5h-6zM8 12h3V5.5h4m-4 6.5v6.5h4",
        "release" => "M2 9h5v6H2zM10 9h5v6h-5zM18 9h4v6h-4zM7 12h3m5 0h3",
        _ => return ui::icon(name),
    };
    html! {
        svg.ui-icon viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor"
            stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false" { path d=(path) {} }
    }
}

fn creation_form(auth: &Auth, available: &[&Project]) -> Markup {
    html! {
        form.canvas-create-form method="post" action="/canvas" {
            (ui::csrf(auth))
            label.canvas-form-field { "Board name"
                input name="title" type="text" maxlength="120" required pattern=r".*\S.*" placeholder="Untitled board" autocomplete="off";
            }
            @if let [project] = available {
                input type="hidden" name="project" value=(project.id);
                p.canvas-assigned { (icon("projects")) span.sr-only { "Project " } (project.name) }
            } @else {
                label.canvas-form-field { "Project"
                    select name="project" required {
                        @for project in available { option value=(project.id) { (project.name) } }
                    }
                }
            }
            fieldset.canvas-template-picker {
                legend { "Start with" }
                @for (key, label) in [("blank", "Blank board"), ("architecture", "System architecture"), ("release", "Release plan"), ("decision", "Decision tree")] {
                    label.canvas-template {
                        input type="radio" name="template" value=(key) checked[key == "blank"];
                        (icon(key)) span { (label) }
                    }
                }
            }
            button.action-primary.canvas-create-submit type="submit" { "Create board" (icon("arrow")) }
        }
    }
}

fn creation_panel(auth: &Auth, available: &[&Project], has_projects: bool) -> Markup {
    html! {
        div.canvas-empty-field {
            section.canvas-create-sheet {
                @if !available.is_empty() {
                    h2 { (icon("board")) "New board" }
                    (creation_form(auth, available))
                } @else if !has_projects {
                    h2 { (icon("board")) "Create a project for your boards" }
                    a.action-primary href="/projects#new-project" { "Create project" (icon("arrow")) }
                } @else {
                    h2 { (icon("board")) "No boards" }
                    p { "Boards can be created in an active project." }
                    a.action-secondary href="/projects" { "View projects" (icon("arrow")) }
                }
            }
        }
    }
}

fn writable_projects(projects: &[Project], project: Option<Uuid>) -> Vec<&Project> {
    projects
        .iter()
        .filter(|p| !p.archived && project.is_none_or(|id| id == p.id))
        .collect()
}

pub(super) fn creation(auth: &Auth, projects: &[Project], project: Option<Uuid>) -> Html<String> {
    let directory = project.map_or_else(
        || "/canvas".to_string(),
        |id| format!("/canvas?project={id}"),
    );
    let available = writable_projects(projects, project);
    let body = html! {
        div.canvas-directory {
            header.view-head.canvas-directory-head {
                h1 { "Canvas" }
                a.action-secondary href=(directory) { "Back to boards" }
            }
            (creation_panel(auth, &available, !projects.is_empty()))
        }
    };
    ui::section_page(auth, "New board", "canvas", body, project)
}

pub(super) fn directory(
    auth: &Auth,
    projects: &[Project],
    selected: Option<Uuid>,
    boards: &[BoardSummary],
    page: i64,
    more: bool,
) -> Html<String> {
    let available = writable_projects(projects, selected);
    let create_url = selected.map_or_else(
        || "/canvas?new=true".to_string(),
        |id| format!("/canvas?project={id}&new=true"),
    );
    let page_url = |number| match selected {
        Some(id) => format!("/canvas?project={id}&page={number}"),
        None => format!("/canvas?page={number}"),
    };
    let content = html! {
        div.canvas-directory {
            header.view-head.canvas-directory-head {
                h1 { "Canvas" }
                @if projects.len() > 1 {
                    form.canvas-project-filter method="get" action="/canvas" {
                        label.sr-only for="canvas-project-filter" { "Filter boards by project" }
                        select id="canvas-project-filter" name="project" {
                            option value="" selected[selected.is_none()] { "All projects" }
                            @for p in projects {
                                option value=(p.id) selected[selected == Some(p.id)] { (p.name) @if p.archived { " (archived)" } }
                            }
                        }
                        button.action-secondary type="submit" { "Apply" }
                    }
                }
            }
            @if boards.is_empty() && page == 1 {
                (creation_panel(auth, &available, !projects.is_empty()))
            } @else {
                div.canvas-directory-content {
                    div.canvas-list-heading {
                        h2 { "Boards" @if !more && page == 1 { span.canvas-stat { (boards.len()) } } }
                        @if !available.is_empty() {
                            a.action-secondary.canvas-create-link href=(create_url) { (icon("plus")) "New board" }
                        }
                    }
                    ul.canvas-board-list {
                        @for board in boards {
                            li {
                                a.canvas-board-link href=(format!("/canvas/{}",board.id)) {
                                    span.canvas-board-symbol { (icon("board")) }
                                    span.canvas-board-name { strong { (&board.title) } small { (&board.project_name)
                                        @if board.read_only { " · Archived" }
                                    } }
                                    span.canvas-board-metrics {
                                        @if board.node_count > 0 { span { (board.node_count) @if board.node_count==1 { " node" } @else { " nodes" } } }
                                        @if board.edge_count > 0 { span { (board.edge_count) @if board.edge_count==1 { " connection" } @else { " connections" } } }
                                        @if board.stroke_count > 0 { span { (board.stroke_count) @if board.stroke_count==1 { " stroke" } @else { " strokes" } } }
                                        @if board.node_count==0 && board.stroke_count==0 { span { "Empty board" } }
                                    }
                                    time datetime=(board.updated_at.to_rfc3339()) title=(board.updated_at.format("%Y-%m-%d %H:%M UTC")) { (board.updated_at.format("%d %b")) }
                                    (icon("arrow"))
                                }
                                @if !board.read_only {
                                    details.canvas-board-actions {
                                        summary.canvas-tool aria-label=(format!("Actions for {}",board.title)) title="Board actions" { (icon("controls")) }
                                        form.canvas-remove-board method="post" action=(format!("/canvas/{}/delete",board.id)) {
                                            (ui::csrf(auth)) input type="hidden" name="revision" value=(board.revision);
                                            p { "Deleting this board also removes its checkpoints." }
                                            label.canvas-form-field { "Type the board name to confirm" input name="confirmation" required maxlength="120" autocomplete="off"; }
                                            button.action-secondary type="submit" { "Delete board" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    @if page > 1 || more {
                        nav.canvas-pages aria-label="Board pages" {
                            @if page > 1 { a.action-secondary href=(page_url(page-1)) { "Previous" } }
                            @if more { a.action-secondary href=(page_url(page+1)) { "Next" (icon("arrow")) } }
                        }
                    }
                }
            }
        }
    };
    ui::section_page(auth, "Canvas", "canvas", content, selected)
}

fn tool(key: &str, text: &str) -> Markup {
    let shortcut = match key {
        "select" => Some("V"),
        "pan" => Some("H"),
        "note" => Some("N"),
        "connect" => Some("C"),
        "pen" => Some("P"),
        _ => None,
    };
    let title = shortcut.map_or_else(|| text.to_owned(), |key| format!("{text} ({key})"));
    html! {
        button.canvas-tool type="button" data-tool=(key) aria-label=(text) title=(title) aria-keyshortcuts=[shortcut] aria-pressed=(if key=="select" {"true"} else {"false"}) { (icon(key)) }
    }
}

fn command(key: &str, text: &str, symbol: &str) -> Markup {
    html! { button.canvas-tool type="button" data-command=(key) aria-label=(text) title=(text) { (icon(symbol)) } }
}

fn drawing_tools(read_only: bool) -> Markup {
    html! {
        div.canvas-dock id="canvas-tools" role="group" aria-label="Drawing tools" {
            (tool("select","Select")) (tool("pan","Pan"))
            @if !read_only {
                span.canvas-divider aria-hidden="true" {}
                details.canvas-shapes data-disclosure-menu {
                    summary.canvas-tool title="Shapes" aria-label="Shapes" { (icon("shapes")) }
                    div.canvas-shape-options {
                        @for (key,label) in [("process","Process"),("decision","Decision"),("service","Service"),("database","Database")] {
                            button type="button" data-tool=(key) aria-pressed="false" { (icon(key)) span { (label) } }
                        }
                    }
                }
                (tool("note","Note")) (tool("connect","Connect")) (tool("pen","Pen"))
            }
        }
    }
}

pub(super) fn editor(auth: &Auth, board: &Board) -> Html<String> {
    let board_url = format!("/canvas?project={}", board.project);
    let content = html! {
        section.canvas-editor data-canvas-editor data-id=(board.id) data-owner=(auth.user_id)
            data-csrf=(&auth.csrf) data-read-only=(board.read_only) aria-labelledby="canvas-page-heading" {
            header.canvas-header {
                a.canvas-back href=(board_url) title=(format!("Boards in {}",board.project_name)) {
                    (icon("board")) span { "Boards" }
                }
                span.canvas-slash aria-hidden="true" { "/" }
                h1 id="canvas-page-heading" { (&board.title) }
                input id="canvas-title" aria-label="Board name" value=(&board.title) readonly[board.read_only] maxlength="120" autocomplete="off";
                div.canvas-save-state {
                    span id="canvas-status" role="status" aria-live="polite" {}
                    span id="canvas-save-actions" {}
                }
                @if board.read_only { span.canvas-read-only { "Read only" } }
                div id="canvas-extras" {}
            }
            div.canvas-fallback {
                noscript { p { "Enable JavaScript to draw on this board." } }
                (fallback::graphic(&board.document))
                (fallback::outline(&board.document))
            }
            div.canvas-viewport {
                span.sr-only id="canvas-stage-help" {
                    "Use the bracket keys to select an item. Arrow keys move a selected shape or pan the canvas."
                    @if !board.read_only { " Choose a shape tool and press Enter to place it." }
                }
                svg id="canvas-stage" xmlns="http://www.w3.org/2000/svg" tabindex="0" role="group"
                    aria-label="Diagram canvas" aria-describedby="canvas-stage-help" {
                    g id="canvas-world" {}
                }
                @if !board.read_only {
                    div.canvas-welcome {
                        (icon("board"))
                        h2 { "Start your diagram" }
                        button.action-secondary type="button" data-tool="note" { (icon("plus")) "Place a note" }
                    }
                }
                (drawing_tools(board.read_only))
                div.canvas-history role="group" aria-label="Edit actions" {
                    @if !board.read_only {
                        (command("undo","Undo","undo")) (command("redo","Redo","redo"))
                        (command("delete","Delete selection","delete"))
                    }
                }
                div.canvas-view-tools role="group" aria-label="View controls" {
                    (command("zoom-out","Zoom out","minus"))
                    (command("fit","Fit drawing","fit"))
                    (command("zoom-in","Zoom in","plus"))
                    @if !board.read_only {
                        span.canvas-divider aria-hidden="true" {}
                        button.canvas-tool type="button" data-command="snap" aria-label="Snap to grid" title="Snap to grid" aria-pressed="false" { (icon("snap")) }
                    }
                }
                div id="canvas-inspector" hidden {}
                div id="canvas-minimap" {}
            }
        }
        script type="module" src="/assets/canvas.js" {}
    };
    ui::section_page(auth, &board.title, "canvas", content, Some(board.project))
}
