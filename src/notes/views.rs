use super::*;
use crate::ui::{csrf, icon, page, page_in_project, section_page};
use axum::{
    extract::Query,
    http::{StatusCode, header},
    response::Html,
};
use maud::{Markup, html};
use std::collections::HashMap;

#[derive(Default, Deserialize)]
pub(super) struct Filter {
    project: Option<Uuid>,
    deleted: Option<Uuid>,
    q: Option<String>,
    back: Option<String>,
    #[serde(default)]
    page: u32,
}
#[derive(Default, Deserialize)]
pub(super) struct Saved {
    saved: Option<String>,
    draft_token: Option<Uuid>,
    back: Option<String>,
}

// A note may be opened from a paged search, a project, or a direct URL.
// Only a local Notes index can travel through the read/edit/preview round trip.
fn index_origin(value: Option<&str>) -> Option<&str> {
    let value = value.filter(|v| v.len() <= 2048)?;
    let uri = value.parse::<axum::http::Uri>().ok()?;
    (uri.scheme().is_none()
        && uri.authority().is_none()
        && uri.path() == "/notes"
        && !value.contains('#')
        && url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
            .all(|(key, _)| matches!(key.as_ref(), "project" | "q" | "page")))
    .then_some(value)
}

pub(super) fn with_origin(path: &str, origin: Option<&str>) -> String {
    let Some(origin) = index_origin(origin) else {
        return path.to_owned();
    };
    let join = if path.contains('?') { '&' } else { '?' };
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("back", origin)
        .finish();
    format!("{path}{join}{query}")
}

fn index_url(project: Option<Uuid>, term: &str, page: u32) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if let Some(id) = project {
        query.append_pair("project", &id.to_string());
    }
    if !term.is_empty() {
        query.append_pair("q", term);
    }
    if page > 1 {
        query.append_pair("page", &page.to_string());
    }
    let query = query.finish();
    if query.is_empty() {
        "/notes".into()
    } else {
        format!("/notes?{query}")
    }
}

#[cfg(test)]
#[test]
fn note_returns_keep_search_and_page_and_only_open_the_notes_index() {
    let origin = index_url(Some(Uuid::nil()), "read & write", 2);
    let destination = with_origin("/notes/example?saved=updated", Some(&origin));
    let query = destination.split_once('?').unwrap().1;
    let fields: HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes()).collect();
    assert_eq!(fields["back"], origin);
    assert!(origin.ends_with("q=read+%26+write&page=2"));
    for invalid in [
        "//example.org/notes",
        "https://example.org/notes",
        "/notes/other",
        "/notes?deleted=anything",
    ] {
        assert_eq!(
            with_origin("/notes/example", Some(invalid)),
            "/notes/example"
        );
    }
}

pub(super) async fn editor_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("editor.js"),
    )
}

type NoteSummary = (Uuid, String, String, chrono::DateTime<chrono::Utc>, String);

