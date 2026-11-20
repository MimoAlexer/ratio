//! Native, one-hop neighborhoods shared by note and issue detail pages.
use maud::{Markup, html};
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Entity {
    Note,
    Issue,
    Release,
}

struct Node {
    entity: Entity,
    id: Uuid,
    title: String,
    path: Option<String>,
}
struct Source {
    excerpt: String,
    revision: i64,
    found: bool,
}
struct Edge {
    peer: Node,
    relation: String,
    href: String,
    source: Option<Source>,
    section: Option<String>,
}

const COMPACT_EDGES: i64 = 8;
const MAX_EDGES: i64 = 250;
const MAX_NODES: u16 = 100;
const DEFAULT_NODES: u16 = 25;

#[derive(Clone, Copy)]
enum GraphView {
    Inline,
    Standalone { page: u32, nodes: u16 },
}

#[derive(Default)]
struct GraphStats {
    start: i64,
    total_edges: i64,
    total_nodes: i64,
    pages: i64,
}

impl GraphView {
    fn page(self) -> u32 {
        match self {
            Self::Inline => 1,
            Self::Standalone { page, .. } => page,
        }
    }

    fn url(self, focus: &Node, page: u32) -> String {
        match self {
            Self::Inline => focus.graph_url(),
            Self::Standalone { nodes, .. } => {
                format!("{}?nodes={nodes}&page={page}", focus.graph_url())
            }
        }
    }
}

impl Entity {
    fn route(self) -> &'static str {
        match self {
            Self::Note => "notes",
            Self::Issue => "issues",
            Self::Release => "releases",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Note => "Note",
            Self::Issue => "Issue",
            Self::Release => "Release",
        }
    }
}

impl Node {
    fn url(&self) -> String {
        format!("/{}/{}", self.entity.route(), self.id)
    }
    fn graph_url(&self) -> String {
        format!("/graph/{}/{}", self.entity.route(), self.id)
    }
}

fn shortened(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    let mut result: String = value.chars().take(width.saturating_sub(1)).collect();
    result.push('…');
    result
}

fn glyph(entity: Entity, x: i32, y: i32) -> Markup {
    html! {
        g transform=(format!("translate({x} {y})")) fill="none" stroke="currentColor" stroke-width="1.8" {
            @match entity {
                Entity::Note => path d="M-6 -8 H2 L6 -4 V8 H-6 Z M2 -8 V-4 H6" {},
                Entity::Issue => circle cx="0" cy="0" r="7" {},
                Entity::Release => path d="M0 -8 L8 0 L0 8 L-8 0 Z" {},
            }
        }
    }
}

