use crate::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    io::{self, Cursor, Read, Seek, SeekFrom},
};
use uuid::Uuid;
use zip::ZipArchive;
#[cfg(test)]
use {
    std::io::Write,
    zip::{ZipWriter, write::SimpleFileOptions},
};

pub const MAX_UPLOAD: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 2000;
const MAX_NOTE: usize = 2_000_000;
const MAX_MANIFEST: usize = 8 * 1024 * 1024;
const MAX_HISTORY: usize = 16 * 1024 * 1024;
const MAX_HISTORY_RECORDS: usize = 25_000;

#[derive(Debug)]
pub struct Package {
    pub entries: Vec<Entry>,
    pub warnings: Vec<String>,
    pub manifest: Option<Manifest>,
    pub history: Vec<Value>,
}

#[derive(Debug)]
pub struct Entry {
    pub path: String,
    pub bytes: Vec<u8>,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub project: Uuid,
    pub files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestFile {
    pub id: Uuid,
    pub path: String,
    pub kind: String,
    pub sha256: String,
    pub data: Value,
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::BadRequest(message.into())
}

fn safe_components(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 2048
        && !path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':'))
        && path
            .split('/')
            .all(|part| !part.trim().is_empty() && part.len() <= 255 && !matches!(part, "." | ".."))
}

fn hidden(path: &str) -> bool {
    path.split('/').any(|part| part.starts_with('.'))
}

fn macos_metadata(path: &str) -> bool {
    path.split('/').any(|part| part == "__MACOSX")
}

pub fn valid_path(path: &str) -> bool {
    safe_components(path) && !hidden(path)
}

fn extension(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map_or("", |(_, ext)| ext)
}

fn mime_for_extension(path: &str) -> Option<&'static str> {
    match extension(path).to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Verify the extension and the raster container type without expanding any pixels.
pub fn image_type(path: &str, bytes: &[u8]) -> Option<&'static str> {
    let mime = mime_for_extension(path)?;
    let matches = match mime {
        "image/png" => {
            bytes.len() >= 45
                && bytes.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR")
                && bytes.ends_with(b"\0\0\0\0IEND\xaeB\x60\x82")
                && bytes[16..20] != [0; 4]
                && bytes[20..24] != [0; 4]
        }
        "image/jpeg" => {
            bytes.len() >= 12 && bytes.starts_with(b"\xff\xd8\xff") && bytes.ends_with(b"\xff\xd9")
        }
        "image/gif" => {
            bytes.len() >= 14
                && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"))
                && bytes[6..8] != [0, 0]
                && bytes[8..10] != [0, 0]
                && bytes.last() == Some(&0x3b)
        }
        "image/webp" => {
            bytes.len() >= 24
                && bytes.starts_with(b"RIFF")
                && &bytes[8..12] == b"WEBP"
                && matches!(&bytes[12..16], b"VP8 " | b"VP8L" | b"VP8X")
                && u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize == bytes.len() - 8
        }
        _ => false,
    };
    matches.then_some(mime)
}

fn u16_at(b: &[u8], pos: usize) -> usize {
    u16::from_le_bytes([b[pos], b[pos + 1]]) as usize
}

fn u32_at(b: &[u8], pos: usize) -> usize {
    u32::from_le_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]]) as usize
}

