use super::*;
use crate::ui::{csrf, page};
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
    #[serde(default)]
    page: u32,
}
#[derive(Default, Deserialize)]
pub(super) struct Saved {
    saved: Option<String>,
    draft_token: Option<Uuid>,
}

pub(super) async fn editor_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("editor.js"),
    )
}

type NoteSummary = (Uuid, String, String, i64, String);

pub(super) async fn find_notes(
    db: &PgPool,
    owner: Uuid,
    project: Option<Uuid>,
    term: &str,
    offset: i64,
) -> AppResult<Vec<NoteSummary>> {
    Ok(sqlx::query_as(
        "SELECT n.id,n.title,n.path,n.revision,p.name FROM notes n JOIN projects p ON (p.owner_id,p.id)=(n.owner_id,n.project_id)
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
    let encoded =
        percent_encoding::utf8_percent_encode(term, percent_encoding::NON_ALPHANUMERIC).to_string();
    let more = rows.len() > 50;
    rows.truncate(50);
    let filter = q
        .project
        .map_or_else(String::new, |id| format!("project={id}&"));
    let filter = if term.is_empty() {
        filter
    } else {
        format!("{filter}q={encoded}&")
    };
    Ok(page(
        &auth,
        "Notes",
        "notes",
        html! {
            header.toolbar { div { h1 { "Notes" } @if let Some(p)=selected { a.muted href=(format!("/projects/{}",p.id)) {(p.name)} } }
                @if selected.is_some_and(|p|!p.archived) || (selected.is_none() && projects.iter().any(|p|!p.archived)){
                    a.button href=(q.project.map_or_else(||"/notes/new".to_owned(),|p|format!("/notes/new?project={p}"))) { "New note" }
                }
            }
            @if let Some((project,id))=deleted {
                p.notice role="status" data-note-deleted="" data-user=(auth.user_id) data-project=(project) data-note=(id) {"Note permanently deleted."}
                script src="/assets/note-editor.js" defer {}
            }
            @if projects.len()>1 {
                nav.toolbar aria-label="Filter notes by project" {
                    a href=(if term.is_empty(){"/notes".to_owned()}else{format!("/notes?q={encoded}")}) {"All projects"}
                    @for p in &projects {a href=(if term.is_empty(){format!("/notes?project={}",p.id)}else{format!("/notes?project={}&q={encoded}",p.id)}){(p.name)}}
                }
            }
            @if !projects.is_empty() {
                form.toolbar method="get" action="/notes" role="search" {
                    @if let Some(id)=q.project {input type="hidden" name="project" value=(id);}
                    label.field {"Search notes" input type="search" name="q" maxlength="160" value=(term) placeholder="Title, path, or Markdown text";}
                    button.secondary {"Search"}
                    @if !term.is_empty() {a href=(q.project.map_or_else(||"/notes".to_owned(),|id|format!("/notes?project={id}"))) {"Clear search"}}
                }
            }
            @if rows.is_empty() && !term.is_empty() {p.empty {"No notes match “" (term) "”."}}
            @else if rows.is_empty() {
                section.empty { h2 { "No notes here yet" } @if projects.is_empty() {a.button href="/projects" {"Create a project"}} @else if selected.is_some_and(|p|p.archived) { p {"This project is archived."} } @else { p {"Save a decision, link related notes, or turn a paragraph into an issue."} } }
            } @else {
                div.issue-list { @for (id,title,path,revision,project) in rows {
                    article.row {div {a href=(format!("/notes/{id}")) {strong {(title)}} div.muted {code {(path)}}} span.muted {(project) " · rev " (revision)}}
                }}
            }
            nav.toolbar aria-label="Note pages" {
                @if page_no>1 {a href=(format!("/notes?{filter}page={}",page_no-1)) {"Previous"}}
                @if more {a href=(format!("/notes?{filter}page={}",page_no+1)) {"Next"}}
            }
        },
    ))
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
            html! {h1{"New note"} section.empty {p{"Create an active project to save your first note."} a.button href="/projects" {"Open projects"}}},
        ));
    };
    let form = NoteForm {
        csrf: String::new(),
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
) -> AppResult<Html<String>> {
    let note = load_note(&s.db, auth.user_id, id).await?;
    store::writable(&s.db, auth.user_id, note.project_id).await?;
    let form = NoteForm {
        csrf: String::new(),
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
    Ok(page(
        auth,
        title,
        "notes",
        html! {
            header.toolbar {div {a.muted href=(format!("/notes?project={}",f.project)) {(project.name) " / Notes"} h1 {(title)} }}
            @if let Some(banner)=banner {(banner)}
            @if id.is_none() && projects.iter().filter(|p|!p.archived).count()>1 {
                form.toolbar method="get" action="/notes/new" { label { "Project " select name="project" { @for p in projects.iter().filter(|p|!p.archived){option value=(p.id) selected[p.id==f.project] {(p.name)}} }} button.secondary {"Switch project"} }
            }
            @if let Some(preview)=preview { section.panel {h2{"Markdown preview"} div.markdown {(preview)}} }
            form.stack method="post" action=(action) data-note-editor="" data-user=(auth.user_id) data-project=(f.project) data-note=(id.map(|id|id.to_string()).unwrap_or_default()) data-revision=(f.revision.map(|v|v.to_string()).unwrap_or_default()) {
                (csrf(auth))
                input type="hidden" name="project" value=(f.project);
                @if let Some(id)=id {input type="hidden" name="note_id" value=(id);}
                @if let Some(rev)=f.revision {input type="hidden" name="revision" value=(rev);}
                div.notice data-draft-actions="" hidden {
                    span {"A local draft is available. "}
                    button.secondary type="button" data-restore="" {"Restore draft"} " "
                    button.secondary type="button" data-discard="" {"Discard draft"}
                }
                label.field { "Title" input name="title" value=(f.title) maxlength="200" required; }
                label.field { "Markdown path" input name="path" value=(f.path) placeholder="decisions/link-resolution.md" required; }
                @if !suggestions.is_empty() {
                    div.toolbar data-link-tools="" data-suggestions-url=(format!("/notes/suggestions?project={}",f.project)) hidden {
                        label {"Link a note or issue " input type="text" list="note-link-options" data-link-input="" autocomplete="off"; }
                        datalist id="note-link-options" { @for item in suggestions { option value=(item.token) {(item.label)} } }
                        button.secondary type="button" data-insert-link="" {"Insert link"}
                    }
                }
                label.field {"Markdown" textarea name="body" rows="18" placeholder="Write a decision. Link [[other notes]] or a project issue." {"\n"(f.body)}}
                div.toolbar {
                    button.button type="submit" {"Save note"}
                    button.secondary type="submit" formaction="/notes/preview" formnovalidate {"Preview Markdown"}
                    a href=(id.map_or_else(||format!("/notes?project={}",f.project),|id|format!("/notes/{id}"))) {"Cancel"}
                }
                small.muted data-draft-status="" role="status" {}
            }
            script src="/assets/note-editor.js" defer {}
        },
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
    Ok(page(
        &auth,
        &note.title,
        "notes",
        html! {
            header.toolbar { div {a.muted href=(format!("/notes?project={}",note.project_id)){(project.name) " / Notes"} h1{(note.title)} p.muted{code{(note.path)} " · Revision " (note.revision)} }
                @if !project.archived {a.button href=(format!("/notes/{id}/edit")){"Edit note"}}
            }
            @if let Some(origin)=query.saved.as_deref().filter(|o|matches!(*o,"created"|"updated")){
                p.notice role="status" data-note-saved="" data-user=(auth.user_id) data-project=(note.project_id) data-note=(id) data-origin=(origin) data-saved-path=(note.path) data-saved-revision=(note.revision) data-draft-token=(query.draft_token.map(|id|id.to_string()).unwrap_or_default()) {"Note saved."}
                script src="/assets/note-editor.js" defer {}
            }
            @if note.body.is_empty(){p.muted{"This note is empty."}} @else {article.panel.markdown aria-label="Note content"{(rendered)}}
            @if !unresolved.is_empty() {
                section.panel { h2{"Unresolved links"} ul.stack {
                    @for (ord,reference,choices) in unresolved {
                        li {code{(reference.raw)} " "
                            @if choices.is_empty(){span.muted{"No matching note or issue."}}
                            @else if project.archived {span.muted{"Multiple matches."}}
                            @else {
                                form.toolbar method="post" action=(format!("/notes/{id}/links/{ord}")) {
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
                details.panel id="create-issue" {
                    summary {"Create issue from a paragraph"}
                    form.stack method="post" action=(format!("/notes/{id}/issues")) {
                        (csrf(&auth)) input type="hidden" name="revision" value=(note.revision);
                        input type="hidden" name="command_id" value=(Uuid::new_v4());
                        label.field {"Source paragraph" select name="source_anchor" required {
                            @for (i,p) in doc.paragraphs.iter().enumerate(){option value=(p.anchor){(i+1) ". " (p.excerpt.chars().take(140).collect::<String>())}}
                        }}
                        label.field {"Issue title" input name="title" maxlength="200" placeholder="Use the first line of the paragraph";}
                        button.button {"Create linked issue"}
                    }
                }
            }
            (panel)
            p {a.muted href=(format!("/notes/{id}/delete")) {"Delete note…"}}
        },
    ))
}

pub(super) async fn confirm_delete(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> AppResult<Html<String>> {
    let note = load_note(&s.db, auth.user_id, id).await?;
    let (incoming,issues):(i64,i64)=sqlx::query_as("SELECT (SELECT count(DISTINCT source_id) FROM note_links WHERE owner_id=$1 AND dest_note=$2 AND source_id<>$2),(SELECT count(*) FROM note_issue_sources WHERE owner_id=$1 AND note_id=$2)").bind(auth.user_id).bind(id).fetch_one(&s.db).await?;
    Ok(page(
        &auth,
        "Delete note",
        "notes",
        html! {
            header {a href=(format!("/notes/{id}")){"Back to " (note.title)} h1 {"Permanently delete note"}}
            section.panel.stack {
                p {strong{(note.title)} " · " code{(note.path)} " · Revision " (note.revision)}
                p {"Deleting removes the Markdown source, links, and source excerpts. " (incoming) " other notes link here. " (issues) " issues originated here and keep their issue descriptions."}
                p.muted {"Pending import previews for this project will be cleared. This deletion is permanent."}
                form.stack method="post" action=(format!("/notes/{id}/delete")) {
                    (csrf(&auth)) input type="hidden" name="revision" value=(note.revision);
                    label.field {"Type " code{(note.path)} " to confirm" input type="text" name="confirmation" required autocomplete="off";}
                    div.toolbar {button.danger type="submit" {"Permanently delete note"} a href=(format!("/notes/{id}")){"Cancel"}}
                }
            }
        },
    ))
}