pub(super) async fn find_notes(
    db: &PgPool,
    owner: Uuid,
    project: Option<Uuid>,
    term: &str,
    offset: i64,
) -> AppResult<Vec<NoteSummary>> {
    Ok(sqlx::query_as(
        "SELECT n.id,n.title,n.path,n.updated_at,p.name FROM notes n JOIN projects p ON (p.owner_id,p.id)=(n.owner_id,n.project_id)
         WHERE n.owner_id=$1 AND ($2::uuid IS NULL OR n.project_id=$2)
           AND ($3='' OR strpos(lower(n.path),lower($3))>0 OR strpos(lower(n.title),lower($3))>0
                OR to_tsvector('simple', n.title || ' ' || left(n.body,32000)) @@ plainto_tsquery('simple',$3)
                OR (octet_length(n.body)>32000 AND strpos(lower(substring(n.body FROM greatest(1,32001-char_length($3)))),lower($3))>0))
         ORDER BY n.updated_at DESC,n.id LIMIT 51 OFFSET $4")
        .bind(owner).bind(project).bind(term).bind(offset).fetch_all(db).await?)
}

pub(super) async fn list(
    State(s): State<AppState>,
    auth: Auth,
    Query(q): Query<Filter>,
) -> AppResult<Html<String>> {
    let projects = store::projects(&s.db, auth.user_id).await?;
    let selected = if let Some(id) = q.project {
        Some(
            projects
                .iter()
                .find(|p| p.id == id)
                .ok_or(AppError::NotFound)?,
        )
    } else {
        None
    };
    let deleted = if let (Some(project), Some(id)) = (q.project, q.deleted) {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM notes WHERE owner_id=$1 AND project_id=$2 AND id=$3)",
        )
        .bind(auth.user_id)
        .bind(project)
        .bind(id)
        .fetch_one(&s.db)
        .await?;
        (!exists).then_some((project, id))
    } else {
        None
    };
    let page_no = q.page.max(1);
    let offset = i64::from(page_no - 1) * 50;
    let term = q.q.as_deref().unwrap_or_default().trim();
    if term.chars().count() > 160 || term.contains('\0') {
        return Err(AppError::BadRequest(
            "Use a note search up to 160 characters.".into(),
        ));
    }
    let mut rows = find_notes(&s.db, auth.user_id, q.project, term, offset).await?;
    let more = rows.len() > 50;
    rows.truncate(50);
    let origin = index_url(q.project, term, page_no);
    let clear = index_url(q.project, "", 1);
    let writable = selected.map_or_else(|| projects.iter().any(|p| !p.archived), |p| !p.archived);
    let new_href = q.project.map_or_else(
        || "/notes/new".to_owned(),
        |id| format!("/notes/new?project={id}"),
    );
    Ok(section_page(
        &auth,
        "Notes",
        "notes",
        html! {
            header.view-head {
                h1 { "Notes" }
                @if writable {
                    div.view-actions { a.action-primary href=(with_origin(&new_href, Some(&origin))) { (icon("plus")) "New note" } }
                }
            }
            @if let Some((project,id))=deleted {
                p.status-note role="status" data-note-deleted="" data-user=(auth.user_id) data-project=(project) data-note=(id) {"Note permanently deleted."}
                script src="/assets/note-editor.js" defer {}
            }
            div.note-library {
                @if !projects.is_empty() {
                    div.view-tools.note-index-tools {
                        form.note-search method="get" action="/notes" role="search" aria-label="Notes" {
                            @if let Some(id)=q.project {input type="hidden" name="project" value=(id);}
                            label.note-search-input {
                                span.sr-only {"Search notes by title, path, or text"}
                                (icon("search"))
                                input type="search" name="q" maxlength="160" value=(term) placeholder="Search notes by title, path, or text";
                            }
                            button.action-secondary type="submit" {"Search"}
                            @if !term.is_empty() {a.action-quiet.note-clear href=(&clear) aria-label="Clear note search" {"Clear"}}
                        }
                        @if projects.len()>1 {
                            details.note-scope data-disclosure-menu="" {
                                summary { (icon("projects")) (selected.map_or("All projects", |p|p.name.as_str())) }
                                nav aria-label="Filter notes by project" {
                                    a href=(index_url(None,term,1)) aria-current=[selected.is_none().then_some("true")] {"All projects"}
                                    @for p in &projects {
                                        a href=(index_url(Some(p.id),term,1)) aria-current=[(q.project == Some(p.id)).then_some("true")] { (p.name) @if p.archived { span {" · Archived"} } }
                                    }
                                }
                            }
                        } @else if let Some(p) = selected {
                            a.note-index-project href=(format!("/projects/{}",p.id)) { (icon("projects")) (p.name) }
                        }
                    }
                }
                @if rows.is_empty() {
                    section.empty-state.note-empty {
                        @if page_no > 1 {
                            h2 {"No notes on this page"} a.action-secondary href=(index_url(q.project,term,1)) {"Go to first page"}
                        } @else if !term.is_empty() {
                            h2 {"No matching notes"} p {"No title, path or note text matched “" (term) "”."}
                        } @else if projects.is_empty() {
                            h2 {"Create a project for your notes"} p {"Each note belongs to a project."}
                            a.action-primary href="/projects" {"Create a project"}
                        } @else if !writable {
                            h2 {"No notes here yet"} p {"Restore an archived project to add notes."}
                            a.action-secondary href=(selected.map_or_else(||"/projects".to_owned(),|p|format!("/projects/{}",p.id))) {"View project" @if selected.is_none() {"s"}}
                        } @else {
                            h2 {"No notes here yet"} p {"Use New note to save a decision or a development checklist."}
                        }
                    }
                } @else {
                    div.note-ledger-head aria-hidden="true" {
                        span {"Note"} span {"Project"} span {"Last edited" (icon("down"))}
                    }
                    ul.record-list.note-ledger role="list" aria-label="Notes, most recently edited first" {
                        @for (id,title,path,updated,project) in &rows {
                            li {
                                a.record-row.note-record href=(with_origin(&format!("/notes/{id}"), Some(&origin))) {
                                    div.note-file { (icon("notes")) div { strong {(title)} (file_path(path)) } }
                                    span.note-project {(project)}
                                    time datetime=(updated.to_rfc3339()) title=(updated.format("%b %-d, %Y at %H:%M UTC")) { (updated.format("%b %-d, %Y")) }
                                }
                            }
                        }
                    }
                }
                @if more || page_no > 1 {
                    nav.note-pages aria-label="Note pages" {
                        span {"Page " (page_no)}
                        @if page_no>1 {a.action-secondary rel="prev" href=(index_url(q.project,term,page_no-1)) {"Previous"}}
                        @if more {a.action-secondary rel="next" href=(index_url(q.project,term,page_no+1)) {"Next"}}
                    }
                }
            }
        },
        q.project,
    ))
}

