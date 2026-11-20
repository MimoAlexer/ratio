use maud::Markup;
use uuid::Uuid;

#[derive(Debug)]
pub(super) struct Document {
    pub links: Vec<Reference>,
    pub paragraphs: Vec<Paragraph>,
}

#[derive(Debug, Clone)]
pub(super) struct Reference {
    pub raw: String,
    pub target: String,
    pub fragment: Option<String>,
    pub label: String,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub(super) struct Paragraph {
    pub anchor: String,
    pub excerpt: String,
}

use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use pulldown_cmark::{
    Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd, html as markdown_html,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

fn parser(body: &str) -> impl Iterator<Item = (Event<'_>, Range<usize>)> {
    let options = Options::ENABLE_WIKILINKS
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS;
    Parser::new_ext(body, options).into_offset_iter()
}

fn local_reference(kind: LinkType, dest: &str) -> Option<Reference> {
    if matches!(kind, LinkType::Autolink | LinkType::Email) {
        return None;
    }
    let dest = dest.trim();
    let (target, fragment) = dest
        .split_once('#')
        .map_or((dest, None), |(p, f)| (p, Some(f)));
    let target = percent_decode_str(target).decode_utf8().ok()?.into_owned();
    let fragment = fragment
        .map(|v| percent_decode_str(v).decode_utf8().map(|v| v.into_owned()))
        .transpose()
        .ok()?
        .filter(|f| !f.trim().is_empty());
    let first = target.split('/').next().unwrap_or_default();
    if (target.is_empty() && fragment.is_none())
        || target.starts_with("//")
        || first.contains(':')
        || target.contains('\\')
        || target.chars().any(char::is_control)
    {
        return None;
    }
    Some(Reference {
        raw: dest.to_owned(),
        target,
        fragment,
        label: String::new(),
        kind: if matches!(kind, LinkType::WikiLink { .. }) {
            "wiki"
        } else {
            "markdown"
        }
        .to_owned(),
    })
}

fn external_url(kind: LinkType, dest: &str) -> Option<String> {
    if matches!(kind, LinkType::WikiLink { .. })
        || dest
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return None;
    }
    if kind == LinkType::Email {
        return Some(format!("mailto:{dest}"));
    }
    let lower = dest.to_ascii_lowercase();
    ["https://", "http://", "mailto:"]
        .into_iter()
        .any(|scheme| lower.starts_with(scheme) && lower.len() > scheme.len())
        .then(|| dest.to_owned())
}

fn native_note(target: &str) -> Option<Uuid> {
    Uuid::parse_str(target.strip_prefix("/notes/")?).ok()
}

/// A copied native URL already names the rendered DOM anchor. Vault links name
/// an authored heading or explicit block, whose anchor is derived at render time.
pub(super) fn link_fragment(target: &str, fragment: &str) -> String {
    if native_note(target).is_some()
        && ["heading-", "block-", "p-"]
            .iter()
            .any(|prefix| fragment.starts_with(prefix))
        && fragment
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
    {
        return fragment.to_owned();
    }
    fragment_id(fragment)
}

pub(super) fn fragment_id(value: &str) -> String {
    let value = value.trim();
    let (prefix, text) = value
        .strip_prefix('^')
        .map_or(("heading", value), |v| ("block", v));
    let mut slug = String::new();
    let mut separator = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || c == '_' {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(c);
            separator = false;
        } else {
            separator = true;
        }
    }
    if slug.is_empty() {
        slug = hex::encode(&Sha256::digest(text.as_bytes())[..8]);
    }
    format!("{prefix}-{slug}")
}

#[derive(Default)]
struct Anchors {
    used: HashSet<String>,
    next: HashMap<String, usize>,
}

impl Anchors {
    fn allocate(&mut self, base: String) -> String {
        if self.used.insert(base.clone()) {
            return base;
        }
        let n = self.next.entry(base.clone()).or_insert(2);
        loop {
            let id = format!("{base}-{n}");
            *n += 1;
            if self.used.insert(id.clone()) {
                return id;
            }
        }
    }
}