fn compact_diagram(focus: &Node, edges: &[Edge]) -> Markup {
    // Keep type at its native size at every width. Percentage coordinates adapt the
    // two columns while HTML labels inside the SVG can wrap at word boundaries.
    let row_height = 66;
    let height = row_height * edges.len() as i32 + 16;
    let middle = height / 2;
    let first = 8 + row_height / 2;
    let last = height - first;
    html! {
        svg.relationship-graph.map-graph width="100%" height=(height)
            role="group" aria-label=(format!("Local graph of {}", focus.title)) {
            title { (focus.title) " and its direct relationships" }
            defs {
                @for entity in [Entity::Note, Entity::Issue, Entity::Release] {
                    symbol id=(format!("graph-symbol-{}", entity.route())) viewBox="-10 -10 20 20" {
                        (glyph(entity, 0, 0))
                    }
                }
            }
            line.map-wire x1="29%" y1=(middle) x2="37%" y2=(middle) {}
            line.map-wire x1="37%" y1=(first) x2="37%" y2=(last) {}
            g data-graph-node="true" {
                circle cx="29%" cy=(middle) r="3.5" fill="currentColor" {}
                foreignObject x="0" y=(middle-33) width="29%" height="66" {
                    div.map-focus xmlns="http://www.w3.org/1999/xhtml" {
                        small { (focus.entity.name()) }
                        span { (&focus.title) }
                    }
                }
            }
            @for (i, edge) in edges.iter().enumerate() {
                @let top = 8 + i as i32 * row_height;
                @let y = top + row_height/2;
                line.map-wire x1="37%" y1=(y) x2="43%" y2=(y) {}
                a data-relationship-edge="true" href=(&edge.href) aria-label=(format!("{} · {} · {}{}", edge.relation, edge.peer.entity.name(), edge.peer.title, edge.section.as_ref().map(|s|format!(" § {s}")).unwrap_or_default())) {
                    title { (&edge.relation) " · " (&edge.peer.title) @if let Some(section)=&edge.section { " § " (section) } }
                    rect class="map-hit" x="41%" y=(top+3) width="59%" height=(row_height-6) rx="3" fill="transparent" {}
                    g data-graph-node="true" {
                        use x="43%" y=(y-10) width="20" height="20" href=(format!("#graph-symbol-{}",edge.peer.entity.route())) {}
                    }
                    foreignObject x="43%" y=(top+2) width="57%" height=(row_height-4) {
                        div.map-peer xmlns="http://www.w3.org/1999/xhtml" {
                            small { (&edge.relation) }
                            span {
                                (&edge.peer.title)
                                @if let Some(section)=&edge.section { " § " (section) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn entity_key(node: &Node) -> (Entity, Uuid) {
    (node.entity, node.id)
}

fn page_node_count(focus: &Node, edges: &[Edge]) -> usize {
    let mut nodes = HashSet::from([entity_key(focus)]);
    nodes.extend(edges.iter().map(|edge| entity_key(&edge.peer)));
    nodes.len()
}

// A shared vertical trunk keeps the large graph legible. Each peer has one
// symbol, while each typed or anchored relationship has its own linked row.
fn lane_diagram(focus: &Node, edges: &[Edge]) -> Markup {
    let mut y = 62;
    let mut groups = Vec::new();
    for group in edges.chunk_by(|a, b| entity_key(&a.peer) == entity_key(&b.peer)) {
        groups.push((y, group));
        y += 38 + group.len() as i32 * 32;
    }
    let height = y + 8;
    let last_edge_y = y - 38;
    html! {
        svg class="relationship-graph lane-graph" width="680" height=(height) viewBox=(format!("0 0 680 {height}")) role="group" aria-label=(format!("Local graph of {}",focus.title)) {
            title { (focus.title) " and its direct relationships" }
            path d=(format!("M25 35 V{last_edge_y}")) fill="none" stroke="currentColor" stroke-opacity=".45" {}
            g data-graph-node="true" {
                (glyph(focus.entity,25,22))
                text x="48" y="26" fill="currentColor" font-size="15" font-weight="600" { (shortened(&focus.title,48)) }
            }
            @for (top, group) in groups {
                @let peer = &group[0].peer;
                @let bottom = top + group.len() as i32 * 32;
                path d=(format!("M78 {top} H64 V{bottom}")) fill="none" stroke="currentColor" stroke-opacity=".55" {}
                a href=(peer.url()) aria-label=(format!("Open {} · {}",peer.entity.name(),peer.title)) {
                    @if entity_key(peer) != entity_key(focus) {
                        g data-graph-node="true" { (glyph(peer.entity,90,top)) }
                    }
                    text x="110" y=(top+5) font-size="15" font-weight="600" fill="currentColor" {
                        @if entity_key(peer) == entity_key(focus) { "This item · " }
                        (shortened(&peer.title,54))
                    }
                }
                @for (n, edge) in group.iter().enumerate() {
                    @let row = top + ((n + 1) as i32) * 32;
                    a data-relationship-edge="true" href=(&edge.href) aria-label=(format!("{} · {}{}",edge.relation,peer.title,edge.section.as_ref().map(|s|format!(" § {s}")).unwrap_or_default())) {
                        title { (&edge.relation) " · " (&peer.title) @if let Some(section)=&edge.section { " § " (section) } }
                        rect x="24" y=(row-14) width="652" height="30" fill="transparent" {}
                        path d=(format!("M25 {row} H64")) stroke="currentColor" stroke-opacity=".65" fill="none" {}
                        circle cx="64" cy=(row) r="2.5" fill="currentColor" {}
                        text x="111" y=(row+5) font-size="13" fill="currentColor" { (&edge.relation) }
                        @if let Some(section)=&edge.section {
                            text x="300" y=(row+5) font-size="13" fill="currentColor" opacity=".8" { "§ " (shortened(section,39)) }
                        }
                    }
                }
            }
        }
    }
}

fn render_panel(focus: &Node, edges: &[Edge], stats: &GraphStats, view: GraphView) -> Markup {
    let page = view.page();
    let displayed_nodes = page_node_count(focus, edges);
    let expanded = matches!(view, GraphView::Standalone { .. });
    let show_lanes =
        expanded && (edges.len() > COMPACT_EDGES as usize || displayed_nodes != edges.len() + 1);
    let end = stats.start + edges.len() as i64;
    let other_edges = stats.total_edges - edges.len() as i64;
    html! {
        section.relationships.relationships-inline[!expanded].relationships-full[expanded] id="relationships"
            aria-labelledby="relationships-heading" {
            div.relationships-head {
                h2 id="relationships-heading" {
                    @if expanded { "Relationship graph" } @else { "Relationships" }
                    @if !expanded && !edges.is_empty() { span.relationship-count { (stats.total_edges) } }
                }
                @if !expanded && !edges.is_empty() {
                    a.relationship-action href=(focus.graph_url()) { "Open full graph" }
                }
            }
            @if edges.is_empty() {
                p.relationship-summary { "No linked items yet." }
            } @else {
                @if let GraphView::Standalone { nodes, .. } = view {
                    div.graph-tools {
                        div {
                            p.relationship-summary { (displayed_nodes) " of " (stats.total_nodes) " nodes · " (stats.start+1) "–" (end) " of " (stats.total_edges) " relationships" }
                            @if other_edges > 0 { p.relationship-summary { (other_edges) " relationships outside this page." } }
                        }
                        div.graph-commands {
                            a.relationship-jump href="#relationship-list" { "Jump to list" }
                            details.graph-options {
                                summary { "Graph options" }
                                form method="get" action=(focus.graph_url()) {
                                    label.field for="graph-size" { "Nodes per page" }
                                    div.graph-options-controls {
                                        input id="graph-size" type="number" name="nodes" min="2" max=(MAX_NODES) value=(nodes) required aria-describedby="graph-size-help";
                                        button.action-secondary type="submit" { "Update graph" }
                                    }
                                    small id="graph-size-help" { "Includes the starting item. Choose 2–" (MAX_NODES) "." }
                                }
                            }
                        }
                    }
                } @else if stats.total_edges > end {
                    p.relationship-summary { "1–" (end) " of " (stats.total_edges) " relationships" }
                }
                details.graph-drawing open[expanded] {
                    summary { @if expanded { "Diagram" } @else { "Graph preview" } }
                    @if show_lanes {
                        div.graph-viewport tabindex="0" role="region" aria-label="Scrollable relationship graph" { (lane_diagram(focus,edges)) }
                    } @else { (compact_diagram(focus,edges)) }
                }
                @if expanded { h3.relationship-list-title { "Linked items" } }
                ol.relationship-list id="relationship-list" tabindex="-1" role="list" start=(stats.start + 1) {
                    @for edge in edges {
                        li {
                            div.relationship-record {
                                span.relationship-kind data-kind=(edge.peer.entity.route()) { span.sr-only { (edge.peer.entity.name()) } }
                                div.relationship-info {
                                    span.relationship-type { (&edge.relation) }
                                    a.relationship-target href=(&edge.href) {
                                        span { (&edge.peer.title) @if let Some(section)=&edge.section { " § " (section) } }
                                        @if let Some(path) = &edge.peer.path { small.relationship-path { (path) } }
                                    }
                                }
                                @if edge.peer.entity != Entity::Release {
                                    a.relationship-action.relationship-explore href=(view.url(&edge.peer,1)) aria-label=(format!("Explore relationships for {}", edge.peer.title)) title=(format!("Explore relationships for {}", edge.peer.title)) { "Graph" }
                                }
                            }
                            @if let Some(source) = &edge.source {
                                @if !source.found { p.relationship-summary { "The source paragraph has changed. Saved excerpt from revision " (source.revision) "." } }
                                blockquote { (&source.excerpt) }
                            }
                        }
                    }
                }
                @if expanded && stats.pages > 1 {
                    nav.graph-pages aria-label="Relationship pages" {
                        span { "Page " (page) " of " (stats.pages) }
                        @if page > 1 { a rel="prev" href=(view.url(focus,page-1)) { "Previous relationships" } }
                        @if i64::from(page)<stats.pages { a rel="next" href=(view.url(focus,page+1)) { "Next relationships" } }
                    }
                }
            }
        }
    }
}

use axum::{
    Router,
    extract::{Path, Query, State},
    response::Html,
    routing::get,
};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};

use super::markdown::{Document, link_fragment};
use crate::{AppError, AppResult, AppState, auth::Auth, ui};

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/graph/{kind}/{id}", get(full_graph))
}

#[derive(Deserialize)]
struct GraphQuery {
    page: Option<u32>,
    nodes: Option<u16>,
}

impl GraphQuery {
    fn view(&self) -> AppResult<GraphView> {
        let nodes = self.nodes.unwrap_or(DEFAULT_NODES);
        if !(2..=MAX_NODES).contains(&nodes) {
            return Err(AppError::BadRequest(format!(
                "Choose between 2 and {MAX_NODES} nodes."
            )));
        }
        let page = self.page.unwrap_or(1);
        if page == 0 {
            return Err(AppError::NotFound);
        }
        Ok(GraphView::Standalone { page, nodes })
    }
}

async fn full_graph(
    auth: Auth,
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, Uuid)>,
    Query(query): Query<GraphQuery>,
) -> AppResult<Html<String>> {
    let entity = match kind.as_str() {
        "notes" => Entity::Note,
        "issues" => Entity::Issue,
        _ => return Err(AppError::NotFound),
    };
    let view = query.view()?;
    let focus = load_focus(&state.db, auth.user_id, entity, id).await?;
    let panel = neighborhood(&state.db, auth.user_id, &focus.0, focus.1, view).await?;
    Ok(ui::page_in_project(
        &auth,
        "Local graph",
        entity.route(),
        html! {
            div.graph-page {
                header.view-head {
                    div {
                        a.graph-back href=(focus.0.url()) { (ui::icon("arrow")) "Back to " (entity.name().to_lowercase()) }
                        h1 { (&focus.0.title) }
                    }
                }
                (panel)
            }
        },
        Some(focus.1),
    ))
}

pub(super) async fn note_panel(db: &PgPool, owner: Uuid, note: Uuid) -> AppResult<Markup> {
    inline_graph(db, owner, Entity::Note, note).await
}

pub async fn issue_panel(db: &PgPool, owner: Uuid, issue: Uuid) -> AppResult<Markup> {
    inline_graph(db, owner, Entity::Issue, issue).await
}

async fn inline_graph(db: &PgPool, owner: Uuid, entity: Entity, id: Uuid) -> AppResult<Markup> {
    let (focus, project) = load_focus(db, owner, entity, id).await?;
    neighborhood(db, owner, &focus, project, GraphView::Inline).await
}

async fn load_focus(db: &PgPool, owner: Uuid, entity: Entity, id: Uuid) -> AppResult<(Node, Uuid)> {
    let sql = match entity {
        Entity::Note => "SELECT title,project_id FROM notes WHERE owner_id=$1 AND id=$2",
        Entity::Issue => "SELECT p.key || '-' || i.number::text || ' · ' || i.title,i.project_id
                          FROM issues i JOIN projects p ON p.id=i.project_id AND p.owner_id=i.owner_id
                          WHERE i.owner_id=$1 AND i.id=$2",
        Entity::Release => return Err(AppError::NotFound),
    };
    let (title, project) = sqlx::query_as::<_, (String, Uuid)>(sql)
        .bind(owner)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((
        Node {
            entity,
            id,
            title,
            path: None,
        },
        project,
    ))
}

// The first branch in each query names the returned columns. A UNION collapses
// repeated references to the same target and section within a single note.
const NOTE_EDGES: &str = r#"
SELECT 0 AS rank, 'Created issue' AS relation, 'issues' AS kind, issue_id AS peer, NULL::text AS fragment, ''::text AS written_target
  FROM note_issue_sources WHERE owner_id=$1 AND note_id=$2 AND project_id=$3
UNION
SELECT 1, CASE kind WHEN 'wiki' THEN 'Wiki link to' ELSE 'Markdown link to' END, 'notes', dest_note, fragment,
  CASE WHEN target LIKE '/notes/%' AND fragment ~ '^(heading|block|p)-' THEN target ELSE '' END
  FROM note_links WHERE owner_id=$1 AND source_id=$2 AND project_id=$3 AND dest_note IS NOT NULL
UNION
SELECT 2, CASE kind WHEN 'wiki' THEN 'Wiki link to' ELSE 'Markdown link to' END, 'issues', dest_issue, NULL::text, ''
  FROM note_links WHERE owner_id=$1 AND source_id=$2 AND project_id=$3 AND dest_issue IS NOT NULL
UNION
SELECT 3, CASE kind WHEN 'wiki' THEN 'Wiki backlink from' ELSE 'Markdown backlink from' END, 'notes', source_id, NULL::text, ''
  FROM note_links WHERE owner_id=$1 AND dest_note=$2 AND project_id=$3 AND source_id<>$2
"#;

const ISSUE_EDGES: &str = r#"
SELECT 0 AS rank, 'Decided in' AS relation, 'notes' AS kind, note_id AS peer, NULL::text AS fragment, ''::text AS written_target
  FROM note_issue_sources WHERE owner_id=$1 AND issue_id=$2 AND project_id=$3
UNION
SELECT 1, 'Blocked by', 'issues', blocker_id, NULL::text, ''
  FROM issue_blockers WHERE owner_id=$1 AND issue_id=$2 AND project_id=$3
UNION
SELECT 2, 'Blocks', 'issues', issue_id, NULL::text, ''
  FROM issue_blockers WHERE owner_id=$1 AND blocker_id=$2 AND project_id=$3
UNION
SELECT 3, 'In release scope', 'releases', release_id, NULL::text, ''
  FROM release_members m JOIN releases r ON r.id=m.release_id AND r.owner_id=m.owner_id AND r.project_id=m.project_id
  WHERE m.owner_id=$1 AND m.issue_id=$2 AND m.project_id=$3 AND r.published_at IS NULL
UNION
SELECT 4, 'Shipped in', 'releases', release_id, NULL::text, ''
  FROM release_entries e JOIN releases r ON r.id=e.release_id AND r.owner_id=e.owner_id AND r.project_id=e.project_id
  WHERE e.owner_id=$1 AND e.issue_id=$2 AND e.project_id=$3 AND r.published_at IS NOT NULL
UNION
SELECT 5, CASE kind WHEN 'wiki' THEN 'Wiki backlink from' ELSE 'Markdown backlink from' END, 'notes', source_id, NULL::text, ''
  FROM note_links WHERE owner_id=$1 AND dest_issue=$2 AND project_id=$3
"#;

#[derive(FromRow)]
struct RelationshipRow {
    relation: String,
    kind: String,
    peer: Uuid,
    title: String,
    note_title: Option<String>,
    note_path: Option<String>,
    fragment: Option<String>,
    written_target: String,
    source_anchor: Option<String>,
    excerpt: Option<String>,
    note_revision: Option<i64>,
    note_body: Option<String>,
    total: i64,
    total_nodes: i64,
    pages: i64,
    position: i64,
}

impl RelationshipRow {
    fn into_edge(self) -> AppResult<Edge> {
        let entity = match self.kind.as_str() {
            "notes" => Entity::Note,
            "issues" => Entity::Issue,
            "releases" => Entity::Release,
            _ => return Err(AppError::Internal("Invalid relationship type".to_owned())),
        };
        let peer = Node {
            entity,
            id: self.peer,
            title: self.note_title.unwrap_or(self.title),
            path: self.note_path,
        };
        let mut href = peer.url();
        let mut source = None;
        if let (Some(anchor), Some(excerpt), Some(revision), Some(body)) = (
            self.source_anchor,
            self.excerpt,
            self.note_revision,
            self.note_body,
        ) {
            let found = Document::parse(&body)
                .paragraphs
                .iter()
                .any(|p| p.anchor == anchor);
            if found {
                href.push('#');
                href.push_str(&anchor);
            }
            source = Some(Source {
                excerpt,
                revision,
                found,
            });
        } else if let Some(fragment) = &self.fragment {
            href.push('#');
            href.push_str(&link_fragment(&self.written_target, fragment));
        }
        Ok(Edge {
            peer,
            relation: self.relation,
            href,
            source,
            section: self.fragment,
        })
    }
}

// Rank neighbors before applying the edge cap, so a peer stays on the same
// page unless its node group alone exceeds 250 distinct typed relationships.
const FULL_PAGES: &str = r#"
    ranked AS (
        SELECT v.*, MIN(rank) OVER (PARTITION BY kind,peer) AS peer_priority FROM visible v
    ), numbered AS (
        SELECT r.*, DENSE_RANK() OVER (ORDER BY peer_priority,lower(title),kind,peer) - 1 AS peer_index
        FROM ranked r
    ), buckets AS (
        SELECT n.*, peer_index / $4 AS node_bucket,
               MAX(peer_index) OVER () + 2 - MAX((peer=$2 AND kind=$6)::int) OVER () AS total_nodes
        FROM numbered n
    ), slices AS (
        SELECT b.*, (ROW_NUMBER() OVER (PARTITION BY node_bucket ORDER BY peer_index,rank,relation,fragment NULLS FIRST,written_target)-1)/$7 AS edge_bucket
        FROM buckets b
    ), positions AS (
        SELECT s.*, DENSE_RANK() OVER (ORDER BY node_bucket,edge_bucket) AS page_no,
               ROW_NUMBER() OVER (ORDER BY peer_index,rank,relation,fragment NULLS FIRST,written_target) AS position,
               COUNT(*) OVER () AS total
        FROM slices s
    ), counted AS (
        SELECT v.*, MAX(page_no) OVER () AS pages FROM positions v
    ), paged AS (
        SELECT * FROM counted WHERE page_no=$5
    )
"#;

const INLINE_PAGE: &str = r#"
    paged AS (
        SELECT v.*, COUNT(*) OVER () AS total,0::bigint AS total_nodes,1::bigint AS pages,
            ROW_NUMBER() OVER (ORDER BY rank,lower(title),kind,peer,relation,fragment NULLS FIRST,written_target) AS position
        FROM visible v ORDER BY position LIMIT $4
    )
"#;

async fn neighborhood(
    db: &PgPool,
    owner: Uuid,
    focus: &Node,
    project: Uuid,
    view: GraphView,
) -> AppResult<Markup> {
    let edges_sql = match focus.entity {
        Entity::Note => NOTE_EDGES,
        Entity::Issue => ISSUE_EDGES,
        Entity::Release => return Err(AppError::NotFound),
    };
    let paging = match view {
        GraphView::Inline => INLINE_PAGE,
        GraphView::Standalone { .. } => FULL_PAGES,
    };
    let sql = format!(
        r#"
        WITH edges AS ({edges_sql}),
        visible AS (
            SELECT e.*, COALESCE(n.title || ' · ' || n.path, p.key || '-' || i.number::text || ' · ' || i.title, r.name) AS title,
                   n.title AS note_title, n.path AS note_path
            FROM edges e
            LEFT JOIN notes n ON e.kind='notes' AND n.id=e.peer AND n.owner_id=$1 AND n.project_id=$3
            LEFT JOIN issues i ON e.kind='issues' AND i.id=e.peer AND i.owner_id=$1 AND i.project_id=$3
            LEFT JOIN projects p ON p.id=i.project_id AND p.owner_id=$1
            LEFT JOIN releases r ON e.kind='releases' AND r.id=e.peer AND r.owner_id=$1 AND r.project_id=$3
            WHERE COALESCE(n.id,i.id,r.id) IS NOT NULL
        ), {paging}
        SELECT a.*, s.source_anchor,s.excerpt,s.note_revision,n.body AS note_body
        FROM paged a
        LEFT JOIN note_issue_sources s ON a.relation='Decided in' AND s.issue_id=$2
            AND s.owner_id=$1 AND s.project_id=$3 AND s.note_id=a.peer
        LEFT JOIN notes n ON n.id=s.note_id AND n.owner_id=$1 AND n.project_id=$3
        ORDER BY a.position
    "#
    );
    let query = sqlx::query_as::<_, RelationshipRow>(&sql)
        .bind(owner)
        .bind(focus.id)
        .bind(project);
    let query = match view {
        GraphView::Inline => query.bind(COMPACT_EDGES),
        GraphView::Standalone { nodes, page } => query
            .bind(i64::from(nodes) - 1)
            .bind(i64::from(page))
            .bind(focus.entity.route())
            .bind(MAX_EDGES),
    };
    let rows = query.fetch_all(db).await?;
    if rows.is_empty() && view.page() > 1 {
        return Err(AppError::NotFound);
    }
    let stats = rows
        .first()
        .map(|row| GraphStats {
            start: row.position - 1,
            total_edges: row.total,
            total_nodes: row.total_nodes,
            pages: row.pages,
        })
        .unwrap_or_default();
    let edges: Vec<_> = rows
        .into_iter()
        .map(RelationshipRow::into_edge)
        .collect::<AppResult<_>>()?;
    Ok(render_panel(focus, &edges, &stats, view))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(entity: Entity, id: u128, title: &str) -> Node {
        Node {
            entity,
            id: Uuid::from_u128(id),
            title: title.to_owned(),
            path: None,
        }
    }

    #[test]
    fn links_match_in_graph_and_list_and_open_the_source_paragraph() {
        let focus = node(Entity::Issue, 1, "LU-1 · Shipping");
        let edges = vec![
            Edge {
                peer: node(Entity::Note, 3, "Design <choice>"),
                relation: "Decided in".into(),
                href: "/notes/three#block-design".into(),
                section: None,
                source: Some(Source {
                    excerpt: "Ship one change.".into(),
                    revision: 2,
                    found: true,
                }),
            },
            Edge {
                peer: node(Entity::Issue, 2, "LU-2 · Test"),
                relation: "Blocked by".into(),
                href: "/issues/two".into(),
                section: None,
                source: None,
            },
        ];
        let rendered = render_panel(
            &focus,
            &edges,
            &GraphStats {
                start: 0,
                total_edges: 2,
                total_nodes: 3,
                pages: 1,
            },
            GraphView::Standalone { page: 1, nodes: 25 },
        )
        .into_string();
        let graph = rendered.split("</svg>").next().unwrap();
        let list = rendered.split("</svg>").nth(1).unwrap();
        for section in [graph, list] {
            assert!(
                section.find("/notes/three#block-design").unwrap()
                    < section.find("/issues/two").unwrap()
            );
            assert!(section.contains("Decided in"));
            assert!(section.contains("Blocked by"));
        }
        assert!(list.contains("Ship one change."));
        assert!(list.contains("Design &lt;choice&gt;"));
        assert!(list.contains("/graph/notes/00000000-0000-0000-0000-000000000003"));
    }

    #[test]
    fn missing_anchor_keeps_the_excerpt_and_opens_the_note() {
        let focus = node(Entity::Issue, 1, "LU-1");
        let edges = [Edge {
            peer: node(Entity::Note, 3, "Design"),
            relation: "Decided in".into(),
            href: "/notes/three".into(),
            section: None,
            source: Some(Source {
                excerpt: "<old paragraph>".into(),
                revision: 1,
                found: false,
            }),
        }];
        let rendered = render_panel(
            &focus,
            &edges,
            &GraphStats {
                start: 0,
                total_edges: 1,
                total_nodes: 2,
                pages: 1,
            },
            GraphView::Standalone { page: 1, nodes: 25 },
        )
        .into_string();
        assert!(rendered.contains("paragraph has changed"));
        assert!(rendered.contains("&lt;old paragraph&gt;"));
        assert!(rendered.contains("href=\"/notes/three\""));
    }

    #[sqlx::test]
    async fn persisted_relationships_expand_and_source_falls_back_after_edit(db: PgPool) {
        let owner: Uuid = sqlx::query_scalar("INSERT INTO users(email) VALUES($1) RETURNING id")
            .bind(format!("graph-{}@example.invalid", Uuid::new_v4()))
            .fetch_one(&db)
            .await
            .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,key,name) VALUES($1,'LU','Graph fixture') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
        let note: Uuid = sqlx::query_scalar("INSERT INTO notes(owner_id,project_id,path,title,body) VALUES($1,$2,'a.md','Design','Ship one change. ^ship-plan') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        let issue: Uuid = sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title) VALUES($1,$2,1,'One') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        sqlx::query("INSERT INTO note_issue_sources(owner_id,project_id,note_id,issue_id,source_anchor,excerpt,note_revision) VALUES($1,$2,$3,$4,'block-ship-plan','Ship one change.',1)")
            .bind(owner).bind(project).bind(note).bind(issue).execute(&db).await.unwrap();
        sqlx::query("WITH blockers AS (INSERT INTO issues(owner_id,project_id,number,title) SELECT $1,$2,n,('Blocker '||n::text) FROM generate_series(2,11) n RETURNING id) INSERT INTO issue_blockers(owner_id,project_id,issue_id,blocker_id) SELECT $1,$2,$3,id FROM blockers")
            .bind(owner).bind(project).bind(issue).execute(&db).await.unwrap();

        let graph = issue_panel(&db, owner, issue).await.unwrap().into_string();
        assert!(graph.contains(&format!("/notes/{note}#block-ship-plan")));
        assert!(graph.contains("1–8 of 11 relationships"));
        assert!(graph.contains("Blocked by"));
        let (focus, project) = load_focus(&db, owner, Entity::Issue, issue).await.unwrap();
        let later = neighborhood(
            &db,
            owner,
            &focus,
            project,
            GraphView::Standalone { page: 2, nodes: 9 },
        )
        .await
        .unwrap()
        .into_string();
        assert!(later.contains("9–11 of 11 relationships"));
        assert!(later.contains("Previous relationships"));
        let reverse = note_panel(&db, owner, note).await.unwrap().into_string();
        assert!(reverse.contains("Created issue"));
        assert!(reverse.contains(&format!("/issues/{issue}")));
        assert!(matches!(
            issue_panel(&db, Uuid::new_v4(), issue).await,
            Err(AppError::NotFound)
        ));

        let peer: Uuid = sqlx::query_scalar("INSERT INTO notes(owner_id,project_id,path,title,body) VALUES($1,$2,'b.md','Plan','# Milestones') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        sqlx::query("INSERT INTO note_links(owner_id,project_id,source_id,ordinal,raw,target,fragment,label,kind,dest_note,dest_issue) VALUES ($1,$2,$3,0,'b#Milestones','b','Milestones','Plan','wiki',$4,NULL), ($1,$2,$4,0,'a.md','a.md',NULL,'Design','markdown',$3,NULL), ($1,$2,$4,1,'LU-1','LU-1',NULL,'One','wiki',NULL,$5)")
            .bind(owner).bind(project).bind(note).bind(peer).bind(issue).execute(&db).await.unwrap();
        let note_edges = note_panel(&db, owner, note).await.unwrap().into_string();
        assert!(note_edges.contains("Wiki link to"));
        assert!(note_edges.contains("Markdown backlink from"));
        assert!(note_edges.contains(&format!("/notes/{peer}#heading-milestones")));

        let original = format!("/notes/{}", Uuid::new_v4());
        sqlx::query("INSERT INTO note_links(owner_id,project_id,source_id,ordinal,raw,target,fragment,label,kind,dest_note) VALUES ($1,$2,$3,1,'source URL',$5,'block-choice','Choice','markdown',$4), ($1,$2,$3,2,'vault path','b.md','block-choice','Vault choice','markdown',$4)")
            .bind(owner).bind(project).bind(note).bind(peer).bind(original).execute(&db).await.unwrap();
        let remapped = note_panel(&db, owner, note).await.unwrap().into_string();
        assert!(remapped.contains(&format!("/notes/{peer}#block-choice")));
        assert!(remapped.contains(&format!("/notes/{peer}#heading-block-choice")));

        let draft: Uuid = sqlx::query_scalar("INSERT INTO releases(owner_id,project_id,name) VALUES($1,$2,'Next release') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        sqlx::query("INSERT INTO release_members(owner_id,project_id,release_id,issue_id) VALUES($1,$2,$3,$4)")
            .bind(owner).bind(project).bind(draft).bind(issue).execute(&db).await.unwrap();
        let shipped: Uuid = sqlx::query_scalar("INSERT INTO releases(owner_id,project_id,name) VALUES($1,$2,'Shipped release') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        sqlx::query("INSERT INTO release_entries(owner_id,project_id,release_id,issue_id,number,title,issue_revision) VALUES($1,$2,$3,$4,1,'One',1)")
            .bind(owner).bind(project).bind(shipped).bind(issue).execute(&db).await.unwrap();
        sqlx::query("UPDATE releases SET published_at=now(), published_on=CURRENT_DATE,publish_command=gen_random_uuid() WHERE id=$1 AND owner_id=$2")
            .bind(shipped).bind(owner).execute(&db).await.unwrap();
        let provenance = neighborhood(
            &db,
            owner,
            &focus,
            project,
            GraphView::Standalone { page: 2, nodes: 9 },
        )
        .await
        .unwrap()
        .into_string();
        for relation in ["In release scope", "Shipped in", "Wiki backlink from"] {
            assert!(provenance.contains(relation));
        }
        for release_id in [draft, shipped] {
            assert!(provenance.contains(&format!("/releases/{release_id}")));
        }

        sqlx::query("UPDATE notes SET body='Revised decision.',revision=revision+1 WHERE id=$1 AND owner_id=$2")
            .bind(note).bind(owner).execute(&db).await.unwrap();
        let changed = issue_panel(&db, owner, issue).await.unwrap().into_string();
        assert!(changed.contains("paragraph has changed"));
        assert!(changed.contains("Ship one change."));
        assert!(!changed.contains("#block-ship-plan"));

        sqlx::query("DELETE FROM users WHERE id=$1")
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn graph_pages_preserve_nodes_and_every_section_link_at_both_limits(db: PgPool) {
        fn heading_links(part: &str) -> Vec<String> {
            part.split("href=\"")
                .skip(1)
                .filter_map(|s| s.split_once('"').map(|(url, _)| url))
                .filter(|url| url.starts_with("/notes/") && url.contains("#heading-"))
                .map(str::to_owned)
                .collect()
        }

        fn check_page(html: &str, node_count: usize, edge_count: usize) -> Vec<String> {
            let (diagram, list) = html.split_once("</svg>").expect("local SVG graph");
            assert_eq!(
                diagram.matches("data-graph-node=\"true\"").count(),
                node_count
            );
            assert_eq!(
                diagram.matches("data-relationship-edge=\"true\"").count(),
                edge_count
            );
            let links = heading_links(diagram);
            assert_eq!(links.len(), edge_count);
            assert_eq!(
                links,
                heading_links(list),
                "the diagram and ordered list must open the same sections in the same order"
            );
            links
        }
        let owner: Uuid = sqlx::query_scalar("INSERT INTO users(email) VALUES ($1) RETURNING id")
            .bind(format!("graph-scale-{}@example.invalid", Uuid::new_v4()))
            .fetch_one(&db)
            .await
            .unwrap();
        let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects(owner_id,key,name) VALUES ($1,'SCALE','Graph scale') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&db)
    .await
    .unwrap();
        let center: Uuid = sqlx::query_scalar("INSERT INTO notes(owner_id,project_id,path,title) VALUES ($1,$2,'center.md','Center') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        let peers: Vec<Uuid> = sqlx::query_scalar(
            "WITH created AS (
                INSERT INTO notes(owner_id,project_id,path,title,body)
                SELECT $1,$2,lpad(n::text,3,'0') || '.md','Peer ' || lpad(n::text,3,'0'),
                       E'# Part 1\n\n# Part 2\n\n# Part 3'
                FROM generate_series(1,120) n RETURNING id,path
             ) SELECT id FROM created ORDER BY path",
        )
        .bind(owner)
        .bind(project)
        .fetch_all(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO note_links(owner_id,project_id,source_id,ordinal,kind,target,raw,label,fragment,dest_note)
             SELECT $1,$2,$3,(peer_no * 3 + part)::int,'wiki',n.path,
                    n.path || '#Part ' || part,'Part ' || part,'Part ' || part,n.id
             FROM unnest($4::uuid[]) WITH ORDINALITY AS targets(id,peer_no)
             JOIN notes n ON n.id=targets.id
             CROSS JOIN LATERAL generate_series(1,CASE WHEN peer_no <= 52 THEN 3 ELSE 2 END) part",
        ).bind(owner).bind(project).bind(center).bind(&peers).execute(&db).await.unwrap();

        let (focus, project) = load_focus(&db, owner, Entity::Note, center).await.unwrap();
        let page = |nodes, page| {
            neighborhood(
                &db,
                owner,
                &focus,
                project,
                GraphView::Standalone { nodes, page },
            )
        };
        let first = page(100, 1).await.unwrap().into_string();
        let last = page(100, 2).await.unwrap().into_string();
        let small = page(25, 1).await.unwrap().into_string();
        let compact = note_panel(&db, owner, center).await.unwrap().into_string();

        // A single note can link hundreds of sections on the same neighbor.
        // The continuation must include its 251st section exactly once.
        sqlx::query("DELETE FROM note_links WHERE owner_id=$1 AND source_id=$2")
            .bind(owner)
            .bind(center)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO note_links(owner_id,project_id,source_id,ordinal,kind,target,raw,label,fragment,dest_note)
             SELECT $1,$2,$3,n,'wiki','001.md','001.md#Section ' || lpad(n::text,3,'0'),
                    'Read','Section ' || lpad(n::text,3,'0'),$4
             FROM generate_series(1,251) n",
        ).bind(owner).bind(project).bind(center).bind(peers[0]).execute(&db).await.unwrap();
        let dense_first = page(25, 1).await.unwrap().into_string();
        let dense_last = page(25, 2).await.unwrap().into_string();
        sqlx::query("DELETE FROM users WHERE id=$1")
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();

        let expected: Vec<String> = peers
            .iter()
            .enumerate()
            .flat_map(|(i, id)| {
                let count = if i < 52 { 3 } else { 2 };
                (1..=count).map(move |part| format!("/notes/{id}#heading-part-{part}"))
            })
            .collect();
        assert!(first.contains("100 of 121 nodes"));
        assert!(first.contains("42 relationships outside this page."));
        assert!(last.contains("251–292 of 292 relationships"));
        let mut all = check_page(&first, 100, 250);
        all.extend(check_page(&last, 22, 42));
        assert_eq!(all, expected);
        assert_eq!(check_page(&small, 25, 72), expected[..72]);
        assert!(first.contains("?nodes=100&amp;page=2"));
        assert!(last.contains("?nodes=100&amp;page=1"));
        assert!(small.contains("?nodes=25&amp;page=2"));
        let compact_graph = compact.split_once("</svg>").unwrap().0;
        assert_eq!(
            compact_graph
                .matches("data-relationship-edge=\"true\"")
                .count(),
            8
        );

        let mut dense = check_page(&dense_first, 2, 250);
        dense.extend(check_page(&dense_last, 2, 1));
        let expected_dense: Vec<String> = (1..=251)
            .map(|n| format!("/notes/{}#heading-section-{n:03}", peers[0]))
            .collect();
        assert_eq!(dense, expected_dense);
        assert!(dense_first.contains("?nodes=25&amp;page=2"));
        assert!(!dense_last.contains("?nodes=25&amp;page=3"));
    }

    #[test]
    fn graph_query_validates_node_limits_and_page_selection() {
        assert!(matches!(
            GraphQuery {
                nodes: None,
                page: None
            }
            .view(),
            Ok(GraphView::Standalone { nodes: 25, page: 1 })
        ));
        assert!(matches!(
            GraphQuery {
                nodes: Some(100),
                page: Some(2)
            }
            .view(),
            Ok(GraphView::Standalone {
                nodes: 100,
                page: 2
            })
        ));
        for invalid_nodes in [0, 1, 101] {
            assert!(matches!(
                GraphQuery {
                    nodes: Some(invalid_nodes),
                    page: None
                }
                .view(),
                Err(AppError::BadRequest(_))
            ));
        }
        assert!(matches!(
            GraphQuery {
                nodes: Some(25),
                page: Some(0)
            }
            .view(),
            Err(AppError::NotFound)
        ));
    }
}