/// Zip indexes entries when opening a file. Limit its index before opening it.
fn directory(bytes: &[u8]) -> AppResult<(usize, usize)> {
    if bytes.len() > MAX_UPLOAD || bytes.len() < 22 {
        return Err(invalid("Upload a ZIP of at most 64 MiB."));
    }
    // The standard ZIP32 footer is 22 bytes followed by a comment of up to 65,535 bytes.
    let tail = bytes.len().saturating_sub(22 + u16::MAX as usize);
    let end_offset = bytes[tail..]
        .windows(4)
        .enumerate()
        .rev()
        .find_map(|(i, signature)| {
            let position = tail + i;
            let end = bytes.get(position..position + 22)?;
            (signature == b"PK\x05\x06" && position + 22 + u16_at(end, 20) == bytes.len())
                .then_some(position)
        })
        .ok_or_else(|| invalid("The ZIP directory is missing or truncated."))?;
    let end = &bytes[end_offset..];
    let count = u16_at(end, 10);
    let start = u32_at(end, 16);
    if count == 0
        || count > MAX_ENTRIES
        || count != u16_at(end, 8)
        || u16_at(end, 4) != 0
        || u16_at(end, 6) != 0
        || end_offset != start.saturating_add(u32_at(end, 12))
        || bytes.get(start..start.saturating_add(4)) != Some(b"PK\x01\x02")
    {
        return Err(invalid(
            "Use a standard, single-volume ZIP with 1–2,000 entries and a complete directory.",
        ));
    }
    // Prevent the library's alternative-footer recovery from accepting other indexes.
    if bytes[start..end_offset]
        .windows(4)
        .chain(end[4..].windows(4))
        .any(|s| s == b"PK\x05\x06")
    {
        return Err(invalid("The ZIP contains ambiguous directory metadata."));
    }
    Ok((count, start))
}

/// During index construction only the checked central directory is visible. A
/// malformed header cannot induce a search for other ZIP footers embedded in a
/// stored attachment. Selected file streams become readable after index checks.
struct ArchiveReader<'a> {
    cursor: Cursor<&'a [u8]>,
    directory_floor: &'a Cell<u64>,
}

impl Read for ArchiveReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let prefix = self
            .directory_floor
            .get()
            .saturating_sub(self.cursor.position())
            .min(buf.len() as u64) as usize;
        if prefix > 0 {
            buf[..prefix].fill(0);
            self.cursor
                .set_position(self.cursor.position() + prefix as u64);
        }
        self.cursor
            .read(&mut buf[prefix..])
            .map(|read| prefix + read)
    }
}

impl Seek for ArchiveReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.cursor.seek(pos)
    }
}

struct Member {
    index: usize,
    path: String,
    size: u64,
}

fn members(
    bytes: &[u8],
    zip: &mut ZipArchive<ArchiveReader<'_>>,
    directory_start: usize,
    folder: &str,
    warnings: &mut Vec<String>,
) -> AppResult<Vec<Member>> {
    let mut selected = Vec::new();
    let mut byte_count = 0_u64;
    let mut seen = HashSet::new();
    let mut ignored_hidden = 0;
    let mut ranges = Vec::new();
    let folder_prefix = format!("{folder}/");
    for i in 0..zip.len() {
        let file = zip
            .by_index_raw(i)
            .map_err(|_| invalid("A ZIP file header could not be read."))?;
        let name = file.name().strip_suffix('/').unwrap_or(file.name());
        if !safe_components(name)
            || file.is_symlink()
            || file
                .unix_mode()
                .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o040000 | 0o100000))
        {
            return Err(invalid(
                "ZIP entries must use relative paths and regular files or directories.",
            ));
        }
        if !seen.insert(name.to_owned()) {
            return Err(invalid("Two ZIP entries use the same path."));
        }
        let header = usize::try_from(file.header_start())
            .map_err(|_| invalid("Invalid ZIP header position."))?;
        let local = bytes
            .get(header..)
            .and_then(|s| s.get(..30))
            .ok_or_else(|| invalid("Truncated ZIP header."))?;
        if !local.starts_with(b"PK\x03\x04")
            || bytes.get(header + 30..header + 30 + u16_at(local, 26)) != Some(file.name_raw())
        {
            return Err(invalid("The ZIP local and directory paths disagree."));
        }
        let packed_end = file
            .data_start()
            .checked_add(file.compressed_size())
            .filter(|end| *end <= directory_start as u64)
            .ok_or_else(|| invalid("A ZIP entry extends outside the data area."))?;
        ranges.push((file.header_start(), packed_end));
        byte_count = byte_count
            .checked_add(file.size())
            .filter(|n| *n <= MAX_UPLOAD as u64)
            .ok_or_else(|| invalid("The ZIP exceeds 64 MiB of expanded data."))?;
        if file.is_dir() {
            continue;
        }
        let relative = if folder.is_empty() {
            name
        } else if let Some(p) = name.strip_prefix(&folder_prefix) {
            p
        } else {
            continue;
        };
        if hidden(relative) {
            ignored_hidden += 1;
            continue;
        }
        selected.push(Member {
            index: i,
            path: relative.to_owned(),
            size: file.size(),
        });
    }
    ranges.sort_unstable();
    if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(invalid("ZIP entries overlap."));
    }
    if ignored_hidden > 0 {
        warnings.push(format!("Skipped {ignored_hidden} hidden or system files."));
    }
    if selected.is_empty() && !folder.is_empty() {
        return Err(invalid(
            "The selected folder has no visible files in this ZIP.",
        ));
    }
    Ok(selected)
}