fn block_marker(text: &str) -> Option<&str> {
    let name = text.strip_prefix('^')?;
    (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '-')).then_some(text)
}

fn index_paragraphs(
    body: &str,
    ranges: Vec<Range<usize>>,
    anchors: &mut Anchors,
    by_offset: &mut HashMap<usize, String>,
) -> Vec<Paragraph> {
    let mut paragraphs = Vec::new();
    let mut ranges = ranges.into_iter().peekable();
    while let Some(range) = ranges.next() {
        let raw = body[range.clone()].trim();
        if let Some(marker) = block_marker(raw) {
            by_offset.insert(range.start, anchors.allocate(fragment_id(marker)));
            continue;
        }
        let (excerpt, mut explicit) = raw
            .rsplit_once(char::is_whitespace)
            .and_then(|(text, id)| Some((text.trim_end(), Some(block_marker(id)?))))
            .unwrap_or((raw, None));
        if explicit.is_none()
            && let Some(next) = ranges.peek()
            && body[range.end..next.start].trim().is_empty()
            && let Some(marker) = block_marker(body[next.clone()].trim())
        {
            explicit = Some(marker);
            ranges.next();
        }
        let base = explicit
            .map(fragment_id)
            .unwrap_or_else(|| format!("p-{}", hex::encode(&Sha256::digest(raw.as_bytes())[..12])));
        let anchor = anchors.allocate(base);
        by_offset.insert(range.start, anchor.clone());
        paragraphs.push(Paragraph {
            anchor,
            excerpt: excerpt.to_owned(),
        });
    }
    paragraphs
}

struct Analysis {
    doc: Document,
    anchors: HashMap<usize, String>,
    ordinals: HashMap<usize, usize>,
    has_h1: bool,
}

