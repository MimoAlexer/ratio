use crate::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

const MAX_NODES: usize = 250;
const MAX_EDGES: usize = 500;
const MAX_STROKES: usize = 200;
const MAX_STROKE_POINTS: usize = 2_048;
const MAX_TOTAL_POINTS: usize = 20_000;
const WORLD_LIMIT: f64 = 16_000.0;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    pub version: u8,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub strokes: Vec<Stroke>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Node {
    pub id: Uuid,
    pub kind: NodeKind,
    pub label: String,
    pub body: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NodeKind {
    Process,
    Decision,
    Note,
    Service,
    Database,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Decision => "decision",
            Self::Note => "note",
            Self::Service => "service",
            Self::Database => "database",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub kind: SourceKind,
    pub id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SourceKind {
    Issue,
    Note,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Note => "note",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Edge {
    pub id: Uuid,
    pub from: Uuid,
    pub to: Uuid,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stroke {
    pub id: Uuid,
    pub points: Vec<[f64; 2]>,
}

fn valid_coordinate(value: f64) -> bool {
    (-WORLD_LIMIT..=WORLD_LIMIT).contains(&value)
}

fn valid_text(text: &str, max_chars: usize) -> bool {
    text.chars().count() <= max_chars
        && text.chars().all(|c| {
            // The same text is stored in PostgreSQL JSONB and exported as SVG XML.
            matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
        })
}

impl Document {
    pub fn validate(&self) -> AppResult<()> {
        if self.version != 1 {
            return Err(AppError::BadRequest("Unsupported canvas format.".into()));
        }
        if self.nodes.len() > MAX_NODES
            || self.edges.len() > MAX_EDGES
            || self.strokes.len() > MAX_STROKES
        {
            return Err(AppError::BadRequest(
                "A board supports up to 250 shapes, 500 connectors and 200 strokes.".into(),
            ));
        }

        // Selections use an ID across all three kinds of canvas object.
        let mut ids = HashSet::new();
        let all_ids = self
            .nodes
            .iter()
            .map(|n| n.id)
            .chain(self.edges.iter().map(|e| e.id))
            .chain(self.strokes.iter().map(|s| s.id));
        for id in all_ids {
            if !ids.insert(id) {
                return Err(AppError::BadRequest(
                    "Each canvas object needs its own ID.".into(),
                ));
            }
        }

        let mut node_ids = HashSet::new();
        for node in &self.nodes {
            node_ids.insert(node.id);
            if !valid_coordinate(node.x)
                || !valid_coordinate(node.y)
                || !(64.0..=720.0).contains(&node.w)
                || !(40.0..=480.0).contains(&node.h)
            {
                return Err(AppError::BadRequest(
                    "Shape size or position is outside the board limits.".into(),
                ));
            }
            if !valid_text(&node.label, 160) || !valid_text(&node.body, 2_000) {
                return Err(AppError::BadRequest(
                    "Use up to 160 characters for a shape title and 2,000 for its text.".into(),
                ));
            }
        }
        for edge in &self.edges {
            if !node_ids.contains(&edge.from) || !node_ids.contains(&edge.to) {
                return Err(AppError::BadRequest(
                    "Both ends of a connector must be shapes on this board.".into(),
                ));
            }
            if !valid_text(&edge.label, 160) {
                return Err(AppError::BadRequest(
                    "Use up to 160 characters for a connector label.".into(),
                ));
            }
        }

        let mut total_points = 0;
        for stroke in &self.strokes {
            if stroke.points.is_empty() || stroke.points.len() > MAX_STROKE_POINTS {
                return Err(AppError::BadRequest(
                    "A pencil stroke needs 1 to 2,048 points.".into(),
                ));
            }
            total_points += stroke.points.len();
            if total_points > MAX_TOTAL_POINTS {
                return Err(AppError::BadRequest(
                    "A board supports up to 20,000 pencil points.".into(),
                ));
            }
            if stroke
                .points
                .iter()
                .flatten()
                .any(|&value| !valid_coordinate(value))
            {
                return Err(AppError::BadRequest(
                    "A pencil stroke extends beyond the board limits.".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn from_template(key: &str) -> AppResult<Self> {
        use NodeKind::*;
        type NodeSeed = (NodeKind, &'static str, &'static str, i16, i16);
        type EdgeSeed = (usize, usize, &'static str);
        let (nodes, edges): (&[NodeSeed], &[EdgeSeed]) = match key {
            "blank" => (&[], &[]),
            "architecture" => (
                &[
                    (Process, "Client", "Where does a request start?", -384, -112),
                    (
                        Service,
                        "API",
                        "Handle requests and validate input.",
                        -96,
                        -112,
                    ),
                    (
                        Database,
                        "Database",
                        "Store application records.",
                        192,
                        -112,
                    ),
                    (
                        Service,
                        "Job queue",
                        "Buffer work that can happen later.",
                        -96,
                        80,
                    ),
                    (
                        Service,
                        "Worker",
                        "Run queued jobs and store the result.",
                        192,
                        80,
                    ),
                ],
                &[
                    (0, 1, "request"),
                    (1, 2, "read / write"),
                    (1, 3, "enqueue"),
                    (3, 4, "deliver"),
                    (4, 2, "persist"),
                ],
            ),
            "release" => (
                &[
                    (
                        Process,
                        "Cut a release",
                        "Choose the changes to include.",
                        -432,
                        -80,
                    ),
                    (
                        Process,
                        "Run checks",
                        "Exercise the release candidate.",
                        -176,
                        -80,
                    ),
                    (
                        Decision,
                        "Ready to ship?",
                        "Review failing checks and known risks.",
                        80,
                        -96,
                    ),
                    (
                        Process,
                        "Publish",
                        "Tag the release and publish the changelog.",
                        384,
                        -80,
                    ),
                    (
                        Process,
                        "Fix and retest",
                        "Link the blocking issue here.",
                        -48,
                        128,
                    ),
                ],
                &[
                    (0, 1, ""),
                    (1, 2, ""),
                    (2, 3, "yes"),
                    (2, 4, "no"),
                    (4, 1, "retry"),
                ],
            ),
            "decision" => (
                &[
                    (
                        Decision,
                        "Where should state live?",
                        "Compare storage options for this project.",
                        -384,
                        -16,
                    ),
                    (
                        Process,
                        "On device",
                        "Consider offline access and local recovery.",
                        -80,
                        -112,
                    ),
                    (
                        Service,
                        "Shared service",
                        "Consider sync, privacy and availability.",
                        -80,
                        80,
                    ),
                    (
                        Note,
                        "Decision record",
                        "Record the choice, its trade-offs and when to revisit it.",
                        224,
                        0,
                    ),
                ],
                &[
                    (0, 1, "local"),
                    (0, 2, "remote"),
                    (1, 3, "trade-offs"),
                    (2, 3, "trade-offs"),
                ],
            ),
            _ => {
                return Err(AppError::BadRequest(
                    "Choose an available canvas template.".into(),
                ));
            }
        };
        let nodes: Vec<Node> = nodes
            .iter()
            .map(|&(kind, label, body, x, y)| Node {
                id: Uuid::new_v4(),
                kind,
                label: label.into(),
                body: body.into(),
                x: f64::from(x),
                y: f64::from(y),
                w: if kind == Decision { 224.0 } else { 192.0 },
                h: if kind == Decision { 112.0 } else { 80.0 },
                source: None,
            })
            .collect();
        let edges = edges
            .iter()
            .map(|&(from, to, label)| Edge {
                id: Uuid::new_v4(),
                from: nodes[from].id,
                to: nodes[to].id,
                label: label.into(),
            })
            .collect();
        Ok(Self {
            version: 1,
            nodes,
            edges,
            strokes: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn id() -> Uuid {
        Uuid::new_v4()
    }

    fn node(id: Uuid, label: &str) -> Value {
        json!({
            "id":id, "kind":"process", "label":label, "body":"",
            "x":0, "y":0, "w":160, "h":80
        })
    }

    fn edge(from: Uuid, to: Uuid) -> Value {
        json!({"id":id(), "from":from, "to":to, "label":"then"})
    }

    fn stroke(points: usize) -> Value {
        let points: Vec<_> = (0..points).map(|i| [i, 2]).collect();
        json!({"id":id(), "points":points})
    }

    fn document(nodes: Vec<Value>, edges: Vec<Value>, strokes: Vec<Value>) -> Value {
        json!({"version":1, "nodes":nodes, "edges":edges, "strokes":strokes})
    }

    fn accepts(raw: Value) -> bool {
        serde_json::from_value::<Document>(raw).is_ok_and(|doc| doc.validate().is_ok())
    }

    #[test]
    fn templates_have_valid_content_and_blank_starts_empty() {
        let blank = Document::from_template("blank").unwrap();
        assert!(blank.validate().is_ok());
        assert!(blank.nodes.is_empty() && blank.edges.is_empty() && blank.strokes.is_empty());
        let mut titles = Vec::new();
        for key in ["architecture", "release", "decision"] {
            let map = Document::from_template(key).unwrap();
            assert!(map.validate().is_ok(), "{key}");
            assert!(map.nodes.len() >= 2 && !map.edges.is_empty(), "{key}");
            let labels: Vec<_> = map.nodes.iter().map(|n| n.label.clone()).collect();
            assert!(!titles.contains(&labels), "{key}");
            titles.push(labels);
        }
        assert!(Document::from_template("unknown").is_err());
    }

    #[test]
    fn saved_cycle_and_source_survive_a_wire_round_trip() {
        let (a, b, source) = (id(), id(), id());
        let mut linked = node(a, "Review");
        linked["source"] = json!({"kind":"issue", "id":source});
        linked["body"] = json!("Check the issue and\nrecord the outcome.");
        let raw = document(
            vec![linked, node(b, "Revise")],
            vec![edge(a, b), edge(b, a)],
            vec![stroke(3)],
        );
        let saved: Document = serde_json::from_value(raw).unwrap();
        saved.validate().unwrap();
        let encoded = serde_json::to_value(&saved).unwrap();
        assert_eq!(encoded["nodes"][0]["source"]["id"], json!(source));
        let decoded: Document = serde_json::from_value(encoded.clone()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
    }

    #[test]
    fn missing_endpoints_and_global_id_collisions_are_rejected() {
        let (a, b) = (id(), id());
        let nodes = vec![node(a, "Start"), node(b, "End")];
        assert!(!accepts(document(
            nodes.clone(),
            vec![edge(a, id())],
            vec![]
        )));
        let connection = edge(a, b);
        let mut ink = stroke(2);
        ink["id"] = connection["id"].clone();
        assert!(!accepts(document(
            nodes.clone(),
            vec![connection],
            vec![ink]
        )));
        assert!(!accepts(document(
            vec![node(a, "A"), node(a, "B")],
            vec![],
            vec![]
        )));
    }

    #[test]
    fn document_version_text_and_geometry_are_checked_before_saving() {
        let shapes = vec![node(id(), "Design")];
        let good = document(shapes, vec![], vec![]);
        for (field, bad) in [
            ("x", json!(-16000.5)),
            ("y", json!(16000.5)),
            ("w", json!(63)),
            ("w", json!(721)),
            ("h", json!(39)),
            ("h", json!(481)),
            ("kind", json!("unknown")),
            ("label", json!("x".repeat(161))),
            ("body", json!("x".repeat(2001))),
            ("body", json!("broken\0text")),
            ("x", Value::Null),
        ] {
            let mut invalid = good.clone();
            invalid["nodes"][0][field] = bad;
            assert!(!accepts(invalid), "{field}");
        }
        let mut unicode = good.clone();
        unicode["nodes"][0]["label"] = json!("数".repeat(160));
        unicode["nodes"][0]["body"] = json!("Decide <later> & review\nagain.");
        assert!(accepts(unicode));
        let mut later = good;
        later["version"] = json!(2);
        assert!(!accepts(later));
    }

    #[test]
    fn element_limits_are_inclusive_at_the_boundary() {
        let nodes: Vec<_> = (0..250).map(|_| node(id(), "Step")).collect();
        assert!(accepts(document(nodes.clone(), vec![], vec![])));
        let mut too_many = nodes;
        too_many.push(node(id(), "Extra"));
        assert!(!accepts(document(too_many, vec![], vec![])));

        let ids: Vec<_> = (0..25).map(|_| id()).collect();
        let endpoints: Vec<_> = ids.iter().map(|&id| node(id, "Target")).collect();
        let mut links = Vec::new();
        for &from in &ids {
            for &to in &ids {
                if from != to {
                    links.push(edge(from, to));
                }
            }
        }
        assert!(accepts(document(
            endpoints.clone(),
            links[..500].to_vec(),
            vec![]
        )));
        assert!(!accepts(document(endpoints, links[..501].to_vec(), vec![])));
        let ink: Vec<_> = (0..201).map(|_| stroke(2)).collect();
        assert!(accepts(document(vec![], vec![], ink[..200].to_vec())));
        assert!(!accepts(document(vec![], vec![], ink)));
    }

    #[test]
    fn coordinates_in_memory_must_be_finite_before_serialization() {
        let mut map = Document::from_template("release").unwrap();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            map.nodes[0].x = bad;
            assert!(map.validate().is_err());
        }
    }

    #[test]
    fn freehand_has_per_stroke_and_whole_drawing_limits() {
        let legal: Vec<_> = (0..10).map(|_| stroke(2000)).collect();
        assert!(accepts(document(vec![], vec![], legal.clone())));
        let mut over = legal;
        over[0]["points"]
            .as_array_mut()
            .unwrap()
            .push(json!([0, 0]));
        assert!(!accepts(document(vec![], vec![], over)));
        assert!(accepts(document(vec![], vec![], vec![stroke(2048)])));
        assert!(!accepts(document(vec![], vec![], vec![stroke(2049)])));
        let mut bad_point = stroke(3);
        bad_point["points"][0] = json!([0, 16001]);
        assert!(!accepts(document(vec![], vec![], vec![bad_point])));
    }
}