fn content(
    zip: &mut ZipArchive<ArchiveReader<'_>>,
    member: &Member,
    cap: usize,
) -> AppResult<Vec<u8>> {
    if member.size > cap as u64 {
        return Err(invalid(format!(
            "{} exceeds the per-file byte limit ({cap} bytes).",
            member.path
        )));
    }
    let file = zip
        .by_index(member.index)
        .map_err(|_| invalid("An entry is encrypted or uses an unsupported compression method."))?;
    let mut bytes = Vec::with_capacity(member.size as usize);
    // The central header is untrusted. A forged size may understate the stream.
    file.take(member.size + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            invalid(format!(
                "{} could not be read or failed its ZIP checksum.",
                member.path
            ))
        })?;
    if bytes.len() as u64 != member.size {
        return Err(invalid(format!(
            "{} has an invalid expanded length.",
            member.path
        )));
    }
    Ok(bytes)
}

fn markdown_kind(path: &str, native: bool) -> Option<&'static str> {
    if !extension(path).eq_ignore_ascii_case("md") {
        return None;
    }
    if !native || path.starts_with("Notes/") {
        return Some("note");
    }
    if path.starts_with("Issues/") {
        return Some("issue");
    }
    if path.starts_with("Releases/") {
        return Some("release");
    }
    None
}