fn analyze(body: &str) -> Analysis {
    // CommonMark image alt text may itself contain square brackets. In the wiki
    // extension that inner text can be tokenized as a wikilink instead.
    let image_spans: Vec<_> = if body.contains("![") && body.contains("[[") {
        let mut outer_end = 0;
        Parser::new(body)
            .into_offset_iter()
            .filter_map(|(event, span)| {
                if matches!(event, Event::Start(Tag::Image { .. })) && span.start >= outer_end {
                    outer_end = span.end;
                    Some(span)
                } else {
                    None
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut doc = Document {
        links: Vec::new(),
        paragraphs: Vec::new(),
    };
    let mut paragraph_ranges = Vec::new();
    let mut anchors = Anchors::default();
    let mut by_offset = HashMap::new();
    let mut ordinals = HashMap::new();
    let mut heading: Option<(usize, String)> = None;
    let mut has_h1 = false;
    let mut reference: Option<usize> = None;
    let mut image_depth = 0;
    for (event, range) in parser(body) {
        match event {
            Event::Start(Tag::Image { .. }) => image_depth += 1,
            Event::End(TagEnd::Image) => image_depth -= 1,
            _ if image_depth > 0 => {}
            Event::Start(Tag::Paragraph) => paragraph_ranges.push(range),
            Event::Start(Tag::Heading { level, .. }) => {
                has_h1 |= level == HeadingLevel::H1;
                heading = Some((range.start, String::new()));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((start, label)) = heading.take() {
                    by_offset.insert(start, anchors.allocate(fragment_id(&label)));
                }
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => {
                let index = image_spans.partition_point(|image| image.end <= range.start);
                if image_spans
                    .get(index)
                    .is_some_and(|image| image.contains(&range.start))
                {
                    continue;
                }
                if let Some(link) = local_reference(link_type, &dest_url) {
                    let ordinal = doc.links.len();
                    ordinals.insert(range.start, ordinal);
                    doc.links.push(link);
                    reference = Some(ordinal);
                }
            }
            Event::End(TagEnd::Link) => {
                reference = None;
            }
            Event::Text(s) | Event::Code(s) => {
                if let Some((_, label)) = &mut heading {
                    label.push_str(&s);
                }
                if let Some(i) = reference {
                    doc.links[i].label.push_str(&s);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, label)) = &mut heading {
                    label.push(' ');
                }
                if let Some(i) = reference {
                    doc.links[i].label.push(' ');
                }
            }
            _ => {}
        }
    }
    doc.paragraphs = index_paragraphs(body, paragraph_ranges, &mut anchors, &mut by_offset);
    Analysis {
        doc,
        anchors: by_offset,
        ordinals,
        has_h1,
    }
}

impl Document {
    pub(super) fn parse(body: &str) -> Self {
        analyze(body).doc
    }
}

fn image_url(kind: LinkType, dest: &str, source_path: &str, project: Uuid) -> Option<String> {
    let path = percent_decode_str(dest).decode_utf8().ok()?;
    if path.starts_with("//") || path.contains(['?', '#', '\\']) {
        return None;
    }
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    if !matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "avif"
    ) {
        return None;
    }
    let folder = source_path
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent);
    let wiki_root =
        matches!(kind, LinkType::WikiLink { .. }) && path.contains('/') && !path.starts_with('.');
    let base = if path.starts_with('/') || wiki_root {
        ""
    } else {
        folder
    };
    let normalized = joined_note_path(base, path.strip_prefix('/').unwrap_or(&path))?;
    const SEGMENT: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    let encoded = normalized
        .split('/')
        .map(|s| utf8_percent_encode(s, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/");
    Some(format!("/transfer/media/{project}/{encoded}"))
}

/// Source HTML is always escaped. Links pass through an allowlist and image
/// alt text remains readable without loading resources from arbitrary hosts.
pub(super) fn render(
    body: &str,
    links: &[Option<String>],
    project: Uuid,
    source_path: &str,
) -> Markup {
    let analysis = analyze(body);
    // The note title is the page heading. Keep the source's heading hierarchy below it.
    let embedded_level = |level: HeadingLevel| {
        HeadingLevel::try_from(level as usize + usize::from(analysis.has_h1))
            .unwrap_or(HeadingLevel::H6)
    };
    let mut events = Vec::new();
    let mut image_depth = 0usize;
    let mut span_link = false;
    let mut local_image = false;
    let mut in_marker = false;
    for (event, range) in parser(body) {
        let safe = match event {
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                ..
            }) => {
                image_depth += 1;
                if image_depth == 1
                    && let Some(href) = image_url(link_type, &dest_url, source_path, project)
                {
                    local_image = true;
                    Event::Start(Tag::Image {
                        link_type: LinkType::Inline,
                        dest_url: href.into(),
                        title,
                        id: "".into(),
                    })
                } else {
                    continue;
                }
            }
            Event::End(TagEnd::Image) => {
                image_depth -= 1;
                if local_image && image_depth == 0 {
                    local_image = false;
                    Event::End(TagEnd::Image)
                } else {
                    continue;
                }
            }
            Event::Text(text) | Event::Code(text) if image_depth > 0 => Event::Text(text),
            _ if image_depth > 0 => continue,
            Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
            Event::Start(Tag::Paragraph) if block_marker(body[range.clone()].trim()).is_some() => {
                in_marker = true;
                if let Some(id) = analysis.anchors.get(&range.start) {
                    Event::Html(format!("<span id=\"{id}\"></span>").into())
                } else {
                    continue;
                }
            }
            Event::End(TagEnd::Paragraph) if in_marker => {
                in_marker = false;
                continue;
            }
            _ if in_marker => continue,
            Event::Start(Tag::Paragraph) => {
                let id = &analysis.anchors[&range.start];
                Event::Html(format!("<p id=\"{id}\">").into())
            }
            Event::Start(Tag::Heading { level, .. }) => Event::Start(Tag::Heading {
                level: embedded_level(level),
                id: Some(analysis.anchors[&range.start].clone().into()),
                classes: vec![],
                attrs: vec![],
            }),
            Event::End(TagEnd::Heading(level)) => {
                Event::End(TagEnd::Heading(embedded_level(level)))
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            }) => {
                let href = if let Some(ordinal) = analysis.ordinals.get(&range.start) {
                    links
                        .get(*ordinal)
                        .and_then(Option::as_ref)
                        .filter(|h| {
                            h.starts_with('/')
                                && !h.starts_with("//")
                                && !h.contains('\\')
                                && !h.chars().any(char::is_control)
                        })
                        .cloned()
                } else {
                    external_url(link_type, &dest_url)
                };
                span_link = href.is_none();
                if let Some(href) = href {
                    Event::Start(Tag::Link {
                        link_type: LinkType::Inline,
                        dest_url: href.into(),
                        title,
                        id: "".into(),
                    })
                } else {
                    Event::InlineHtml("<span class=\"unresolved-link\">".into())
                }
            }
            Event::End(TagEnd::Link) if span_link => {
                span_link = false;
                Event::InlineHtml("</span>".into())
            }
            other => other,
        };
        events.push(safe);
    }
    let mut html = String::new();
    markdown_html::push_html(&mut html, events.into_iter());
    maud::PreEscaped(html)
}
pub(super) fn note_wikilink(path: &str) -> String {
    // Delimiters remain legal filenames, including a literal closing pair of brackets.
    const RESERVED: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
        .add(b'%')
        .add(b'#')
        .add(b'|')
        .add(b'[')
        .add(b']')
        .add(b'&')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'\\');
    format!("[[/{}]]", utf8_percent_encode(path, RESERVED))
}

