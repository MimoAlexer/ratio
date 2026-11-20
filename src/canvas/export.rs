//! A self-contained SVG of the saved map. The same attachment is used by web
//! browsers and by the desktop host's restricted, same-origin Save As flow.
use super::model::{Document, Node, NodeKind, Stroke};
use crate::{AppResult, AppState, auth::Auth};
use axum::{
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write,
};
use uuid::Uuid;

pub(super) async fn download(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    let board = super::load_board(&state.db, auth.user_id, id).await?;
    board.document.validate()?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/svg+xml".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"canvas-{id}.svg\""),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_owned()),
        ],
        render(&board.title, &board.document).into_string(),
    )
        .into_response())
}

type Point = [f64; 2];

#[derive(Default)]
struct Bounds(Option<(Point, Point)>);

impl Bounds {
    fn include(&mut self, p: Point) {
        let (low, high) = self.0.get_or_insert((p, p));
        for axis in 0..2 {
            low[axis] = low[axis].min(p[axis]);
            high[axis] = high[axis].max(p[axis]);
        }
    }

    fn view_box(&self) -> [f64; 4] {
        match self.0 {
            Some(([x, y], [r, b])) => [x - 32.0, y - 32.0, r - x + 64.0, b - y + 64.0],
            None => [0.0, 0.0, 640.0, 360.0],
        }
    }
}

fn connector(a: &Node, b: &Node, reverse: bool) -> [Point; 4] {
    let (ax, ay) = (a.x + a.w / 2.0, a.y + a.h / 2.0);
    let (bx, by) = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    if a.id == b.id {
        return [
            [a.x + a.w, ay],
            [a.x + a.w + (a.w * 0.8).max(52.0), ay],
            [ax, a.y - (a.h * 0.9).max(52.0)],
            [ax, a.y],
        ];
    }
    let dx = bx - ax;
    let dy = by - ay;
    let horizontal = dx.abs() / (a.w + b.w) >= dy.abs() / (a.h + b.h);
    let (vx, vy) = if horizontal {
        (if dx >= 0.0 { 1.0 } else { -1.0 }, 0.0)
    } else {
        (0.0, if dy >= 0.0 { 1.0 } else { -1.0 })
    };
    let start = [ax + vx * a.w / 2.0, ay + vy * a.h / 2.0];
    let end = [bx - vx * b.w / 2.0, by - vy * b.h / 2.0];
    let bend = (0.42 * (end[0] - start[0]).hypot(end[1] - start[1])).clamp(24.0, 160.0);
    let lane = if reverse { 18.0 } else { 0.0 };
    [
        start,
        [
            start[0] + vx * bend - vy * lane,
            start[1] + vy * bend + vx * lane,
        ],
        [
            end[0] - vx * bend - vy * lane,
            end[1] - vy * bend + vx * lane,
        ],
        end,
    ]
}

fn render_edge(points: [Point; 4], label: &str, bounds: &mut Bounds) -> Markup {
    for p in points {
        bounds.include(p);
    }
    let [a, b, c, d] = points;
    let path = format!(
        "M {} {} C {} {}, {} {}, {} {}",
        a[0], a[1], b[0], b[1], c[0], c[1], d[0], d[1]
    );
    let x = (a[0] + 3.0 * b[0] + 3.0 * c[0] + d[0]) / 8.0;
    let y = (a[1] + 3.0 * b[1] + 3.0 * c[1] + d[1]) / 8.0 - 7.0;
    let shown_label = abbreviated(label, 23);
    let half_label_width = shown_label.chars().count() as f64 * 6.0;
    bounds.include([x - half_label_width, y - 12.0]);
    bounds.include([x + half_label_width, y + 3.0]);
    html! {
        g class="connector" {
            title { (label) }
            path d=(path) fill="none" stroke="#949494" stroke-width="1.5" marker-end="url(#arrow)" {}
            @if !label.is_empty() {
                text x=(x) y=(y) text-anchor="middle" fill="#d6d6d6" stroke="#171717" stroke-width="4" stroke-linejoin="round" paint-order="stroke" font-size="12" { (shown_label) }
            }
        }
    }
}