fn file_path(path: &str) -> Markup {
    let (folder, filename) = path
        .rsplit_once('/')
        .map_or((None, path), |(p, f)| (Some(p), f));
    // The ending often disambiguates similarly named files in deep folder trees.
    let tail_at = filename
        .char_indices()
        .rev()
        .take(20)
        .filter_map(|(offset, ch)| matches!(ch, '-' | '_' | '.' | ' ').then_some(offset))
        .last()
        .unwrap_or(filename.len());
    let (stem, tail) = filename.split_at(tail_at);
    html! {
        code.note-list-path title=(path) {
            @if let Some(folder) = folder {span.note-list-folders {(folder) "/"}}
            span.note-list-name {
                @if !stem.is_empty() {span {(stem)}}
                bdi {(tail)}
            }
        }
    }
}

pub(super) async fn new(
    State(s): State<AppState>,
    auth: Auth,
    Query(q): Query<Filter>,
) -> AppResult<Html<String>> {
    let projects = store::projects(&s.db, auth.user_id).await?;
    let project = if let Some(p) = q.project {
        store::writable(&s.db, auth.user_id, p).await?.id
    } else if let Some(p) = projects.iter().find(|p| !p.archived) {
        p.id
    } else {
        return Ok(page(
            &auth,
            "New note",
            "notes",
            html! {header.view-head {h1{"New note"}} section.empty-state.note-empty {p{"Create an active project to save your first note."} a.action-primary href="/projects" {"Open projects"}}},
        ));
    };
    let form = NoteForm {
        csrf: String::new(),
        back: index_origin(q.back.as_deref()).map(str::to_owned),
        draft_token: None,
        project,
        note_id: None,
        revision: None,
        title: String::new(),
        path: String::new(),
        body: String::new(),
    };
    editor(&s.db, &auth, None, form, None, None).await
}

pub(super) async fn edit(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Query(q): Query<Saved>,
) -> AppResult<Html<String>> {
    let note = load_note(&s.db, auth.user_id, id).await?;
    store::writable(&s.db, auth.user_id, note.project_id).await?;
    let form = NoteForm {
        csrf: String::new(),
        back: index_origin(q.back.as_deref()).map(str::to_owned),
        draft_token: None,
        project: note.project_id,
        note_id: Some(id),
        revision: Some(note.revision),
        title: note.title,
        path: note.path,
        body: note.body,
    };
    editor(&s.db, &auth, Some(id), form, None, None).await
}