/// Validate user-entered paths before they can become entries in a Markdown export.
pub fn validate_path(input: &str) -> Result<String, String> {
    let mut path = input.trim().to_owned();
    if path.is_empty() || !path.split('/').all(note_component) {
        return Err("Use a relative Markdown path with visible folder and file names.".into());
    }
    if !has_note_extension(&path) {
        path.push_str(".md");
    }
    if path.len() > 1024 || path.split('/').any(|part| part.len() > 255) {
        return Err("Shorten the note path or one of its folder or file names.".into());
    }
    Ok(path)
}

fn note_component(part: &str) -> bool {
    !part.trim().is_empty()
        && !part.starts_with('.')
        && !part
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\\' | ':'))
}

fn has_note_extension(path: &str) -> bool {
    path.as_bytes()
        .last_chunk::<3>()
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".md"))
}

fn note_stem(path: &str) -> &str {
    if has_note_extension(path) {
        &path[..path.len() - 3]
    } else {
        path
    }
}

fn joined_note_path<'a>(base: &'a str, target: &'a str) -> Option<String> {
    let mut path: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "." => {}
            ".." => {
                path.pop()?;
            }
            part if note_component(part) => path.push(part),
            _ => return None,
        }
    }
    Some(path.join("/"))
}

pub(super) fn candidates(
    source_path: &str,
    reference: &Reference,
    notes: &[(uuid::Uuid, String, String)],
) -> Vec<uuid::Uuid> {
    let target = reference.target.trim();
    if let Some(id) = native_note(target) {
        return notes
            .iter()
            .find(|(note, _, _)| *note == id)
            .map(|n| n.0)
            .into_iter()
            .collect();
    }
    if target.is_empty() {
        return notes
            .iter()
            .filter(|(_, path, _)| reference.fragment.is_some() && path == source_path)
            .map(|(id, _, _)| *id)
            .collect();
    }

    if reference.kind == "wiki" && !target.contains('/') && note_component(target) {
        // A bare wiki name must not silently pick the note in the current folder.
        let stem = note_stem(target);
        let matches: Vec<_> = notes
            .iter()
            .filter(|(_, path, _)| note_stem(path.rsplit('/').next().unwrap_or(path)) == stem)
            .map(|(id, _, _)| *id)
            .collect();
        if !matches.is_empty() {
            return matches;
        }
        return notes
            .iter()
            .filter(|(_, _, title)| title == target)
            .map(|(id, _, _)| *id)
            .collect();
    }

    let lookup = |base: &str, path: &str| -> Vec<uuid::Uuid> {
        let Some(joined) = joined_note_path(base, path) else {
            return Vec::new();
        };
        notes
            .iter()
            .filter(|(_, path, _)| note_stem(path) == note_stem(&joined))
            .map(|(id, _, _)| *id)
            .collect()
    };
    if let Some(path) = target.strip_prefix('/') {
        return lookup("", path);
    }
    if reference.kind == "wiki" && !target.starts_with("./") && !target.starts_with("../") {
        let root_matches = lookup("", target);
        if !root_matches.is_empty() {
            return root_matches;
        }
    }
    let folder = source_path
        .rsplit_once('/')
        .map_or("", |(folder, _)| folder);
    lookup(folder, target)
}
#[cfg(test)]
mod path_tests {
    use super::{Reference, candidates, validate_path};
    use uuid::Uuid;