fn shape(node: &Node) -> Markup {
    let (w, h) = (node.w, node.h);
    let middle = w / 2.0;
    match node.kind {
        NodeKind::Decision => {
            html! { polygon points=(format!("{middle},0 {w},{} {middle},{h} 0,{}",h/2.0,h/2.0)) {} }
        }
        NodeKind::Database => {
            let cap = (h * 0.16).min(10.0);
            let bottom = h - cap;
            html! {
                path d=(format!("M 0 {cap} A {middle} {cap} 0 0 1 {w} {cap} V {bottom} A {middle} {cap} 0 0 1 0 {bottom} Z")) {}
                path d=(format!("M 0 {cap} A {middle} {cap} 0 0 0 {w} {cap}")) fill="none" {}
            }
        }
        NodeKind::Note => {
            let fold = 14.0_f64.min(w / 6.0).min(h / 5.0);
            let crease = w - fold;
            html! {
                path d=(format!("M 0 0 H {crease} L {w} {fold} V {h} H 0 Z")) {}
                path d=(format!("M {crease} 0 V {fold} H {w}")) fill="none" {}
            }
        }
        NodeKind::Process | NodeKind::Service => html! { rect width=(w) height=(h) rx="8" {} },
    }
}

fn abbreviated(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let mut line: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        line.pop();
        line.push('…');
    }
    line
}

fn render_node(node: &Node) -> Markup {
    let (w, h) = (node.w, node.h);
    let decision = node.kind == NodeKind::Decision;
    let icon = match node.kind {
        NodeKind::Process => Some("M2 4h14v10H2zM5 8h8"),
        NodeKind::Service => Some("M2 3h14v5H2zM2 10h14v5H2zM5 5.5h1m-1 7h1"),
        NodeKind::Database => Some("M2 5c0-3 14-3 14 0s-14 3-14 0Zm0 0v8c0 3 14 3 14 0V5"),
        NodeKind::Note => Some("M4 2h8l3 3v11H4zM11 2v4h4M7 10h5"),
        NodeKind::Decision => None,
    }
    .filter(|_| w >= 150.0);
    let x = if decision {
        w / 2.0
    } else if icon.is_some() {
        40.0
    } else {
        14.0
    };
    let cap = if node.kind == NodeKind::Database {
        (h * 0.16).min(10.0)
    } else {
        0.0
    };
    let usable = if decision {
        w * 0.67
    } else {
        w - x
            - if node.kind == NodeKind::Note {
                20.0
            } else {
                12.0
            }
    };
    let body_width = if decision {
        (w * (0.89 - 34.0 / h)).max(12.0)
    } else {
        w - x - if node.source.is_some() { 24.0 } else { 12.0 }
    };
    let show_body = !node.body.trim().is_empty()
        && h - cap * 2.0 >= if decision { 70.0 } else { 48.0 }
        && w >= 96.0;
    let title_y = h / 2.0 + if show_body { -2.0 } else { 5.0 } + cap * 0.32;
    let label = if node.label.trim().is_empty() {
        node.kind.as_str()
    } else {
        node.label.trim()
    };
    let clip_id = format!("text-{}", node.id);
    html! {
        g class=(node.kind.as_str()) transform=(format!("translate({} {})",node.x,node.y)) {
            title {(label)}
            desc { (&node.body) @if let Some(source)=node.source { " · Linked " (source.kind.as_str()) } }
            g fill="#202020" stroke="#636363" stroke-width="1.15" stroke-linejoin="round" {(shape(node))}
            @if let Some(path)=icon {
                path d=(path) transform=(format!("translate(14 {})", title_y-12.0)) fill="none" stroke="#c0c0c0" stroke-width="1.35" stroke-linecap="round" stroke-linejoin="round" {}
            }
            @if node.source.is_some() {
                @let pin_x = if decision { w*0.5 } else { w-16.0 };
                @let pin_y = match node.kind { NodeKind::Database=>h-15.0, NodeKind::Decision=>h-18.0, _=>h-11.0 };
                path d="M1 8l2-2m0 0 3-3m-3 3-1-1a2 2 0 0 1 0-3l1-1a2 2 0 0 1 3 0m0 2 1 1a2 2 0 0 1 0 3L6 9a2 2 0 0 1-3 0"
                    transform=(format!("translate({} {})",pin_x-3.0,pin_y-4.0)) fill="none" stroke="#b4b4b4" stroke-width="1.2" {}
            }
            clipPath id=(&clip_id) { rect x="4" y="4" width=(w-8.0) height=(h-8.0) {} }
            g clip-path=(format!("url(#{clip_id})")) text-anchor=(if decision {"middle"} else {"start"}) {
                text x=(x) y=(title_y) fill="#f2f2f2" font-size="14" font-weight="600" {
                    (abbreviated(label,(usable/7.5).max(2.0) as usize))
                }
                @if show_body {
                    text x=(x) y=(title_y+18.0) fill="#b8b8b8" font-size="12" {
                        (abbreviated(&node.body,(body_width/6.5).max(2.0) as usize))
                    }
                }
            }
        }
    }
}