pub(super) async fn preview(
    State(s): State<AppState>,
    auth: Auth,
    Form(f): Form<NoteForm>,
) -> AppResult<Html<String>> {
    auth.check_csrf(&f.csrf)?;
    store::writable(&s.db, auth.user_id, f.project).await?;
    if let Some(id) = f.note_id {
        let note = load_note(&s.db, auth.user_id, id).await?;
        if note.project_id != f.project {
            return Err(AppError::NotFound);
        }
    }
    validated_source(&f.body)?;
    let preview = markdown::render(&f.body, &[], f.project, &f.path);
    editor(&s.db, &auth, f.note_id, f, None, Some(preview)).await
}

pub(super) async fn recover(
    db: &PgPool,
    auth: &Auth,
    id: Option<Uuid>,
    mut form: NoteForm,
    msg: &str,
    conflict: bool,
) -> AppResult<Response> {
    let current = match id {
        Some(n) => Some(load_note(db, auth.user_id, n).await?),
        None => None,
    };
    let changed = current
        .as_ref()
        .is_some_and(|n| Some(n.revision) != form.revision);
    let banner = html! {
        section.notice role="alert" {p{(msg)}
            @if let Some(note)=current.as_ref().filter(|_|changed) {
                details open { summary {"Saved copy · revision " (note.revision)}
                    p {strong{(note.title)} " · " code{(note.path)}} pre {(note.body)}
                }
                p {"Your submitted copy is in the editor. Saving it will apply your changes to revision " (note.revision) "."}
            }
        }
    };
    if let Some(note) = current {
        form.revision = Some(note.revision);
    }
    let body = editor(db, auth, id, form, Some(banner), None).await?;
    Ok((
        if conflict {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        },
        body,
    )
        .into_response())
}

#[derive(Deserialize)]
pub(super) struct LinkQuery {
    project: Uuid,
    #[serde(default)]
    q: String,
}

#[derive(serde::Serialize)]
pub(super) struct Suggestion {
    pub(super) token: String,
    label: String,
}

pub(super) async fn suggestions(
    State(s): State<AppState>,
    auth: Auth,
    Query(q): Query<LinkQuery>,
) -> AppResult<axum::Json<Vec<Suggestion>>> {
    let project = store::project(&s.db, auth.user_id, q.project).await?;
    Ok(axum::Json(
        link_suggestions(&s.db, auth.user_id, &project, &q.q).await?,
    ))
}

pub(super) async fn link_suggestions(
    db: &PgPool,
    owner: Uuid,
    project: &store::Project,
    query: &str,
) -> AppResult<Vec<Suggestion>> {
    if query.chars().count() > 1024 || query.contains('\0') {
        return Err(AppError::BadRequest("The link search is too long.".into()));
    }
    let query = query
        .trim()
        .trim_start_matches("[[")
        .trim_end_matches("]]")
        .trim_start_matches('/');
    let query = percent_encoding::percent_decode_str(query).decode_utf8_lossy();
    if query.contains('\0') {
        return Err(AppError::BadRequest(
            "The link search contains an invalid character.".into(),
        ));
    }
    let choices:Vec<(String,String,bool)>=sqlx::query_as(
        "SELECT token,label,is_note FROM (
             SELECT path AS token,title AS label,true AS is_note FROM notes WHERE owner_id=$1 AND project_id=$2
             UNION ALL SELECT $3 || '-' || number::text,title,false FROM issues WHERE owner_id=$1 AND project_id=$2 AND NOT archived
         ) AS candidates
         WHERE $4='' OR strpos(lower(token),lower($4))>0 OR strpos(lower(label),lower($4))>0
         ORDER BY (lower(token)=lower($4)) DESC,(lower(label)=lower($4)) DESC,lower(label),token LIMIT 60")
        .bind(owner).bind(project.id).bind(&project.key).bind(query.as_ref()).fetch_all(db).await?;
    Ok(choices
        .into_iter()
        .map(|(token, label, is_note)| Suggestion {
            token: if is_note {
                markdown::note_wikilink(&token)
            } else {
                format!("[[{token}]]")
            },
            label,
        })
        .collect())
}