    fn reference(kind: &str, target: &str) -> Reference {
        Reference {
            raw: target.to_string(),
            target: target.to_string(),
            fragment: None,
            label: target.to_string(),
            kind: kind.to_string(),
        }
    }

    fn notes() -> Vec<(Uuid, String, String)> {
        [
            (1, "daily/Index.md", "Résumé"),
            (2, "daily/Design.md", "Local design"),
            (3, "archive/Design.MD", "Old design"),
            (4, "Design.md", "Root design"),
            (5, "daily/archive/Local.md", "Neighbor"),
        ]
        .into_iter()
        .map(|(n, path, name)| (Uuid::from_u128(n), path.into(), name.into()))
        .collect()
    }

    #[test]
    fn requires_a_choice_between_duplicate_wiki_basenames() {
        let choices = candidates("daily/Index.md", &reference("wiki", "Design"), &notes());
        assert_eq!(choices.len(), 3);
        for id in [2, 3, 4] {
            assert!(choices.contains(&Uuid::from_u128(id)));
        }
    }

    #[test]
    fn resolves_explicit_paths_and_markdown_relative_to_source() {
        for (kind, target, id) in [
            ("wiki", "../archive/Design", 3),
            ("wiki", "./Design.md", 2),
            ("wiki", "archive/Design", 3),
            ("wiki", "archive/Local", 5),
            ("wiki", "/Design", 4),
            ("markdown", "../Design.md", 4),
            ("markdown", "Design.md", 2),
            ("markdown", "archive/Local.md", 5),
            ("markdown", "/Design.MD", 4),
            ("wiki", "Résumé", 1),
        ] {
            assert_eq!(
                candidates("daily/Index.md", &reference(kind, target), &notes()),
                [Uuid::from_u128(id)],
                "{kind}: {target}"
            );
        }
    }

    #[test]
    fn resolves_same_note_fragment_and_refuses_to_escape_root() {
        let mut local = reference("wiki", "");
        local.fragment = Some("^paragraph".into());
        assert_eq!(
            candidates("daily/Index.md", &local, &notes()),
            [Uuid::from_u128(1)]
        );
        for target in [
            "../../archive/Design",
            "//example.com/Design",
            "C:/Design",
            ".hidden/../Design",
        ] {
            assert!(
                candidates("daily/Index.md", &reference("wiki", target), &notes()).is_empty(),
                "{target}"
            );
        }
    }

