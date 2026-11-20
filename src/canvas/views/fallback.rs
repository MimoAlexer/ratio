use super::super::model::{Document, Node, NodeKind, SourceKind};
use maud::{Markup, html};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fmt::Write,
};

struct Curve<'a> {
    points: [[f64; 2]; 4],
    label: &'a str,
}

fn shape_label(node: &Node) -> Cow<'_, str> {
    if node.label.trim().is_empty() {
        format!("Untitled {}", node.kind.as_str()).into()
    } else {
        (&node.label[..]).into()
    }
}

fn curves(document: &Document) -> Vec<Curve<'_>> {
    let nodes: HashMap<_, _> = document.nodes.iter().map(|n| (n.id, n)).collect();
    let pairs: HashSet<_> = document.edges.iter().map(|e| (e.from, e.to)).collect();
    document
        .edges
        .iter()
        .filter_map(|edge| {
            let (from, to) = (nodes.get(&edge.from)?, nodes.get(&edge.to)?);
            let (ax, ay) = (from.x + from.w / 2.0, from.y + from.h / 2.0);
            let (bx, by) = (to.x + to.w / 2.0, to.y + to.h / 2.0);
            let points = if edge.from == edge.to {
                [
                    [from.x + from.w, ay],
                    [from.x + from.w + (0.8 * from.w).max(52.0), ay],
                    [ax, from.y - (0.9 * from.h).max(52.0)],
                    [ax, from.y],
                ]
            } else {
                let (dx, dy) = (bx - ax, by - ay);
                let (nx, ny) = if dx.abs() / (from.w + to.w) >= dy.abs() / (from.h + to.h) {
                    (if dx >= 0.0 { 1.0 } else { -1.0 }, 0.0)
                } else {
                    (0.0, if dy >= 0.0 { 1.0 } else { -1.0 })
                };
                let x1 = ax + nx * from.w / 2.0;
                let y1 = ay + ny * from.h / 2.0;
                let x2 = bx - nx * to.w / 2.0;
                let y2 = by - ny * to.h / 2.0;
                let pull = ((x2 - x1).hypot(y2 - y1) * 0.42).clamp(24.0, 160.0);
                let split = if pairs.contains(&(edge.to, edge.from)) {
                    18.0
                } else {
                    0.0
                };
                [
                    [x1, y1],
                    [x1 + nx * pull - ny * split, y1 + ny * pull + nx * split],
                    [x2 - nx * pull - ny * split, y2 - ny * pull + nx * split],
                    [x2, y2],
                ]
            };
            Some(Curve {
                points,
                label: &edge.label,
            })
        })
        .collect()
}

