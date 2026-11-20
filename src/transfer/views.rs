use super::{Report, *};
use crate::ui::{csrf, format_bytes, icon, page_in_project, section_page};
use axum::{extract::Query, response::Html};
use maud::{Markup, html};

#[derive(Deserialize)]
pub(super) struct Filter {
    project: Option<Uuid>,
    images_page: Option<u32>,
}

pub(super) async fn index(
    auth: Auth,
    State(s): State<AppState>,
    Query(filter): Query<Filter>,
) -> AppResult<Html<String>> {
    let projects = store::projects(&s.db, auth.user_id).await?;
    let scoped_project = projects.iter().find(|p| Some(p.id) == filter.project);
    if filter.project.is_some() && scoped_project.is_none() {
        return Err(AppError::NotFound);
    }
    let archived_destination = scoped_project.filter(|p| p.archived);
    let used = store::storage_used(&s.db, auth.user_id).await?;
    let plan: String = sqlx::query_scalar("SELECT plan FROM users WHERE id=$1")
        .bind(auth.user_id)
        .fetch_one(&s.db)
        .await?;
    let limits = store::limits(&plan);
    let preview: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT id,payload IS NULL FROM transfer_previews WHERE owner_id=$1 AND expires_at>now()",
    )
    .bind(auth.user_id)
    .fetch_optional(&s.db)
    .await?;
    let page_number = filter.images_page.unwrap_or(1).max(1);
    let image_page = |n: u32| {
        format!(
            "/transfer?images_page={n}{}#images",
            filter
                .project
                .map(|p| format!("&project={p}"))
                .unwrap_or_default()
        )
    };
    let images:Vec<(Uuid,String,String,bool)>=sqlx::query_as("SELECT b.id,p.name,b.path,p.archived FROM transfer_blobs b JOIN projects p ON p.id=b.project_id AND p.owner_id=b.owner_id WHERE b.owner_id=$1 AND b.trashed_at IS NULL AND ($2::uuid IS NULL OR b.project_id=$2) ORDER BY p.name,b.path,b.id LIMIT 51 OFFSET $3").bind(auth.user_id).bind(filter.project).bind((i64::from(page_number)-1)*50).fetch_all(&s.db).await?;
    let more_images = images.len() > 50;
    Ok(section_page(
        &auth,
        "Import and export",
        "transfer",
        html! {
            header.view-head {
                h1 { "Import and export" }
                @if let Some((id, finished)) = preview {
                    a.action-quiet href=(format!("/transfer/review/{id}")) {
                        @if finished { "View last import" } @else { "Open current import preview" }
                        (icon("arrow"))
                    }
                }
            }
            div.transfer-workspace {
                section.content-section.transfer-import aria-labelledby="import-title" {
                    div.section-head { h2 id="import-title" { "Import into a project" } }
                    @if projects.iter().any(|p| !p.archived) {
                        div.transfer-columns {
                            form.transfer-form.form-stack method="post" action="/transfer/preview" enctype="multipart/form-data" {
                                (csrf(&auth))
                                div.transfer-pair {
                                    label.field { "Destination project"
                                        select name="project" required aria-describedby=[archived_destination.map(|_| "archived-import-hint")] {
                                            @if archived_destination.is_some() {
                                                option value="" selected disabled { "Choose an active project" }
                                            }
                                            @for p in projects.iter().filter(|p| !p.archived) {
                                                option value=(p.id) selected[filter.project == Some(p.id)] { (p.name) }
                                            }
                                        }
                                    }
                                    label.field { "Import format"
                                        select name="format" {
                                            option value="vault" { "Markdown or Ratio ZIP" }
                                            option value="linear" { "Linear CSV" }
                                        }
                                    }
                                }
                                @if let Some(project) = archived_destination {
                                    p.transfer-hint id="archived-import-hint" { (project.name) " is archived. Select an active destination for this import." }
                                }
                                label.field {
                                    "Source file"
                                    input type="file" name="file" accept=".zip,.csv" required aria-describedby="import-file-hint";
                                }
                                p.transfer-hint id="import-file-hint" { "ZIP up to 64 MiB or CSV up to 8 MiB." }
                                details.transfer-disclosure {
                                    summary { "Import a folder inside a ZIP" }
                                    label.field { "Folder path"
                                        input type="text" name="folder" maxlength="1024" aria-describedby="import-folder-hint" placeholder="My Vault/Projects";
                                    }
                                    p.transfer-hint id="import-folder-hint" { "Leave empty for a full archive or a CSV." }
                                }
                                div.transfer-submit {
                                    button.action-primary type="submit" { "Preview import" (icon("arrow")) }
                                    span { "Review changes before importing." }
                                }
                            }
                            aside.transfer-reference aria-labelledby="file-limits" {
                                h3 id="file-limits" { "File requirements" }
                                dl {
                                    div { dt { "Markdown or Ratio ZIP" }
                                        dd { "Up to 2,000 entries and 64 MiB, compressed and expanded. PNG, JPEG, GIF and WebP images can each use up to " (limits.image_bytes / 1_000_000) " MB." }
                                    }
                                    div { dt { "Linear CSV" }
                                        dd { "Up to 2,000 issues and 8 MiB. Include a Title column and an ID or Identifier column." }
                                    }
                                }
                                p.transfer-capacity {
                                    strong { (format_bytes((limits.storage_bytes - used).max(0))) }
                                    " available in this workspace"
                                    span { (format_bytes(used)) " used" }
                                }
                            }
                        }
                    } @else {
                        div.empty-state { p { "Create or restore a project to import files." }
                            a.action-secondary href="/projects" { "Open projects" }
                        }
                    }
                }
                section.content-section.transfer-export aria-labelledby="export-title" {
                    div.section-head { h2 id="export-title" { "Export a project" } }
                    p.transfer-intro { "Download a ZIP with Markdown, images, the project manifest and recorded native history." }
                    @if projects.is_empty() {
                        p.status-note { "No projects to export." }
                    } @else {
                        ul.record-list.transfer-records {
                            @for p in &projects {
                                li.record-row {
                                    div.transfer-project {
                                        span.transfer-key { (p.key) }
                                        span { (p.name) }
                                        @if p.archived { small { "Archived" } }
                                    }
                                    a.action-secondary download href=(format!("/transfer/export/{}", p.id)) aria-label=(format!("Download ZIP for {}", p.name)) {
                                        (icon("down")) "Download ZIP"
                                    }
                                }
                            }
                        }
                        details.transfer-disclosure {
                            summary { "Restore limits and backups" }
                            p { "Browser restore accepts 64 MiB compressed and expanded, up to 2,000 entries, an 8 MiB manifest, and history of up to 16 MiB and 25,000 rows. Larger exports can still be opened in Markdown tools." }
                            p { "For full service recovery, the operator restores the PostgreSQL backup, including image bytes and history." }
                        }
                    }
                }
                section.content-section.transfer-images id="images" aria-labelledby="images-title" {
                    div.section-head {
                        h2 id="images-title" { "Imported images" }
                        a.action-quiet href=(super::trash::list_url(filter.project, 1)) { "Image trash" (icon("arrow")) }
                    }
                    @if let Some(project) = scoped_project {
                        p.transfer-intro { "Images in " (project.name) }
                    }
                    @if images.is_empty() {
                        p.transfer-intro {
                            @if page_number > 1 { "No images on this page." } @else { "No imported images." }
                        }
                    } @else {
                        ul.record-list.transfer-records {
                            @for (id, project, path, archived) in images.into_iter().take(50) {
                                li.record-row {
                                    div.transfer-image-name {
                                        a href=(format!("/transfer/assets/{id}")) { (path) }
                                        small { (project) }
                                    }
                                    @if !archived {
                                        form method="post" action=(format!("/transfer/assets/{id}/trash")) {
                                            (csrf(&auth))
                                            button.action-quiet type="submit" aria-label=(format!("Move to trash — {path}")) { "Move to trash" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    @if page_number > 1 || more_images {
                        nav.toolbar aria-label="Image pages" {
                            @if page_number > 1 { a href=(image_page(page_number - 1)) { "Previous images" } }
                            @if more_images { a href=(image_page(page_number.saturating_add(1))) { "More images" } }
                        }
                    }
                }
            }
        },
        filter.project,
    ))
}

#[derive(FromRow)]
struct Review {
    project_id: Uuid,
    name: String,
    format: String,
    stale: bool,
    done: bool,
    expires_at: DateTime<Utc>,
    report: sqlx::types::Json<Report>,
}

pub(super) async fn review(
    auth: Auth,
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Html<String>> {
    let review=sqlx::query_as::<_,Review>("SELECT r.project_id,p.name,r.format,r.stale,(r.payload IS NULL) AS done,r.expires_at,r.report FROM transfer_previews r JOIN projects p ON p.owner_id=r.owner_id AND p.id=r.project_id WHERE r.id=$1 AND r.owner_id=$2").bind(id).bind(auth.user_id).fetch_optional(&s.db).await?.ok_or(AppError::NotFound)?;
    let r = review.report.0;
    let done = review.done;
    let title = if done {
        "Import complete"
    } else {
        "Import preview"
    };
    let active = !done && !review.stale && review.expires_at > Utc::now();
    let (unchanged, changes): (Vec<_>, Vec<_>) =
        r.rows.iter().partition(|(_, action)| action == "Unchanged");
    Ok(page_in_project(
        &auth,
        title,
        "transfer",
        html! {
            header.view-head {
                div {
                    a.transfer-back href=(format!("/transfer?project={}", review.project_id)) { "Import and export" }
                    h1 { (title) }
                }
                @if done {
                    a.action-primary href=(format!("/projects/{}", review.project_id)) { "Open project" (icon("arrow")) }
                }
            }
            div.transfer-workspace {
                section.content-section.transfer-review aria-labelledby="review-title" {
                    div.section-head {
                        h2 id="review-title" { (review.name) }
                        span.transfer-format { @if review.format == "linear" { "Linear CSV" } @else { "Markdown ZIP" } }
                    }
                    div.transfer-report-grid {
                        div {
                            dl.transfer-totals {
                                @for (label, count) in [("New", r.created), ("Updated", r.updated), ("Unchanged", r.unchanged), ("Conflicts", r.conflicts)] {
                                    div { dt { (label) } dd { (count) } }
                                }
                            }
                            p.transfer-storage { "Storage change " span { (format!("{:+} bytes", r.growth)) }
                                @if !done && r.conflicts > 0 { " with local values kept" }
                            }
                        }
                        @if !r.fields.is_empty() || !r.warnings.is_empty() {
                            div.transfer-report-evidence {
                                @if !r.fields.is_empty() {
                                    details.transfer-disclosure {
                                        summary { @if review.format == "linear" { "Recognized CSV columns" } @else { "Supported archive data" } }
                                        @if review.format == "linear" {
                                            p { (r.fields.join(", ")) }
                                        } @else {
                                            ul { @for field in &r.fields { li { (field) } } }
                                        }
                                    }
                                }
                                @if !r.warnings.is_empty() {
                                    details.transfer-disclosure.transfer-notices open[r.warnings.len() <= 4] {
                                        summary { (r.warnings.len()) @if r.warnings.len() == 1 { " import notice" } @else { " import notices" } }
                                        ul { @for warning in &r.warnings { li { (warning) } } }
                                    }
                                }
                            }
                        }
                    }
                }
                @if !active && !done {
                    div.status-note {
                        p { @if review.stale { "This project changed after the preview." } @else { "This preview expired." } }
                        a.action-secondary href=(format!("/transfer?project={}", review.project_id)) { "Create a fresh preview" }
                    }
                }
                @if !r.rows.is_empty() {
                    section.content-section.transfer-changes aria-labelledby="changes-title" {
                        div.section-head {
                            h2 id="changes-title" { "Changes" }
                            @if active && changes.len() > 10 { a.action-quiet href="#import-decision" { "Go to import decision" (icon("down")) } }
                        }
                        @if !changes.is_empty() {
                            (change_table(&changes, "Changes to files and records"))
                        }
                        @if !unchanged.is_empty() {
                            details.transfer-disclosure {
                                summary { "Unchanged files and records (" (r.unchanged) ")" }
                                (change_table(&unchanged, "Unchanged files and records"))
                            }
                        }
                    }
                }
                @if active {
                    form.content-section.transfer-decision.form-stack id="import-decision" method="post" action=(format!("/transfer/commit/{id}")) {
                        (csrf(&auth))
                        @if r.conflicts > 0 {
                            fieldset.transfer-conflicts aria-describedby="transfer-choice-hint" {
                                legend { "Conflicting changes" }
                                p id="transfer-choice-hint" {
                                    "One choice applies to all conflicts in this import."
                                    @if review.format != "linear" { " Sealed releases and recorded events can only be kept locally or restored into a new project." }
                                }
                                label { input type="radio" name="decision" value="local" checked; "Keep local values" }
                                label { input type="radio" name="decision" value="incoming"; "Use incoming values" }
                            }
                        } @else {
                            input type="hidden" name="decision" value="local";
                        }
                        div.transfer-submit {
                            button.action-primary type="submit" { "Import changes" (icon("arrow")) }
                            a.action-quiet href=(format!("/transfer?project={}", review.project_id)) { "Back to imports" }
                            span { "Preview expires " time datetime=(review.expires_at.to_rfc3339()) { (review.expires_at.format("%d %b, %H:%M UTC")) } }
                        }
                    }
                }
            }
        },
        Some(review.project_id),
    ))
}

fn change_table(rows: &[&(String, String)], label: &str) -> Markup {
    html! {
        div.transfer-change-table tabindex=[(rows.len() > 10).then_some("0")] role=[(rows.len() > 10).then_some("region")] aria-label=[(rows.len() > 10).then_some(label)] {
            table {
                caption.sr-only { (label) }
                thead { tr { th scope="col" { "File or record" } th scope="col" { "Change" } } }
                tbody { @for (name, action) in rows {
                    tr { th scope="row" { (name) } td { (action) } }
                } }
            }
        }
    }
}