async fn editor(
    db: &PgPool,
    auth: &Auth,
    id: Option<Uuid>,
    f: NoteForm,
    banner: Option<Markup>,
    preview: Option<Markup>,
) -> AppResult<Html<String>> {
    let project = store::project(db, auth.user_id, f.project).await?;
    let projects = if id.is_none() {
        store::projects(db, auth.user_id).await?
    } else {
        Vec::new()
    };
    let suggestions = link_suggestions(db, auth.user_id, &project, "").await?;
    let action = id.map_or_else(|| "/notes".to_owned(), |id| format!("/notes/{id}"));
    let title = if id.is_some() {
        "Edit note"
    } else {
        "New note"
    };
    let origin = index_origin(f.back.as_deref());
    let fallback = index_url(Some(f.project), "", 1);
    let back = origin.unwrap_or(&fallback);
    let cancel = id.map_or_else(
        || back.to_owned(),
        |id| with_origin(&format!("/notes/{id}"), origin),
    );
    Ok(page_in_project(
        auth,
        title,
        "notes",
        html! {
            nav.note-trail aria-label="Note location" {
                a href=(back) {"Notes"} span aria-hidden="true" {"/"}
                a href=(format!("/projects/{}",project.id)) {(project.name)}
            }
            header.view-head.note-editor-head { h1 {(title)} }
            div.note-writing {
                @if let Some(banner)=banner {(banner)}
                @if id.is_none() && projects.iter().filter(|p|!p.archived).count()>1 {
                    form.note-switch method="get" action="/notes/new" {
                        @if let Some(back)=origin {input type="hidden" name="back" value=(back);}
                        label.field { "Project" select name="project" { @for p in projects.iter().filter(|p|!p.archived){option value=(p.id) selected[p.id==f.project] {(p.name)}} }}
                        button.action-secondary type="submit" {"Switch project"}
                    }
                }
                @if let Some(preview)=preview {
                    section.note-preview {
                        div.section-head { h2{"Markdown preview"} a.action-quiet href="#note-source" {"Back to editor"} }
                        div.note-document.markdown {(preview)}
                    }
                }
                form.form-stack.note-editor method="post" action=(action) data-note-editor="" data-user=(auth.user_id) data-project=(f.project) data-note=(id.map(|id|id.to_string()).unwrap_or_default()) data-revision=(f.revision.map(|v|v.to_string()).unwrap_or_default()) {
                    (csrf(auth))
                    input type="hidden" name="project" value=(f.project);
                    @if let Some(back)=origin {input type="hidden" name="back" value=(back);}
                    @if let Some(id)=id {input type="hidden" name="note_id" value=(id);}
                    @if let Some(rev)=f.revision {input type="hidden" name="revision" value=(rev);}
                    div.status-note.note-draft-actions data-draft-actions="" hidden {
                        span {"A local draft is available."}
                        div.note-draft-buttons role="group" aria-label="Local draft actions" {
                            button.action-secondary type="button" data-restore="" {"Restore draft"}
                            button.action-quiet type="button" data-discard="" {"Discard draft"}
                        }
                    }
                    div.note-file-fields {
                        label.field { "Title" input name="title" value=(f.title) maxlength="200" required; }
                        label.field.note-path-field { "Markdown path" input name="path" value=(f.path) placeholder="decisions/link-resolution.md" required; }
                    }
                    div.note-source-field {
                        div.note-source-tools {
                            label for="note-source" {"Markdown"}
                            @if !suggestions.is_empty() {
                                details.note-link-tools data-link-tools="" data-suggestions-url=(format!("/notes/suggestions?project={}",f.project)) hidden {
                                    summary {"Insert link"}
                                    div.note-link-options {
                                        label.field {"Find a note or issue" input type="text" list="note-link-options" data-link-input="" autocomplete="off" aria-describedby="note-link-message" placeholder="Title, path, or issue ID"; }
                                        datalist id="note-link-options" { @for item in suggestions { option value=(item.token) {(item.label)} } }
                                        button.action-secondary type="button" data-insert-link="" {"Insert link"}
                                        p.note-link-message id="note-link-message" role="status" {}
                                    }
                                }
                            }
                        }
                        textarea id="note-source" name="body" rows="13" placeholder="Write in Markdown. Link a note or issue with [[double brackets]]." {"\n"(f.body)}
                    }
                    div.note-savebar {
                        button.action-primary type="submit" {"Save note"}
                        button.action-secondary type="submit" formaction="/notes/preview" formnovalidate {"Preview Markdown"}
                        a.action-quiet href=(cancel) {"Cancel"}
                    }
                    small.muted data-draft-status="" role="status" {}
                }
            }
            script src="/assets/note-editor.js" defer {}
        },
        Some(f.project),
    ))
}