fn viewport(document: &Document, curves: &[Curve<'_>]) -> Option<String> {
    let node_corners = document
        .nodes
        .iter()
        .flat_map(|n| [[n.x, n.y], [n.x + n.w, n.y + n.h]]);
    let mut points = node_corners
        .chain(curves.iter().flat_map(|curve| curve.points))
        .chain(
            document
                .strokes
                .iter()
                .flat_map(|stroke| stroke.points.iter().copied()),
        );
    let [x, y] = points.next()?;
    let (mut left, mut right, mut top, mut bottom) = (x, x, y, y);
    for [x, y] in points {
        left = left.min(x);
        right = right.max(x);
        top = top.min(y);
        bottom = bottom.max(y);
    }
    let padding = 36.0;
    Some(format!(
        "{} {} {} {}",
        left - padding,
        top - padding,
        right - left + 2.0 * padding,
        bottom - top + 2.0 * padding,
    ))
}

fn points_attr(points: &[[f64; 2]]) -> String {
    let mut out = String::new();
    for [x, y] in points {
        let _ = write!(&mut out, "{x},{y} ");
    }
    out
}

fn node_graphic(node: &Node) -> Markup {
    let (w, h) = (node.w, node.h);
    let label = shape_label(node);
    let (x, y) = (w / 2.0, h / 2.0);
    html! {
        g.canvas-reading-node data-kind=(node.kind.as_str()) transform=(format!("translate({} {})", node.x, node.y)) {
            title { (&*label) }
            @match node.kind {
                NodeKind::Decision => {
                    polygon points=(points_attr(&[[x,0.0], [w,y], [x,h], [0.0,y]])) {}
                }
                NodeKind::Database => {
                    path d=(format!("M0 8 Q0 0 {x} 0 T{w} 8 V{} Q{w} {h} {x} {h} T0 {} Z M0 8 Q0 16 {x} 16 T{w} 8",h-8.0,h-8.0)) {}
                }
                NodeKind::Note => {
                    path d=(format!("M0 0 H{} L{w} 14 V{h} H0 Z M{} 0 V14 H{w}", w-14.0, w-14.0)) {}
                }
                _ => { rect width=(w) height=(h) rx="8" {} }
            }
            @let inset = if node.kind == NodeKind::Decision { w * 0.22 } else { 12.0 };
            foreignObject x=(inset) y="0" width=(w - inset * 2.0) height=(h) {
                div.canvas-reading-label xmlns="http://www.w3.org/1999/xhtml" {
                    span { (&*label) }
                }
            }
        }
    }
}

fn connector(curve: &Curve<'_>) -> Markup {
    let [[x1, y1], [cx1, cy1], [cx2, cy2], [x2, y2]] = curve.points;
    let path = format!("M{x1} {y1} C{cx1} {cy1} {cx2} {cy2} {x2} {y2}");
    let middle_x = (x1 + 3.0 * cx1 + 3.0 * cx2 + x2) / 8.0;
    let middle_y = (y1 + 3.0 * cy1 + 3.0 * cy2 + y2) / 8.0 - 7.0;
    html! {
        path.canvas-reading-wire d=(path) marker-end="url(#canvas-reading-arrow)" {}
        @if !curve.label.is_empty() {
            text.canvas-reading-edge-label x=(middle_x) y=(middle_y) text-anchor="middle" {
                title { (curve.label) }
                @if curve.label.chars().count() > 35 { (curve.label.chars().take(34).collect::<String>()) "…" }
                @else { (curve.label) }
            }
        }
    }
}

pub(super) fn graphic(document: &Document) -> Markup {
    let connections = curves(document);
    let Some(view) = viewport(document, &connections) else {
        return html! {};
    };
    html! {
        svg.canvas-reading-map viewBox=(view) role="img" aria-label="Saved diagram" {
            desc { "The diagram outline lists named shapes, their connections and linked records." }
            defs {
                marker id="canvas-reading-arrow" viewBox="0 0 10 10" refX="9" refY="5"
                    markerWidth="7" markerHeight="7" orient="auto-start-reverse" {
                    path d="M0 1 L9 5 L0 9" {}
                }
            }
            @for curve in &connections { (connector(curve)) }
            @for stroke in &document.strokes {
                @if let [[x,y]] = stroke.points.as_slice() {
                    circle.canvas-reading-dot cx=(x) cy=(y) r="1.1" {}
                } @else {
                    polyline.canvas-reading-stroke points=(points_attr(&stroke.points)) {}
                }
            }
            @for node in &document.nodes { (node_graphic(node)) }
        }
    }
}

pub(super) fn outline(document: &Document) -> Markup {
    if document.nodes.is_empty() && document.strokes.is_empty() {
        return html! { p.canvas-reading-empty { "This board is empty." } };
    }
    let nodes: HashMap<_, _> = document.nodes.iter().map(|node| (node.id, node)).collect();
    html! {
        details.canvas-reading-outline {
            summary { "Diagram outline" }
            @if !document.nodes.is_empty() {
                h2 { "Shapes" }
                ul {
                    @for node in &document.nodes {
                        li {
                            strong { (&*shape_label(node)) }
                            small { (node.kind.as_str()) }
                            @if !node.body.is_empty() { p { (&node.body) } }
                            @if let Some(source) = &node.source {
                                @if source.kind == SourceKind::Issue {
                                    a href=(format!("/issues/{}", source.id)) { "Open linked issue" }
                                } @else {
                                    a href=(format!("/notes/{}", source.id)) { "Open linked note" }
                                }
                            }
                        }
                    }
                }
            }
            @if !document.edges.is_empty() {
                h2 { "Connections" }
                ul {
                    @for edge in &document.edges {
                        @if let (Some(from),Some(to)) = (nodes.get(&edge.from), nodes.get(&edge.to)) {
                            li {
                                (&*shape_label(from)) span aria-hidden="true" { " → " } span.sr-only { " to " } (&*shape_label(to))
                                @if !edge.label.is_empty() { small { (&edge.label) } }
                            }
                        }
                    }
                }
            }
            @if !document.strokes.is_empty() {
                p { (document.strokes.len())
                    @if document.strokes.len() == 1 { " freehand stroke" } @else { " freehand strokes" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::model::{Edge, Source, Stroke};
    use uuid::Uuid;

    #[test]
    fn script_free_board_preserves_escaped_records_loops_and_pencil_taps() {
        let mut doc = Document::from_template("decision").unwrap();
        let node = &mut doc.nodes[0];
        node.label = "<button>Choose</button>".into();
        node.body = "Remember & compare".into();
        let source = Uuid::new_v4();
        node.source = Some(Source {
            kind: SourceKind::Issue,
            id: source,
        });
        let first = node.id;
        doc.edges.push(Edge {
            id: Uuid::new_v4(),
            from: first,
            to: first,
            label: "Retry".into(),
        });
        doc.strokes.push(Stroke {
            id: Uuid::new_v4(),
            points: vec![[42.0, 70.0]],
        });
        let picture = graphic(&doc).into_string();
        let text = outline(&doc).into_string();

        assert!(picture.contains("canvas-reading-dot") && picture.contains("Retry"));
        assert!(picture.contains("&lt;button&gt;Choose&lt;/button&gt;"));
        assert!(text.contains("Remember &amp; compare"));
        assert!(text.contains(&format!("href=\"/issues/{source}\"")));
        assert!(!picture.contains("<button>") && !text.contains("<button>"));
    }
}
