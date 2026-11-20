use super::{
    Filters, HistoryRow, ListItem, event_title, label, ledger_icon, priority_mark, status_mark,
};
use crate::{AppResult, ui};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(FromRow)]
struct Preview {
    description: String,
    next_action: String,
    note_id: Option<Uuid>,
    note_title: Option<String>,
    note_anchor: Option<String>,
    note_excerpt: Option<String>,
    recent: sqlx::types::Json<Vec<HistoryRow>>,
}

fn excerpt(text: &str, limit: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", text[..end].trim_end()),
        None => text.to_owned(),
    }
}

fn day(t: DateTime<Utc>) -> Markup {
    html! { time datetime=(t.to_rfc3339()) title=(t.format("%d %b %Y, %H:%M UTC")) { (t.format("%b %-d")) } }
}

pub(super) async fn render(
    db: &PgPool,
    owner: Uuid,
    item: &ListItem,
    f: &Filters,
) -> AppResult<Markup> {
    // Only the item already present on the current filtered page reaches this query.
    // Limit long excerpts in PostgreSQL so directory reads never transfer full note or issue bodies.
    let preview = sqlx::query_as::<_, Preview>(
        "SELECT left(i.body, 301) AS description, i.next_action,
                n.id AS note_id, n.title AS note_title, s.source_anchor AS note_anchor,
                left(s.excerpt, 181) AS note_excerpt,
                to_jsonb(ARRAY(SELECT e FROM (
                    SELECT action, old_status, new_status, occurred_at FROM issue_events
                    WHERE owner_id=i.owner_id AND project_id=i.project_id AND issue_id=i.id
                    ORDER BY occurred_at DESC,revision_after DESC NULLS LAST,id DESC LIMIT 3
                ) e)) AS recent
         FROM issues i LEFT JOIN note_issue_sources s
           ON (s.owner_id,s.project_id,s.issue_id)=(i.owner_id,i.project_id,i.id)
         LEFT JOIN notes n ON (n.owner_id,n.project_id,n.id)=(s.owner_id,s.project_id,s.note_id)
         WHERE i.owner_id=$1 AND i.project_id=$2 AND i.id=$3",
    )
    .bind(owner)
    .bind(item.project_id)
    .bind(item.id)
    .fetch_optional(db)
    .await?;
    let Some(p) = preview else {
        return Ok(html! {});
    };
    let key = format!("{}-{}", item.project_key, item.number);
    let has_action = !p.next_action.trim().is_empty();
    let detail = format!("/issues/{}", item.id);
    let close = format!(
        "{}#record-{}",
        f.change_url("inspect", "none", true),
        item.id
    );
    let return_to_queue = format!("{}#record-{}", f.change_url("inspect", "", true), item.id);
    Ok(html! {
        aside.issue-preview id="issue-preview" tabindex="-1" aria-labelledby="preview-title" data-issue=(item.id) {
            div.preview-top {
                span.preview-context {
                    a href=(format!("/projects/{}", item.project_id)) { (&item.project_name) }
                    span.preview-divider aria-hidden="true" { "/" }
                    code { (&key) }
                }
                div.preview-tools {
                    a.preview-open href=(&detail) { "Open issue" (ledger_icon("open")) }
                    a.preview-dismiss.preview-close href=(close) aria-label="Close preview and show the full issue list" title="Close preview" {
                        (ledger_icon("close"))
                    }
                    a.preview-dismiss.preview-return href=(return_to_queue) aria-label="Return to the issue queue" title="Return to the queue" {
                        (ledger_icon("close"))
                    }
                }
            }
            div.preview-reading {
                article.preview-document aria-labelledby="preview-title" {
                    header.preview-issue-head {
                        h2 id="preview-title" tabindex="-1" { (&item.title) }
                        div.preview-fields {
                            span.preview-status { (status_mark(&item.status)) (label(&item.status)) }
                            span { (priority_mark(&item.priority)) (label(&item.priority)) " priority" }
                            span { (label(&item.kind)) }
                            @if item.archived || item.project_archived { span { "Archived" } }
                        }
                    }
                    @if has_action {
                        section.preview-action {
                            header {
                                h3 { (ui::icon("resume")) "Next action" }
                                @if let Some(stamp) = item.checkpoint_at {
                                    small { "Saved " (day(stamp)) }
                                }
                            }
                            p { (excerpt(&p.next_action, 350)) }
                        }
                    }
                    @if !p.description.trim().is_empty() {
                        section.preview-description { h3 { "Description" } p { (excerpt(&p.description, 300)) } }
                    }
                    @if !has_action && !item.archived && !item.project_archived && matches!(item.status.as_str(), "ready" | "backlog" | "in_progress") {
                        a.preview-add-action href=(format!("{detail}#next-action-title")) { (ui::icon("plus")) "Add a next action" }
                    }
                    @if let (Some(id), Some(title), Some(anchor)) = (p.note_id, p.note_title, p.note_anchor) {
                        section.preview-source {
                            h3 { (ui::icon("notes")) "Source note" }
                            a href=(format!("/notes/{id}#{anchor}")) { (title) (ledger_icon("open")) }
                            @if let Some(source) = p.note_excerpt.filter(|s| !s.trim().is_empty()) { p { (excerpt(&source, 180)) } }
                        }
                    }
                    @if !p.recent.is_empty() {
                        details.preview-activity open {
                            summary { "Recent activity" (ledger_icon("chevron")) }
                            ol { @for event in &p.recent.0 {
                                li { div {
                                    span { (event_title(&event.action)) }
                                    @if let Some(old) = &event.old_status && old != &event.new_status {
                                        small { (label(old)) " → " (label(&event.new_status)) }
                                    }
                                } (day(event.occurred_at)) }
                            } }
                        }
                    }
                }
            }
        }
    })
}