#[derive(FromRow)]
struct Destination {
    ordinal: i32,
    raw: String,
    fragment: Option<String>,
    dest_note: Option<Uuid>,
    dest_issue: Option<Uuid>,
}

pub(super) async fn detail(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Query(query): Query<Saved>,
) -> AppResult<Html<String>> {
    let mut tx = s.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let note: Note = sqlx::query_as(
        "SELECT id,project_id,title,path,body,revision FROM notes WHERE id=$1 AND owner_id=$2",
    )
    .bind(id)
    .bind(auth.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let rows:Vec<Destination>=sqlx::query_as("SELECT ordinal,raw,fragment,dest_note,dest_issue FROM note_links WHERE owner_id=$1 AND source_id=$2 ORDER BY ordinal").bind(auth.user_id).bind(id).fetch_all(&mut *tx).await?;
    let catalog: Vec<(Uuid, String, String)> = if rows
        .iter()
        .any(|r| r.dest_note.is_none() && r.dest_issue.is_none())
    {
        sqlx::query_as(
            "SELECT id,path,title FROM notes WHERE owner_id=$1 AND project_id=$2 ORDER BY path",
        )
        .bind(auth.user_id)
        .bind(note.project_id)
        .fetch_all(&mut *tx)
        .await?
    } else {
        Vec::new()
    };
    tx.commit().await?;
    let project = store::project(&s.db, auth.user_id, note.project_id).await?;
    let doc = Document::parse(&note.body);
    let mut hrefs = vec![None; doc.links.len()];
    let mut unresolved = Vec::new();
    for row in rows {
        let ord = row.ordinal as usize;
        let Some(reference) = doc.links.get(ord).filter(|r| r.raw == row.raw) else {
            continue;
        };
        hrefs[ord] = if let Some(n) = row.dest_note {
            Some(match &row.fragment {
                Some(f) => format!(
                    "/notes/{n}#{}",
                    markdown::link_fragment(&reference.target, f)
                ),
                None => format!("/notes/{n}"),
            })
        } else if let Some(i) = row.dest_issue {
            Some(format!("/issues/{i}"))
        } else {
            unresolved.push((
                ord,
                reference,
                if index::native_issue(&reference.target).is_some() {
                    Vec::new()
                } else {
                    markdown::candidates(&note.path, reference, &catalog)
                },
            ));
            None
        };
    }
    let rendered = markdown::render(&note.body, &hrefs, note.project_id, &note.path);
    let paths: HashMap<Uuid, &str> = catalog
        .iter()
        .map(|(id, path, _)| (*id, path.as_str()))
        .collect();
    let panel = graph::note_panel(&s.db, auth.user_id, id).await?;
    let fallback = index_url(Some(project.id), "", 1);
    let origin = index_origin(query.back.as_deref());
    Ok(page_in_project(
        &auth,
        &note.title,
        "notes",
        html! {
            nav.note-trail aria-label="Note location" {
                a href=(origin.unwrap_or(&fallback)) {"Notes"} span aria-hidden="true" {"/"}
                a href=(format!("/projects/{}",project.id)) {(project.name)}
            }
            header.view-head.note-reader-head {
                div {
                    h1{(note.title)}
                    p.note-file-info { code{(note.path)} span { "Revision " (note.revision) } @if project.archived {span {"Archived project"}} }
                }
                div.view-actions {
                    @if !project.archived { a.action-primary href=(with_origin(&format!("/notes/{id}/edit"), origin)){(icon("edit")) "Edit note"} }
                    details.note-options data-disclosure-menu="" {
                        summary.action-secondary aria-label="More note actions" title="More note actions" {span aria-hidden="true" {"···"}}
                        div {a href=(with_origin(&format!("/notes/{id}/delete"), origin)) {"Delete note…"}}
                    }
                }
            }
            @if let Some(origin)=query.saved.as_deref().filter(|o|matches!(*o,"created"|"updated")){
                p.status-note role="status" data-note-saved="" data-user=(auth.user_id) data-project=(note.project_id) data-note=(id) data-origin=(origin) data-saved-path=(note.path) data-saved-revision=(note.revision) data-draft-token=(query.draft_token.map(|id|id.to_string()).unwrap_or_default()) {"Note saved."}
                script src="/assets/note-editor.js" defer {}
            }
            div.note-reader {
                div.note-reading-column {
                    @if note.body.is_empty(){
                        p.note-blank {"This note is empty."}
                    } @else {
                        article.note-document.markdown aria-label="Note content"{(rendered)}
                    }
                    @if !unresolved.is_empty() {
                        section.note-unresolved { h2{"Unresolved links"} ul {
                            @for (ord,reference,choices) in unresolved {
                                li {code{(reference.raw)} " "
                                    @if choices.is_empty(){span.muted{"No matching note or issue."}}
                                    @else if project.archived {span.muted{"Multiple matches."}}
                                    @else {
                                        form.note-resolve method="post" action=(format!("/notes/{id}/links/{ord}")) {
                                            (csrf(&auth)) input type="hidden" name="revision" value=(note.revision);
                                            label {"Choose a destination " select name="target" required {
                                                option value="" {"Select a note"}
                                                @for target in choices {option value=(target){(paths.get(&target).copied().unwrap_or(""))}}
                                            }}
                                            button.secondary {"Link note"}
                                        }
                                    }
                                }
                            }
                        }}
                    }
                    @if !project.archived && !doc.paragraphs.is_empty() {
                        details.note-promote id="create-issue" {
                            summary {"Create issue from a paragraph"}
                            form.form-stack method="post" action=(format!("/notes/{id}/issues")) {
                                (csrf(&auth)) input type="hidden" name="revision" value=(note.revision);
                                input type="hidden" name="command_id" value=(Uuid::new_v4());
                                label.field {"Source paragraph" select name="source_anchor" required {
                                    @for (i,p) in doc.paragraphs.iter().enumerate(){option value=(p.anchor){(i+1) ". " (p.excerpt.chars().take(140).collect::<String>())}}
                                }}
                                label.field {"Issue title" input name="title" maxlength="200" placeholder="Use the first line of the paragraph";}
                                button.action-secondary {"Create linked issue"}
                            }
                        }
                    }
                }
                aside.note-relationships aria-label="Related notes and issues" {(panel)}
            }
        },
        Some(note.project_id),
    ))
}

pub(super) async fn confirm_delete(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Query(q): Query<Saved>,
) -> AppResult<Html<String>> {
    let note = load_note(&s.db, auth.user_id, id).await?;
    let cancel = with_origin(&format!("/notes/{id}"), q.back.as_deref());
    let (incoming,issues):(i64,i64)=sqlx::query_as("SELECT (SELECT count(DISTINCT source_id) FROM note_links WHERE owner_id=$1 AND dest_note=$2 AND source_id<>$2),(SELECT count(*) FROM note_issue_sources WHERE owner_id=$1 AND note_id=$2)").bind(auth.user_id).bind(id).fetch_one(&s.db).await?;
    Ok(page_in_project(
        &auth,
        "Delete note",
        "notes",
        html! {
            nav.note-trail aria-label="Note location" {a href=(&cancel){"Back to " (note.title)} }
            header.view-head { h1 {"Permanently delete note"} }
            section.note-deletion.form-stack {
                p {strong{(note.title)} " · " code{(note.path)} " · Revision " (note.revision)}
                p {"Deleting removes the Markdown source, links, and source excerpts. " (incoming) " other notes link here. " (issues) " issues originated here and keep their issue descriptions."}
                p.muted {"Pending import previews for this project will be cleared. This deletion is permanent."}
                form.form-stack method="post" action=(format!("/notes/{id}/delete")) {
                    (csrf(&auth)) input type="hidden" name="revision" value=(note.revision);
                    label.field {"Type " code{(note.path)} " to confirm" input type="text" name="confirmation" required autocomplete="off";}
                    div.note-savebar {button.danger type="submit" {"Permanently delete note"} a.action-quiet href=(cancel){"Cancel"}}
                }
            }
        },
        Some(note.project_id),
    ))
}
