use super::{Report, *};
use crate::ui::{csrf, page};
use axum::{extract::Query, response::Html};
use maud::html;

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
    if filter
        .project
        .is_some_and(|id| !projects.iter().any(|p| p.id == id))
    {
        return Err(AppError::NotFound);
    }
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
    Ok(page(
        &auth,
        "Import and export",
        "transfer",
        html! {
            header {h1 {"Import and export"} p class="muted" {"Move a project to another workspace or bring Markdown notes and Linear issues into this one."}}
            @if let Some((id,finished))=preview {p {a href=(format!("/transfer/review/{id}")) {@if finished {"View last import"} @else {"Open current import preview"}}}}
            section class="panel stack" {h2 {"Import into a project"}
                @if projects.iter().any(|p|!p.archived) {
                    form class="stack" method="post" action="/transfer/preview" enctype="multipart/form-data" {(csrf(&auth))
                        label class="field" {"Destination project" select name="project" required { @for p in projects.iter().filter(|p|!p.archived) {option value=(p.id) selected[filter.project==Some(p.id)] {(p.name)}}}}
                        label class="field" {"Import format" select name="format" {option value="vault" {"Markdown vault or Workspace export (.zip)"} option value="linear" {"One Linear project (.csv)"}}}
                        label class="field" {"Folder inside ZIP (optional)" input type="text" name="folder" placeholder="e.g. My Vault/Projects" maxlength="1024";}
                        label class="field" {"File" input type="file" name="file" accept=".zip,.csv" required;}
                        p class="muted" {"ZIP files can contain up to 2,000 entries and 64 MiB of expanded data. Supported images are PNG, JPEG, GIF and WebP, up to " (limits.image_bytes/1_000_000) " MB per image. Linear CSV accepts up to 2,000 rows and 8 MiB. The preview lists changes and unresolved references before you import."}
                        p class="muted" {(format!("{:.1} MB used · {:.1} MB available",used as f64/1_000_000.0,(limits.storage_bytes-used).max(0) as f64/1_000_000.0))}
                        div {button type="submit" {"Preview import"}}
                    }
                } @else {p {"Create or restore a project to import files. " a href="/projects" {"Open projects"}}}
            }
            section class="panel stack" {h2 {"Export a project"} p class="muted" {"Download Markdown, images, a versioned manifest and recorded native history in one ZIP. Archived projects are available here. Browser restore accepts 64 MiB compressed and expanded, 2,000 entries, an 8 MiB manifest and 16 MiB or 25,000 rows of history. Larger exports remain readable in Markdown tools. For full service recovery, the operator restores the PostgreSQL backup, including image bytes and history."}
                @if projects.is_empty() {p {"Your projects will appear here."}} @else {ul class="issue-list" {@for p in &projects {li class="row" {span {(p.name) @if p.archived {" · Archived"}} a class="button secondary" download href=(format!("/transfer/export/{}",p.id)) aria-label=(format!("Download {} ZIP",p.name)) {"Download ZIP"}}}}}
            }
            p {a href=(super::trash::list_url(filter.project,1)) {"Image trash"}}
            @if !images.is_empty() || page_number>1 {section id="images" class="panel stack" {h2 {"Imported images"}
                ul class="issue-list" {@for (id,project,path,archived) in images.into_iter().take(50) {li class="row" {span {a href=(format!("/transfer/assets/{id}")) {(path)} " · " (project)} @if !archived {form method="post" action=(format!("/transfer/assets/{id}/trash")) {(csrf(&auth)) button class="secondary" type="submit" aria-label=(format!("Move {path} to trash")) {"Move to trash"}}}}}}
                nav class="toolbar" aria-label="Image pages" {@if page_number>1 {a href=(image_page(page_number-1)) {"Previous images"}} @if more_images {a href=(image_page(page_number.saturating_add(1))) {"More images"}}}
            }}
        },
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
    Ok(page(
        &auth,
        title,
        "transfer",
        html! {
            header {a href="/transfer" {"Import and export"} h1 {(title)} p {(review.name) " · " @if review.format=="linear" {"Linear CSV"} @else {"Markdown ZIP"}}}
            section class="panel stack" {
                p {(format!("{} new · {} updated · {} unchanged · {} conflicts",r.created,r.updated,r.unchanged,r.conflicts))}
                @if !r.fields.is_empty(){p {(r.fields.join(" · "))}}
                p class="muted" {(format!("Storage change: {:+} bytes",r.growth))}
                @if !r.warnings.is_empty(){details open {summary {(format!("{} import notices",r.warnings.len()))} ul {@for warning in &r.warnings {li {(warning)}}}}}
                @if active {form class="stack" method="post" action=(format!("/transfer/commit/{id}")) {(csrf(&auth))
                    @if r.conflicts>0 {fieldset {legend {"When both copies changed"} label {input type="radio" name="decision" value="local" checked; " Keep the local value for each conflict"} label {input type="radio" name="decision" value="incoming"; " Use the incoming value for each conflict"}}} @else {input type="hidden" name="decision" value="local";}
                    p class="muted" {"This preview expires at " (review.expires_at.format("%H:%M UTC")) "."}
                    div {button type="submit" {"Import changes"}}
                }} @else if done {a href=(format!("/projects/{}",review.project_id)) {"Open project"}} @else {p class="notice" {"The project changed or this preview expired. " a href=(format!("/transfer?project={}",review.project_id)) {"Create a fresh preview"}}}
            }
            @if !r.rows.is_empty(){section class="panel stack" {h2 {"Changes"} table {thead {tr {th scope="col" {"File or record"} th scope="col" {"Action"}}} tbody {@for (name,action) in r.rows {tr {th scope="row" {(name)} td {(action)}}}}}}}
        },
    ))
}