    #[test]
    fn validates_portable_unicode_markdown_paths() {
        assert_eq!(
            validate_path("Équipe/仕様 v2").unwrap(),
            "Équipe/仕様 v2.md"
        );
        assert_eq!(validate_path("Notes/v1.2.MD").unwrap(), "Notes/v1.2.MD");
        for raw in [
            "",
            "../draft.md",
            "foo/../../draft.md",
            "/tmp/draft.md",
            "a//b",
            "C:/design",
            "a\\b",
            "a/.files/draft.md",
            "a/draft\0.md",
            "folder/",
            "./draft.md",
        ] {
            assert!(validate_path(raw).is_err(), "{raw:?}");
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn check_render(body: &str, links: &[Option<String>]) -> Markup {
        render(body, links, Uuid::nil(), "notes/spec.md")
    }

    #[test]
    fn serves_local_images_only_via_authenticated_encoded_project_path() {
        let html = check_render("![art](../Images/設計%20map.png) ![[/Images/test.png|image]] ![no](../../bad.png) ![evil](//bad.host/x.png)", &[]).into_string();
        assert_eq!(html.matches("<img").count(), 2);
        assert!(html.contains("/transfer/media/00000000-0000-0000-0000-000000000000/Images/%E8%A8%AD%E8%A8%88%20map.png"));
        assert!(
            html.contains("/transfer/media/00000000-0000-0000-0000-000000000000/Images/test.png")
        );
    }

    #[test]
    fn resolves_only_prose_wiki_and_markdown_references() {
        let source = "---\nprop: '[[hidden]]'\n---\n# Design\n\nSee [[../決定/Index#Step Two|setup]], [block](../plan.md#%5Eidea) and [[WK-21]].\n\n`[[inline]]`\n\n```text\n[[fenced]]\n```\n";
        let doc = Document::parse(source);
        assert_eq!(
            doc.links
                .iter()
                .map(|l| (
                    l.target.as_str(),
                    l.fragment.as_deref(),
                    l.label.as_str(),
                    l.kind.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("../決定/Index", Some("Step Two"), "setup", "wiki"),
                ("../plan.md", Some("^idea"), "block", "markdown"),
                ("WK-21", None, "WK-21", "wiki")
            ]
        );
        assert_eq!(doc.links[0].raw, "../決定/Index#Step Two");
        let html = check_render(
            source,
            &[
                Some("/notes/a#heading-step-two".into()),
                Some("/notes/b#block-idea".into()),
                Some("/issues/c".into()),
            ],
        )
        .into_string();
        assert!(html.contains("href=\"/notes/a#heading-step-two\""));
        assert!(html.contains("href=\"/issues/c\""));
        assert!(html.contains("<code>[[inline]]</code>"));
    }

    #[test]
    fn keeps_paragraph_anchors_on_reorder_and_tracks_block_ids() {
        let original = "Initial paragraph.\n\nDecision **here**. ^a-1\n\nPlain.\n\nPlain.";
        let moved = "Other.\n\nInitial paragraph.\n\nRewritten. ^a-1";
        let a = Document::parse(original);
        let b = Document::parse(moved);
        assert_eq!(a.paragraphs.len(), 4);
        assert_eq!(b.paragraphs[1].anchor, a.paragraphs[0].anchor);
        assert_eq!(a.paragraphs[1].anchor, fragment_id("^a-1"));
        assert_eq!(b.paragraphs[2].anchor, a.paragraphs[1].anchor);
        assert_ne!(a.paragraphs[2].anchor, a.paragraphs[3].anchor);
        let html = check_render(original, &[]).into_string();
        assert!(html.contains(&format!("id=\"{}\"", a.paragraphs[0].anchor)));
        assert!(html.contains(&format!("id=\"{}\"", a.paragraphs[1].anchor)));
    }

    #[test]
    fn standalone_block_marker_binds_the_preceding_paragraph_without_an_extra_choice() {
        let source = "Decided to publish.\n\n^publication\n\nStill open.";
        let doc = Document::parse(source);
        assert_eq!(doc.paragraphs.len(), 2);
        assert_eq!(doc.paragraphs[0].anchor, fragment_id("^publication"));
        assert_eq!(doc.paragraphs[0].excerpt, "Decided to publish.");
        let html = check_render(source, &[]).into_string();
        assert!(html.contains("<p id=\"block-publication\">Decided to publish."));
    }

    #[test]
    fn markers_keep_duplicate_anchors_unique_and_do_not_jump_over_other_blocks() {
        let body = "First\n\n^tag\n\nSecond ^tag\n\n# Heading\n\n^heading-mark";
        let parsed = Document::parse(body);
        assert_eq!(parsed.paragraphs.len(), 2);
        assert_eq!(parsed.paragraphs[0].anchor, "block-tag");
        assert_eq!(parsed.paragraphs[1].anchor, "block-tag-2");
        let html = check_render(body, &[]).into_string();
        for id in ["block-tag", "block-tag-2", "block-heading-mark"] {
            assert_eq!(html.matches(&format!("id=\"{id}\"")).count(), 1);
        }
        assert!(!html.contains("^heading-mark"));
    }

    #[test]
    fn emits_named_sections_and_distinct_duplicate_anchors() {
        let html = check_render("# One *decision*\n\n# One decision", &[]).into_string();
        assert!(html.starts_with("<h2 ") && html.trim_end().ends_with("</h2>"));
        assert!(html.contains(&format!("id=\"{}\"", fragment_id("One decision"))));
        assert!(html.contains(&format!("id=\"{}-2\"", fragment_id("One decision"))));
    }

    #[test]
    fn leaves_scripts_and_images_inert_and_preserves_safe_external_links() {
        let source = "<script>alert(1)</script>\n\n[x](javascript:alert%281%29) ![leak](https://example.org/tracker) [safe](https://example.org/page?q=1) [[missing]]";
        let doc = Document::parse(source);
        assert_eq!(doc.links.len(), 1);
        let html = check_render(source, &[]).into_string();
        assert!(!html.contains("<script"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(!html.contains("<img"));
        assert!(html.contains("href=\"https://example.org/page?q=1\""));
        assert!(html.contains("missing"));
    }

    #[test]
    fn unsupported_author_syntax_and_external_images_cannot_steal_a_reference_slot() {
        let body = "[[Label **bold**|plain]] then [[Project]]\n\n[[broken\n\n[[Last]] ![image [[no]]](//evil.example/image)\n\n[example](https://safe.example)";
        let doc = Document::parse(body);
        assert_eq!(
            doc.links
                .iter()
                .map(|r| r.target.as_str())
                .collect::<Vec<_>>(),
            vec!["Label **bold**", "Project", "Last"]
        );
        let html = check_render(
            body,
            &[
                Some("/notes/first".into()),
                None,
                Some("/notes/last".into()),
            ],
        )
        .into_string();
        assert!(html.contains("href=\"/notes/first\">plain</a>"));
        assert!(html.contains("href=\"/notes/last\">Last</a>"));
    }
}

#[cfg(test)]
mod suggestions {
    use super::*;
    #[test]
    fn private_note_urls_require_an_existing_note_in_the_scoped_catalog() {
        let id = Uuid::new_v4();
        let catalog = vec![(id, "architecture.md".into(), "Architecture".into())];
        let document = Document::parse(&format!(
            "[Decision](/notes/{id}#choice) [Unknown](/notes/{})",
            Uuid::new_v4()
        ));
        assert_eq!(
            candidates("source.md", &document.links[0], &catalog),
            vec![id]
        );
        assert!(candidates("source.md", &document.links[1], &catalog).is_empty());
        for anchor in ["heading-another-note", "block-choice", "p-123ab"] {
            assert_eq!(link_fragment(&format!("/notes/{id}"), anchor), anchor);
        }
        assert_eq!(
            link_fragment("Architecture.md", "block-choice"),
            "heading-block-choice"
        );
    }
    #[test]
    fn paths_with_reserved_wiki_syntax_remain_one_reference() {
        let path = "Decisions/Design #2 | A]]B &amp; 50%.md";
        let doc = Document::parse(&note_wikilink(path));
        assert_eq!(doc.links.len(), 1);
        assert_eq!(doc.links[0].target, format!("/{path}"));
        assert_eq!(doc.links[0].fragment, None);
    }
}