fn render_stroke(stroke: &Stroke, bounds: &mut Bounds) -> Markup {
    if let [[x, y]] = stroke.points.as_slice() {
        bounds.include([*x, *y]);
        return html! { circle class="ink" cx=(x) cy=(y) r="1.1" fill="#c6c6c6" {} };
    }
    let mut path = String::new();
    for (i, &point) in stroke.points.iter().enumerate() {
        let [x, y] = point;
        bounds.include(point);
        write!(path, "{} {x} {y} ", if i == 0 { "M" } else { "L" }).unwrap();
    }
    html! { path class="ink" d=(path) stroke="#c6c6c6" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" fill="none" {} }
}

fn render(title: &str, doc: &Document) -> Markup {
    let mut bounds = Bounds::default();
    let nodes: HashMap<_, _> = doc.nodes.iter().map(|n| (n.id, n)).collect();
    let directions: HashSet<_> = doc.edges.iter().map(|e| (e.from, e.to)).collect();
    for node in &doc.nodes {
        bounds.include([node.x, node.y]);
        bounds.include([node.x + node.w, node.y + node.h]);
    }
    let edges = html! {
        @for edge in &doc.edges {
            @if let (Some(a), Some(b))=(nodes.get(&edge.from),nodes.get(&edge.to)) {
                (render_edge(connector(a,b,directions.contains(&(edge.to,edge.from))),&edge.label,&mut bounds))
            }
        }
    };
    let strokes = html! { @for stroke in &doc.strokes { (render_stroke(stroke, &mut bounds)) } };
    let [x, y, w, h] = bounds.view_box();
    // Keep the natural image size practical while preserving every world-space coordinate.
    let scale = (1920.0 / w.max(h)).min(1.0);
    let title: String = title.chars().filter(|c|matches!(c,'\t'|'\n'|'\r'|'\u{20}'..='\u{D7FF}'|'\u{E000}'..='\u{FFFD}'|'\u{10000}'..='\u{10FFFF}')).collect();
    html! {
        svg xmlns="http://www.w3.org/2000/svg" viewBox=(format!("{x} {y} {w} {h}")) width=((w*scale).round().max(1.0)) height=((h*scale).round().max(1.0))
            role="img" aria-labelledby="board-title" font-family="system-ui, sans-serif" {
            title id="board-title" { (title) }
            defs {
                marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto" {
                    path d="M 1 1 L 9 5 L 1 9" fill="none" stroke="#999999" stroke-width="1.5" {}
                }
            }
            rect x=(x) y=(y) width=(w) height=(h) fill="#171717" {}
            (edges) (strokes)
            @for node in &doc.nodes { (render_node(node)) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_is_standalone_and_treats_user_labels_as_plain_text() {
        let mut doc = Document::from_template("architecture").unwrap();
        doc.nodes[0].label = "<script>&Ǣ".into();
        doc.nodes[0].body = r#"Recheck <details> with "β"."#.into();
        doc.edges[0].label = "confirm & release".into();
        let svg = render("Board <2026>", &doc).into_string();
        assert!(svg.contains("Board &lt;2026&gt;"));
        assert!(svg.contains("&lt;script&gt;&amp;Ǣ"));
        assert!(svg.contains("confirm &amp; release"));
        assert!(!svg.contains("<script"));
        assert!(!svg.contains("href="));
        assert!(!svg.contains("foreignObject"));
        assert_eq!(svg.matches("class=\"connector\"").count(), doc.edges.len());
    }

    #[test]
    fn blank_boards_and_isolated_strokes_have_a_nonempty_padded_viewbox() {
        let mut doc = Document::from_template("blank").unwrap();
        assert!(
            render("Board", &doc)
                .into_string()
                .contains("viewBox=\"0 0 640 360\"")
        );
        doc.strokes.push(Stroke {
            id: Uuid::new_v4(),
            points: vec![[-10.0, -20.0]],
        });
        let svg = render("Ink", &doc).into_string();
        assert!(svg.contains("viewBox=\"-42 -52 64 64\""));
        assert!(svg.contains("<circle class=\"ink\""));
    }
}