// Tiny JSON arrays have considerable allocation overhead for each byte on the
// wire. Count structural punctuation before building the serde_json Value tree.
fn check_json_shape(bytes: &[u8]) -> AppResult<()> {
    let mut quoted = false;
    let mut escaped = false;
    let mut structure = 0;
    for &byte in bytes {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if quoted => escaped = true,
            b'"' => quoted = !quoted,
            b'[' | b']' | b'{' | b'}' | b':' | b',' if !quoted => {
                structure += 1;
                if structure > 600_000 {
                    return Err(invalid("Native metadata exceeds the structure limit."));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn read_manifest(bytes: &[u8], has_native_history: bool) -> AppResult<Option<Manifest>> {
    check_json_shape(bytes)?;
    let mut manifest: Manifest = match serde_json::from_slice(bytes) {
        Ok(manifest) => manifest,
        Err(_) => {
            // A vault may itself contain a web app or plugin manifest at its root.
            // Only abandon native interpretation when the JSON is well formed and
            // lacks native format markers. Corrupt exports must never silently
            // turn issues or releases into ordinary notes.
            let unrelated = !has_native_history
                && serde_json::from_slice::<Value>(bytes).is_ok_and(|value| {
                    value.as_object().is_some_and(|object| {
                        !object.get("version").is_some_and(Value::is_number)
                            && !(object.contains_key("project") && object.contains_key("files"))
                    })
                });
            if unrelated {
                return Ok(None);
            }
            return Err(invalid(
                "Invalid native manifest.json. Import a vault folder or a version 1 Workspace export.",
            ));
        }
    };
    if manifest.version != 1 || manifest.project.is_nil() || manifest.files.len() > MAX_ENTRIES {
        return Err(invalid(
            "The native manifest version, project ID, or entry count is invalid.",
        ));
    }
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for f in &mut manifest.files {
        let expected =
            markdown_kind(&f.path, true).or_else(|| mime_for_extension(&f.path).map(|_| "blob"));
        if !valid_path(&f.path)
            || f.id.is_nil()
            || !ids.insert(f.id)
            || !paths.insert(f.path.clone())
            || f.sha256.len() != 64
            || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || expected != Some(f.kind.as_str())
        {
            return Err(invalid(
                "Native manifest entries need unique IDs and safe, correctly typed paths with SHA-256 hashes.",
            ));
        }
        f.sha256.make_ascii_lowercase();
    }
    Ok(Some(manifest))
}

fn relink_renames(manifest: &mut Manifest, entries: &[Entry], warnings: &mut Vec<String>) {
    let current: HashSet<_> = entries.iter().map(|e| e.path.as_str()).collect();
    let recorded: HashSet<_> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    let mut missing: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    let mut added: HashMap<(&str, String), Vec<&str>> = HashMap::new();
    for (index, f) in manifest
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| !current.contains(f.path.as_str()))
    {
        missing.entry((&f.kind, &f.sha256)).or_default().push(index);
    }
    for e in entries
        .iter()
        .filter(|e| !recorded.contains(e.path.as_str()))
    {
        let hash = hex::encode(Sha256::digest(&e.bytes));
        added.entry((&e.kind, hash)).or_default().push(&e.path);
    }
    let mut changes = Vec::new();
    for ((kind, hash), indexes) in missing {
        if let Some(new) = added.get(&(kind, hash.to_owned()))
            && new.len() == 1
            && indexes.len() == 1
        {
            changes.push((indexes[0], new[0].to_owned()));
        } else {
            for index in indexes {
                warnings.push(format!(
                    "{} is missing from the ZIP. Its existing record is kept.",
                    manifest.files[index].path
                ));
            }
        }
    }
    changes.sort_unstable_by_key(|(i, _)| *i);
    for (index, path) in changes {
        warnings.push(format!(
            "Recognized a rename from {} to {path}.",
            manifest.files[index].path
        ));
        manifest.files[index].path = path;
    }
}

pub fn parse(bytes: &[u8], folder: &str, image_limit: i64) -> AppResult<Package> {
    let folder = if folder.is_empty() {
        ""
    } else {
        let folder = folder.strip_suffix('/').unwrap_or(folder);
        if !valid_path(folder) || macos_metadata(folder) {
            return Err(invalid("Choose a visible relative folder in the ZIP."));
        }
        folder
    };
    let (count, start) = directory(bytes)?;
    let floor = Cell::new(start as u64);
    let mut zip = ZipArchive::new(ArchiveReader {
        cursor: Cursor::new(bytes),
        directory_floor: &floor,
    })
    .map_err(|_| invalid("The ZIP directory is unreadable."))?;
    if zip.len() != count || zip.offset() != 0 || zip.central_directory_start() != start as u64 {
        return Err(invalid(
            "The ZIP has duplicate names or inconsistent directory metadata.",
        ));
    }
    floor.set(0);
    let mut warnings = Vec::new();
    let selected = members(bytes, &mut zip, start, folder, &mut warnings)?;
    let mut manifest = if let Some(member) = selected.iter().find(|m| m.path == "manifest.json") {
        let bytes = content(&mut zip, member, MAX_MANIFEST)?;
        let detected = read_manifest(&bytes, selected.iter().any(|m| m.path == "history.jsonl"))?;
        if detected.is_none() {
            warnings.push("Skipped manifest.json from another application.".into());
        }
        detected
    } else {
        None
    };
    let mut history = Vec::new();
    let mut entries = Vec::new();
    let mut has_history = false;
    let native_paths: HashSet<_> = manifest
        .iter()
        .flat_map(|m| m.files.iter().map(|f| f.path.as_str()))
        .collect();
    let mut skipped_macos = 0;
    for m in selected {
        if m.path == "manifest.json" {
            continue;
        }
        // A user may explicitly author a note inside a folder named __MACOSX.
        // Preserve it when its identity is recorded by the native export.
        if macos_metadata(&m.path) && !native_paths.contains(m.path.as_str()) {
            skipped_macos += 1;
            continue;
        }
        // These are human-readable copies of checkpoint state carried by issues.
        if manifest.is_some()
            && m.path.starts_with("Checkpoints/")
            && !native_paths.contains(m.path.as_str())
        {
            continue;
        }
        if m.path == "history.jsonl" && manifest.is_some() {
            has_history = true;
            let data = content(&mut zip, &m, MAX_HISTORY)?;
            check_json_shape(&data)?;
            for line in data
                .split(|&c| c == b'\n')
                .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            {
                if history.len() >= MAX_HISTORY_RECORDS {
                    return Err(invalid("The native history exceeds 25,000 records."));
                }
                let value: Value = serde_json::from_slice(line)
                    .map_err(|_| invalid("Invalid JSON record in history.jsonl."))?;
                if !value.is_object() {
                    return Err(invalid("Each native history line must be an object."));
                }
                history.push(value);
            }
            continue;
        }
        let (kind, data) = if let Some(kind) = markdown_kind(&m.path, manifest.is_some()) {
            let cap = if kind == "issue" {
                2 * 1024 * 1024 + 1024
            } else {
                MAX_NOTE
            };
            let data = content(&mut zip, &m, cap)?;
            if std::str::from_utf8(&data).is_err() || data.contains(&0) {
                return Err(invalid(format!(
                    "{} must be UTF-8 Markdown without NUL bytes.",
                    m.path
                )));
            }
            (kind, data)
        } else if mime_for_extension(&m.path).is_some() {
            let data = content(&mut zip, &m, usize::try_from(image_limit).unwrap_or(0))?;
            if image_type(&m.path, &data).is_none() {
                return Err(invalid(format!(
                    "{} does not match its raster image format.",
                    m.path
                )));
            }
            ("blob", data)
        } else {
            warnings.push(format!("Unsupported file skipped: {}", m.path));
            continue;
        };
        entries.push(Entry {
            path: m.path,
            bytes: data,
            kind: kind.into(),
        });
    }
    if skipped_macos > 0 {
        warnings.push(format!("Skipped {skipped_macos} macOS metadata files."));
    }
    if let Some(manifest) = &mut manifest {
        if !has_history {
            warnings.push("The native archive has no history.jsonl file.".into());
        }
        relink_renames(manifest, &entries, &mut warnings);
        let recorded: HashSet<_> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        entries.retain(|e| {
            if (e.kind == "issue" || e.kind == "release") && !recorded.contains(e.path.as_str()) {
                warnings.push(format!(
                    "Skipped {} because its structured metadata is missing.",
                    e.path
                ));
                false
            } else {
                true
            }
        });
    }
    let mut names: HashMap<&str, usize> = HashMap::new();
    for note in entries.iter().filter(|entry| entry.kind == "note") {
        let basename = note.path.rsplit('/').next().unwrap_or(&note.path);
        // Bare wiki targets resolve by stem, including when .MD has uppercase letters.
        let stem = basename.rsplit_once('.').map_or(basename, |(stem, _)| stem);
        *names.entry(stem).or_default() += 1;
    }
    for (name, duplicates) in names.into_iter().filter(|(_, count)| *count > 1) {
        warnings.push(format!(
            "{duplicates} notes share the name “{name}”. Use a folder-qualified link to select one."
        ));
    }
    entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    warnings.sort();
    Ok(Package {
        entries,
        warnings,
        manifest,
        history,
    })
}

#[cfg(test)]
pub fn encode(files: Vec<(String, Vec<u8>)>) -> AppResult<Vec<u8>> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600);
    let mut paths = HashSet::new();
    for (path, bytes) in files {
        if !valid_path(&path) || !paths.insert(path.clone()) {
            return Err(AppError::Internal(
                "Project files have unsafe or conflicting export paths.".into(),
            ));
        }
        zip.start_file(
            path,
            options.large_file(bytes.len() as u64 >= u32::MAX as u64),
        )
        .map_err(|_| AppError::Internal("Could not open a project archive entry.".into()))?;
        zip.write_all(&bytes)
            .map_err(|_| AppError::Internal("Could not write project archive data.".into()))?;
    }
    zip.finish()
        .map(|file| file.into_inner())
        .map_err(|_| AppError::Internal("Could not finalize the project archive.".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in files {
            out.start_file(*name, SimpleFileOptions::default()).unwrap();
            out.write_all(bytes).unwrap();
        }
        out.finish().unwrap().into_inner()
    }

    fn png() -> Vec<u8> {
        hex::decode("89504e470d0a1a0a0000000d4948445200000001000000010804000000b51c0c020000000b4944415478da6364f80f00010501012718e3660000000049454e44ae426082").unwrap()
    }

    #[test]
    fn selected_folder_preserves_bytes_and_excludes_settings_and_neighboring_prefixes() {
        let note = "---\r\n# comment\r\nunknown: true\r\n---\r\n[[概念/Design]]";
        let image = png();
        let zip = fixture(&[
            ("Vault/Decisions/概念/Design.md", note.as_bytes()),
            ("Vault/Decisions/media/p.png", &image),
            ("Vault/Decisions/media/map.svg", b"<svg/>"),
            ("Vault/Decisions/.obsidian/data.md", b"private"),
            ("Vault/Decisions2/unrelated.md", b"not in scope"),
        ]);
        let p = parse(&zip, "Vault/Decisions/", 5000).unwrap();
        assert_eq!(p.entries.len(), 2);
        assert!(p.manifest.is_none());
        assert!(
            p.entries
                .iter()
                .any(|e| e.path == "概念/Design.md" && e.bytes == note.as_bytes())
        );
        assert!(
            p.entries
                .iter()
                .any(|e| e.kind == "blob" && e.bytes == image)
        );
        assert!(p.warnings.iter().any(|s| s.contains("map.svg")));
        assert!(parse(&zip, "Vault/.obsidian", 5000).is_err());
        assert!(parse(&zip, "missing", 5000).is_err());
    }

    #[test]
    fn native_roundtrip_retains_source_bytes_hashes_history_and_unique_rename_identity() {
        let note = b"---\r\n# unknown comment\r\nother: yes\r\n---\r\n";
        let first = Uuid::new_v4();
        let manifest = Manifest {
            version: 1,
            project: Uuid::new_v4(),
            files: vec![ManifestFile {
                id: first,
                path: "Notes/old.md".into(),
                kind: "note".into(),
                sha256: hex::encode(Sha256::digest(note)),
                data: serde_json::json!({}),
            }],
        };
        let zip = encode(vec![
            (
                "manifest.json".into(),
                serde_json::to_vec(&manifest).unwrap(),
            ),
            ("Notes/new.md".into(), note.to_vec()),
            ("history.jsonl".into(), b"{\"type\":\"record\"}\n".to_vec()),
        ])
        .unwrap();
        let restore = parse(&zip, "", 1024).unwrap();
        let f = &restore.manifest.unwrap().files[0];
        assert_eq!((f.id, f.path.as_str()), (first, "Notes/new.md"));
        assert_eq!(f.sha256, hex::encode(Sha256::digest(note)));
        assert_eq!(restore.entries[0].bytes, note);
        assert_eq!(restore.history[0]["type"], "record");
    }

    #[test]
    fn rejects_traversal_links_invalid_media_and_over_limit_sources() {
        for name in [
            "../outside.md",
            "x/../outside.md",
            "/abs.md",
            "C:/abs.md",
            "folder\\escape.md",
        ] {
            assert!(
                parse(&fixture(&[(name, b"x")]), "", 1024).is_err(),
                "{name}"
            );
        }
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        zip.add_symlink("alias", "target", SimpleFileOptions::default())
            .unwrap();
        assert!(parse(&zip.finish().unwrap().into_inner(), "", 1024).is_err());
        assert!(parse(&fixture(&[("misleading.png", b"<html>")]), "", 1024).is_err());
        assert!(parse(&fixture(&[("picture.png", &png())]), "", 4).is_err());
        assert!(
            parse(
                &fixture(&[("note.md", &vec![b'x'; MAX_NOTE + 1])]),
                "",
                1024
            )
            .is_err()
        );
    }

    #[test]
    fn vault_folder_name_never_implies_structured_import_without_native_manifest() {
        let out = parse(&fixture(&[("Issues/input.md", b"plain")]), "", 1024).unwrap();
        assert_eq!(out.entries[0].kind, "note");
        assert!(
            parse(
                &fixture(&[("manifest.json", b"{\"version\":99}")]),
                "",
                1024
            )
            .is_err()
        );
    }
    #[test]
    fn duplicate_names_in_different_folders_are_visible_and_retained() {
        let z = fixture(&[("one/Design.md", b"first"), ("two/Design.MD", b"second")]);
        let pkg = parse(&z, "", 1024).unwrap();
        assert_eq!(pkg.entries.len(), 2);
        assert!(
            pkg.warnings
                .iter()
                .any(|w| w.contains("2 notes share") && w.contains("Design"))
        );
        assert!(parse(&z, "/", 1024).is_err());
    }

    #[test]
    fn ordinary_edited_native_note_keeps_its_original_base_hash() {
        let previous = Uuid::new_v4();
        let missing = Uuid::new_v4();
        let digest = hex::encode(Sha256::digest(b"before"));
        let manifest = Manifest {
            version: 1,
            project: Uuid::new_v4(),
            files: [(previous, "Notes/keep.md"), (missing, "Notes/missing.md")]
                .into_iter()
                .map(|(id, path)| ManifestFile {
                    id,
                    path: path.into(),
                    kind: "note".into(),
                    sha256: digest.clone(),
                    data: serde_json::json!({}),
                })
                .collect(),
        };
        let encoded = serde_json::to_vec(&manifest).unwrap();
        let zipped = fixture(&[
            ("manifest.json", &encoded),
            ("Notes/keep.md", b"after"),
            ("Notes/one.md", b"before"),
            ("Notes/two.md", b"before"),
        ]);
        let imported = parse(&zipped, "", 5000).unwrap();
        let files = imported.manifest.unwrap().files;
        assert_eq!(files[0].id, previous);
        assert_eq!(files[0].sha256, digest);
        assert_eq!(files[1].path, "Notes/missing.md");
        assert_eq!(
            imported
                .entries
                .iter()
                .find(|e| e.path == "Notes/keep.md")
                .unwrap()
                .bytes,
            b"after"
        );
        assert!(imported.warnings.iter().any(|w| w.contains("missing.md")));
    }

    #[test]
    fn count_limit_and_duplicate_paths_apply_to_the_entire_archive() {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        for i in 0..=MAX_ENTRIES {
            w.start_file(
                format!("batch/{i}.md"),
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        }
        let oversized = w.finish().unwrap().into_inner();
        assert!(parse(&oversized, "batch", 1024).is_err());
        let duplicate_file_and_directory = fixture(&[("file.md", b"test"), ("file.md/", b"")]);
        assert!(parse(&duplicate_file_and_directory, "", 1024).is_err());
    }

    #[test]
    fn validates_native_history_record_shapes_and_manifest_entry_types() {
        let m = Manifest {
            version: 1,
            project: Uuid::new_v4(),
            files: vec![],
        };
        let manifest = serde_json::to_vec(&m).unwrap();
        let archive = fixture(&[("manifest.json", &manifest), ("history.jsonl", b"[]")]);
        assert!(parse(&archive, "", 1000).is_err());
        let mut wrong_kind = m;
        wrong_kind.files.push(ManifestFile {
            id: Uuid::new_v4(),
            path: "Notes/n.md".into(),
            kind: "release".into(),
            sha256: "0".repeat(64),
            data: Value::Null,
        });
        let encoded = serde_json::to_vec(&wrong_kind).unwrap();
        assert!(parse(&fixture(&[("manifest.json", &encoded)]), "", 1000).is_err());
    }
    #[test]
    fn unrelated_application_manifest_does_not_turn_a_vault_into_a_native_export() {
        let unrelated = br#"{ "name": "Vault site", "version":"1.0.0", "icons":[] }"#;
        let vault = fixture(&[
            ("manifest.json", unrelated),
            ("Issues/Idea.md", b"# An ordinary note"),
        ]);
        let result = parse(&vault, "", 1000).unwrap();
        assert!(result.manifest.is_none());
        assert_eq!(result.entries[0].kind, "note");
        assert!(result.warnings.iter().any(|w| w.contains("manifest.json")));
        let corrupt = fixture(&[("manifest.json", unrelated), ("history.jsonl", b"")]);
        assert!(parse(&corrupt, "", 1000).is_err());
    }

    #[test]
    fn unsupported_stored_archive_in_vault_does_not_affect_the_outer_directory() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file("data.zip", stored).unwrap();
        writer
            .write_all(&fixture(&[("inner.md", b"nested content")]))
            .unwrap();
        writer.start_file("outer.md", stored).unwrap();
        writer.write_all(b"outer content").unwrap();
        let pkg = parse(&writer.finish().unwrap().into_inner(), "", 1000).unwrap();
        assert_eq!(pkg.entries.len(), 1);
        assert_eq!(pkg.entries[0].path, "outer.md");
        assert!(pkg.warnings.iter().any(|w| w.contains("data.zip")));
    }
    #[test]
    fn native_authored_macosx_notes_roundtrip_while_vault_sidecars_stay_excluded() {
        let data = b"Authored decision";
        let path = "Notes/__MACOSX/decision.md";
        let record = ManifestFile {
            id: Uuid::new_v4(),
            path: path.into(),
            kind: "note".into(),
            sha256: hex::encode(Sha256::digest(data)),
            data: Value::Null,
        };
        let manifest = Manifest {
            project: Uuid::new_v4(),
            version: 1,
            files: vec![record],
        };
        let files = vec![
            (
                "manifest.json".into(),
                serde_json::to_vec(&manifest).unwrap(),
            ),
            (path.into(), data.to_vec()),
            ("Notes/__MACOSX/extra.md".into(), b"noise".to_vec()),
        ];
        let archive = encode(files).unwrap();
        let restored = parse(&archive, "", 1000).unwrap();
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].bytes, data);
        assert_eq!(restored.entries[0].path, path);

        let ordinary = fixture(&[("__MACOSX/decision.md", data), ("decision.md", data)]);
        let imported = parse(&ordinary, "", 1000).unwrap();
        assert_eq!(imported.entries.len(), 1);
        assert_eq!(imported.entries[0].path, "decision.md");
        assert!(parse(&ordinary, "__MACOSX", 1000).is_err());
    }

    #[test]
    fn native_checkpoint_reading_copies_are_ignored_and_vault_checkpoints_are_notes() {
        let path = "Checkpoints/4.md";
        let data = b"# Checkpoint";
        let manifest = Manifest {
            version: 1,
            project: Uuid::new_v4(),
            files: vec![],
        };
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let native = fixture(&[
            ("manifest.json", &manifest_bytes),
            ("history.jsonl", b""),
            (path, data),
        ]);
        let restored = parse(&native, "", 1000).unwrap();
        assert!(restored.entries.is_empty());
        assert!(restored.warnings.is_empty());
        let plain = parse(&fixture(&[(path, data)]), "", 1000).unwrap();
        assert_eq!(plain.entries[0].kind, "note");
        assert_eq!(plain.entries[0].path, path);
    }
}
