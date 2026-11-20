use std::collections::{HashMap, HashSet, VecDeque};

use sqlx::{Postgres, QueryBuilder, Transaction};
use uuid::Uuid;

use super::markdown::{Document, Reference, candidates};
use crate::{AppError, AppResult};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
struct Destination {
    note: Option<Uuid>,
    issue: Option<Uuid>,
    manual: bool,
}

#[derive(sqlx::FromRow)]
struct Previous {
    source_id: Uuid,
    kind: String,
    raw: String,
    label: String,
    target: String,
    dest_note: Option<Uuid>,
    dest_issue: Option<Uuid>,
    manual: bool,
}

impl Previous {
    fn binding(&self) -> Option<Destination> {
        (self.dest_note.is_some() || self.dest_issue.is_some()).then_some(Destination {
            note: self.dest_note,
            issue: self.dest_issue,
            manual: self.manual,
        })
    }
}

// Match occurrences by their written reference, independent of position. Reserve all
// exact matches before carrying a single edited alias or fragment across a revision.
fn retain_bindings(old: Vec<Previous>, links: &[Reference]) -> Vec<Option<Destination>> {
    let mut exact = HashMap::new();
    for (i, item) in old.iter().enumerate() {
        let key = (item.kind.as_str(), item.raw.as_str(), item.label.as_str());
        exact.entry(key).or_insert_with(VecDeque::new).push_back(i);
    }
    let mut consumed = vec![false; old.len()];
    let mut bindings = vec![None; links.len()];
    let mut edited = HashMap::new();
    for (i, link) in links.iter().enumerate() {
        let key = (link.kind.as_str(), link.raw.as_str(), link.label.as_str());
        if let Some(previous) = exact.get_mut(&key).and_then(VecDeque::pop_front) {
            consumed[previous] = true;
            bindings[i] = old[previous].binding();
        } else {
            edited
                .entry((link.kind.as_str(), link.target.as_str()))
                .or_insert_with(Vec::new)
                .push(i);
        }
    }
    let mut remaining = HashMap::new();
    for (i, item) in old.iter().enumerate() {
        if !consumed[i] {
            remaining
                .entry((item.kind.as_str(), item.target.as_str()))
                .or_insert_with(Vec::new)
                .push(item);
        }
    }
    for (key, ordinals) in edited {
        if let ([ordinal], Some([previous])) =
            (ordinals.as_slice(), remaining.get(&key).map(Vec::as_slice))
        {
            bindings[*ordinal] = previous.binding();
        }
    }
    bindings
}

struct IssueCatalog {
    key: String,
    by_number: HashMap<i64, Uuid>,
    ids: HashSet<Uuid>,
}

pub(super) fn native_issue(target: &str) -> Option<Uuid> {
    Uuid::parse_str(target.strip_prefix("/issues/")?).ok()
}

impl IssueCatalog {
    fn lookup(&self, reference: &Reference) -> Option<Uuid> {
        if let Some(id) = native_issue(&reference.target) {
            return self.ids.contains(&id).then_some(id);
        }
        if reference.kind != "wiki" {
            return None;
        }
        let (key, number) = reference.target.rsplit_once('-')?;
        if !self.key.eq_ignore_ascii_case(key)
            || number.starts_with('0')
            || !number.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        self.by_number.get(&number.parse::<i64>().ok()?).copied()
    }
}

type NoteInfo = (Uuid, String, String);
type Tx<'a> = Transaction<'a, Postgres>;

struct Catalog {
    notes: Vec<NoteInfo>,
    issues: IssueCatalog,
}

impl Catalog {
    async fn load(tx: &mut Tx<'_>, owner: Uuid, project: Uuid) -> AppResult<Self> {
        let key = sqlx::query_scalar("SELECT key FROM projects WHERE owner_id=$1 AND id=$2")
            .bind(owner)
            .bind(project)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let notes = sqlx::query_as(
            "SELECT id,path,title FROM notes WHERE owner_id=$1 AND project_id=$2 ORDER BY path,id",
        )
        .bind(owner)
        .bind(project)
        .fetch_all(&mut **tx)
        .await?;
        let issues = sqlx::query_as::<_, (i64, Uuid)>(
            "SELECT number,id FROM issues WHERE owner_id=$1 AND project_id=$2",
        )
        .bind(owner)
        .bind(project)
        .fetch_all(&mut **tx)
        .await?;
        Ok(Self {
            notes,
            issues: IssueCatalog {
                key,
                ids: issues.iter().map(|(_, id)| *id).collect(),
                by_number: issues.into_iter().collect(),
            },
        })
    }

    fn resolve(&self, path: &str, reference: &Reference) -> Destination {
        if let Some(issue) = self.issues.lookup(reference) {
            return Destination {
                issue: Some(issue),
                ..Destination::default()
            };
        }
        if native_issue(&reference.target).is_some() {
            return Destination::default();
        }
        let matches = candidates(path, reference, &self.notes);
        Destination {
            note: match matches.as_slice() {
                [only] => Some(*only),
                _ => None,
            },
            ..Destination::default()
        }
    }
}

/// Describe the unresolved links of imported notes after indexing and restoring
/// explicit choices. Preview calls this inside its rollback-only transaction.
pub async fn unresolved_warnings(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    sources: &[Uuid],
) -> AppResult<Vec<String>> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let mut pending=sqlx::query_as::<_,Pending>(
        "SELECT l.source_id,l.ordinal,n.path,l.raw,l.target,l.fragment,l.label,l.kind
         FROM note_links l JOIN notes n ON n.id=l.source_id AND n.owner_id=l.owner_id AND n.project_id=l.project_id
         WHERE l.owner_id=$1 AND l.project_id=$2 AND l.source_id=ANY($3)
         AND l.dest_note IS NULL AND l.dest_issue IS NULL ORDER BY n.path,l.ordinal LIMIT 101"
    ).bind(owner).bind(project).bind(sources).fetch_all(&mut **tx).await?;
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let notes = sqlx::query_as::<_, NoteInfo>(
        "SELECT id,path,title FROM notes WHERE owner_id=$1 AND project_id=$2 ORDER BY path,id",
    )
    .bind(owner)
    .bind(project)
    .fetch_all(&mut **tx)
    .await?;
    let paths: HashMap<_, _> = notes
        .iter()
        .map(|(id, path, _)| (*id, path.as_str()))
        .collect();
    let more = pending.len() > 100;
    pending.truncate(100);
    let mut warnings = Vec::with_capacity(pending.len() + usize::from(more));
    for mut row in pending {
        let path = std::mem::take(&mut row.path);
        let link = row.reference();
        let matches = if native_issue(&link.target).is_some() {
            Vec::new()
        } else {
            candidates(&path, &link, &notes)
        };
        let mut raw: String = link.raw.chars().take(240).collect();
        if raw.len() < link.raw.len() {
            raw.push('…');
        }
        let warning = if matches.len() > 1 {
            let mut list = matches
                .iter()
                .take(3)
                .filter_map(|id| paths.get(id).copied())
                .collect::<Vec<_>>()
                .join(", ");
            if matches.len() > 3 {
                list.push_str(&format!(" and {} more", matches.len() - 3));
            }
            format!(
                "{path} — ambiguous link “{raw}”. Choose a destination in Notes or write a full path ({list})."
            )
        } else {
            format!(
                "{path} — unresolved link “{raw}”. Check the target path or add the missing destination."
            )
        };
        warnings.push(warning);
    }
    if more {
        warnings.push("More unresolved links are available in Notes after import.".into());
    }
    Ok(warnings)
}

struct Indexed {
    source: Uuid,
    ordinal: i32,
    link: Reference,
    dest: Destination,
}

async fn replace_sources(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    ids: &[Uuid],
    catalog: &Catalog,
) -> AppResult<()> {
    let old = sqlx::query_as::<_, Previous>(
        "SELECT source_id,kind,raw,label,target,dest_note,dest_issue,manual FROM note_links
         WHERE owner_id=$1 AND project_id=$2 AND source_id=ANY($3) ORDER BY source_id,ordinal",
    )
    .bind(owner)
    .bind(project)
    .bind(ids)
    .fetch_all(&mut **tx)
    .await?;
    let mut by_source: HashMap<Uuid, Vec<Previous>> = HashMap::new();
    for link in old {
        by_source.entry(link.source_id).or_default().push(link);
    }
    let notes = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT id,path,body FROM notes WHERE owner_id=$1 AND project_id=$2 AND id=ANY($3)",
    )
    .bind(owner)
    .bind(project)
    .bind(ids)
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM note_links WHERE owner_id=$1 AND project_id=$2 AND source_id=ANY($3)")
        .bind(owner)
        .bind(project)
        .bind(ids)
        .execute(&mut **tx)
        .await?;

    let mut rows = Vec::new();
    for (source, path, body) in notes {
        let links = Document::parse(&body).links;
        let bindings = retain_bindings(by_source.remove(&source).unwrap_or_default(), &links);
        for (ordinal, (link, prior)) in links.into_iter().zip(bindings).enumerate() {
            let dest = prior.unwrap_or_else(|| catalog.resolve(&path, &link));
            let ordinal = i32::try_from(ordinal)
                .map_err(|_| AppError::BadRequest("This note contains too many links.".into()))?;
            rows.push(Indexed {
                source,
                ordinal,
                link,
                dest,
            });
            if rows.len() == 1000 {
                insert_rows(tx, owner, project, &rows).await?;
                rows.clear();
            }
        }
    }
    if !rows.is_empty() {
        insert_rows(tx, owner, project, &rows).await?;
    }
    Ok(())
}

async fn insert_rows(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    rows: &[Indexed],
) -> AppResult<()> {
    let mut q = QueryBuilder::<Postgres>::new(
        "INSERT INTO note_links(owner_id,project_id,source_id,ordinal,raw,target,fragment,label,kind,dest_note,dest_issue,manual) ",
    );
    q.push_values(rows, |mut values, row| {
        values
            .push_bind(owner)
            .push_bind(project)
            .push_bind(row.source)
            .push_bind(row.ordinal)
            .push_bind(&row.link.raw)
            .push_bind(&row.link.target)
            .push_bind(&row.link.fragment)
            .push_bind(&row.link.label)
            .push_bind(&row.link.kind)
            .push_bind(row.dest.note)
            .push_bind(row.dest.issue)
            .push_bind(row.dest.manual);
    });
    q.build().persistent(false).execute(&mut **tx).await?;
    Ok(())
}

/// Rebuild all parsed links after a project-locked batch import. Bound edge identities
/// and individual ambiguity choices survive renames, including simultaneous swaps.
pub async fn reindex(tx: &mut Tx<'_>, owner: Uuid, project: Uuid) -> AppResult<()> {
    let catalog = Catalog::load(tx, owner, project).await?;
    let ids: Vec<_> = catalog.notes.iter().map(|n| n.0).collect();
    for batch in ids.chunks(16) {
        replace_sources(tx, owner, project, batch, &catalog).await?;
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Pending {
    source_id: Uuid,
    ordinal: i32,
    path: String,
    raw: String,
    target: String,
    fragment: Option<String>,
    label: String,
    kind: String,
}
impl Pending {
    fn reference(self) -> Reference {
        Reference {
            raw: self.raw,
            target: self.target,
            fragment: self.fragment,
            label: self.label,
            kind: self.kind,
        }
    }
}

/// Refresh the authored note and retry older, unresolved references that its new
/// name or path may satisfy. Callers hold the containing project's write lock.
pub(super) async fn refresh_note(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    note: Uuid,
) -> AppResult<()> {
    let catalog = Catalog::load(tx, owner, project).await?;
    if !catalog.notes.iter().any(|info| info.0 == note) {
        return Err(AppError::NotFound);
    }
    replace_sources(tx, owner, project, &[note], &catalog).await?;
    resolve_pending(tx, owner, project, note, &catalog).await
}

pub(super) async fn refresh_after_delete(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    deleted: Uuid,
) -> AppResult<()> {
    let catalog = Catalog::load(tx, owner, project).await?;
    resolve_pending(tx, owner, project, deleted, &catalog).await
}

async fn resolve_pending(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    note: Uuid,
    catalog: &Catalog,
) -> AppResult<()> {
    let pending=sqlx::query_as::<_,Pending>(
        "SELECT l.source_id,l.ordinal,n.path,l.raw,l.target,l.fragment,l.label,l.kind
         FROM note_links l JOIN notes n ON n.id=l.source_id AND n.owner_id=l.owner_id AND n.project_id=l.project_id
         WHERE l.owner_id=$1 AND l.project_id=$2 AND l.source_id<>$3 AND l.dest_note IS NULL AND l.dest_issue IS NULL"
    ).bind(owner).bind(project).bind(note).fetch_all(&mut **tx).await?;
    let mut changed = Vec::new();
    for row in pending {
        let source = row.source_id;
        let ordinal = row.ordinal;
        let path = row.path.clone();
        let dest = catalog.resolve(&path, &row.reference());
        if dest.note.is_some() || dest.issue.is_some() {
            changed.push((source, ordinal, dest));
        }
    }
    for batch in changed.chunks(1000) {
        let mut q = QueryBuilder::<Postgres>::new(
            "UPDATE note_links AS l SET dest_note=v.note_id,dest_issue=v.issue_id,manual=false FROM (",
        );
        q.push_values(batch, |mut v, (source, ordinal, dest)| {
            v.push_bind(*source)
                .push_bind(*ordinal)
                .push_bind(dest.note)
                .push_bind(dest.issue);
        });
        q.push(") AS v(source_id,ordinal,note_id,issue_id) WHERE l.source_id=v.source_id AND l.ordinal=v.ordinal AND l.owner_id=")
            .push_bind(owner).push(" AND l.project_id=").push_bind(project);
        q.build().persistent(false).execute(&mut **tx).await?;
    }
    Ok(())
}

/// Resolve pending wikilinks and native issue URLs in the same transaction that
/// creates the issue. Existing note bindings always retain their chosen identity.
pub async fn refresh_issue(
    tx: &mut Tx<'_>,
    owner: Uuid,
    project: Uuid,
    issue: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE note_links l SET dest_issue=i.id,manual=false FROM issues i JOIN projects p
         ON p.id=i.project_id AND p.owner_id=i.owner_id
         WHERE i.owner_id=$1 AND i.project_id=$2 AND i.id=$3 AND l.owner_id=i.owner_id
         AND l.project_id=i.project_id AND l.dest_note IS NULL AND l.dest_issue IS NULL
         AND ((l.kind='wiki' AND lower(l.target)=lower(p.key || '-' || i.number::text))
              OR l.target='/issues/' || i.id::text)",
    )
    .bind(owner)
    .bind(project)
    .bind(issue)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wiki(target: &str, label: &str) -> Reference {
        Reference {
            raw: target.to_owned(),
            target: target.to_owned(),
            fragment: None,
            label: label.into(),
            kind: "wiki".into(),
        }
    }
    fn selected(target: &str, label: &str, note: Uuid) -> Previous {
        Previous {
            source_id: Uuid::nil(),
            kind: "wiki".into(),
            raw: target.into(),
            target: target.into(),
            label: label.into(),
            dest_note: Some(note),
            dest_issue: None,
            manual: true,
        }
    }
    #[test]
    fn manual_choices_survive_reorder_and_a_new_alias_remains_unbound() {
        let a = Uuid::from_u128(21);
        let b = Uuid::from_u128(42);
        let old = vec![
            selected("Idea", "original", a),
            selected("Idea", "second", b),
        ];
        let refs = vec![
            wiki("Idea", "new alias"),
            wiki("Idea", "second"),
            wiki("Idea", "original"),
        ];
        let result = retain_bindings(old, &refs);
        assert!(result[0].is_none());
        assert_eq!(result[1].and_then(|x| x.note), Some(b));
        assert_eq!(result[2].and_then(|x| x.note), Some(a));
        assert!(result[2].unwrap().manual);
    }
    #[test]
    fn renamed_target_is_retained_on_label_edit_but_changed_path_is_resolved_again() {
        let chosen = Uuid::from_u128(11);
        let old = vec![selected("Old", "before", chosen)];
        let refs = vec![wiki("New", "before"), wiki("Old", "changed")];
        let result = retain_bindings(old, &refs);
        assert!(result[0].is_none());
        assert_eq!(result[1].and_then(|x| x.note), Some(chosen));
    }
    #[test]
    fn issue_lookup_uses_canonical_keys_and_in_project_native_paths() {
        let id = Uuid::from_u128(27);
        let other = Uuid::from_u128(32);
        let catalog = IssueCatalog {
            key: "LU".into(),
            by_number: HashMap::from([(27, id)]),
            ids: [id].into_iter().collect(),
        };
        let test = |target: &str| catalog.lookup(&wiki(target, target));
        assert_eq!(test("lu-27"), Some(id));
        assert_eq!(test(&format!("/issues/{id}")), Some(id));
        for s in ["LU-027", "./LU-27", "Other-27", "LU-27.md", "LU-+27"] {
            assert_eq!(test(s), None, "{s}");
        }
        assert_eq!(test(&format!("/issues/{other}")), None);
        let mut local_file = wiki("LU-27", "local");
        local_file.kind = "markdown".into();
        assert_eq!(catalog.lookup(&local_file), None);
    }
    #[test]
    fn note_folders_can_be_named_issues() {
        let id = Uuid::new_v4();
        let catalog = Catalog {
            notes: vec![(id, "issues/design.md".into(), "Design".into())],
            issues: IssueCatalog {
                key: "LU".into(),
                by_number: HashMap::new(),
                ids: HashSet::new(),
            },
        };
        assert_eq!(
            catalog
                .resolve("readme.md", &wiki("/issues/design.md", "design"))
                .note,
            Some(id)
        );
    }
    #[sqlx::test]
    async fn refresh_and_import_reindex_keep_stable_project_scoped_edges(db: sqlx::PgPool) {
        let mut tx = db.begin().await.unwrap();
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('notes@example.test') RETURNING id",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES($1,'Garden','GR') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let outside: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES($1,'Outside','XT') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let foreign_issue:Uuid=sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title) VALUES($1,$2,7,'Private') RETURNING id")
            .bind(owner).bind(outside).fetch_one(&mut *tx).await.unwrap();
        let source:Uuid=sqlx::query_scalar("INSERT INTO notes(owner_id,project_id,path,title,body) VALUES($1,$2,'Home.md','Home',$3) RETURNING id")
            .bind(owner).bind(project).bind(format!("[[Decision]] [[GR-7]] [outside](/issues/{foreign_issue}) [[Pick]]")).fetch_one(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO notes(owner_id,project_id,path,title) VALUES($1,$2,'one/Pick.md','First'),($1,$2,'two/Pick.md','Second')")
            .bind(owner).bind(project).execute(&mut *tx).await.unwrap();
        reindex(&mut tx, owner, project).await.unwrap();
        let pending:i64=sqlx::query_scalar("SELECT count(*) FROM note_links WHERE source_id=$1 AND dest_note IS NULL AND dest_issue IS NULL")
            .bind(source).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(pending, 4);
        let preview = unresolved_warnings(&mut tx, owner, project, &[source])
            .await
            .unwrap();
        assert_eq!(preview.len(), 4);
        assert!(preview.iter().any(|w| w.contains("Home.md")
            && w.contains("Pick")
            && w.contains("one/Pick.md")
            && w.contains("two/Pick.md")));
        assert!(
            preview
                .iter()
                .any(|w| w.contains("Decision") && w.contains("unresolved"))
        );
        assert!(
            unresolved_warnings(&mut tx, owner, outside, &[source])
                .await
                .unwrap()
                .is_empty()
        );
        let pick: Uuid = sqlx::query_scalar(
            "SELECT id FROM notes WHERE owner_id=$1 AND project_id=$2 AND path='two/Pick.md'",
        )
        .bind(owner)
        .bind(project)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE note_links SET dest_note=$2,manual=true WHERE source_id=$1 AND target='Pick'",
        )
        .bind(source)
        .bind(pick)
        .execute(&mut *tx)
        .await
        .unwrap();
        let decision:Uuid=sqlx::query_scalar("INSERT INTO notes(owner_id,project_id,path,title) VALUES($1,$2,'Decision.md','Decision') RETURNING id")
            .bind(owner).bind(project).fetch_one(&mut *tx).await.unwrap();
        refresh_note(&mut tx, owner, project, decision)
            .await
            .unwrap();
        let issue:Uuid=sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title) VALUES($1,$2,7,'Repair') RETURNING id")
            .bind(owner).bind(project).fetch_one(&mut *tx).await.unwrap();
        refresh_issue(&mut tx, owner, project, issue).await.unwrap();
        let bindings=sqlx::query_as::<_,(Option<Uuid>,Option<Uuid>,bool)>("SELECT dest_note,dest_issue,manual FROM note_links WHERE source_id=$1 ORDER BY ordinal")
            .bind(source).fetch_all(&mut *tx).await.unwrap();
        assert_eq!(
            bindings,
            [
                (Some(decision), None, false),
                (None, Some(issue), false),
                (None, None, false),
                (Some(pick), None, true)
            ]
        );
        sqlx::query("UPDATE notes SET path='renamed.md',title='Changed' WHERE id=$1")
            .bind(decision)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE notes SET body='[[Pick]] [[Decision|decision renamed]] [[GR-7]]' WHERE id=$1",
        )
        .bind(source)
        .execute(&mut *tx)
        .await
        .unwrap();
        reindex(&mut tx, owner, project).await.unwrap();
        let imported=sqlx::query_as::<_,(Option<Uuid>,Option<Uuid>,bool)>("SELECT dest_note,dest_issue,manual FROM note_links WHERE source_id=$1 ORDER BY ordinal")
            .bind(source).fetch_all(&mut *tx).await.unwrap();
        assert_eq!(
            imported,
            [
                (Some(pick), None, true),
                (Some(decision), None, false),
                (None, Some(issue), false)
            ]
        );

        let noisy = (0..110)
            .map(|i| format!("[[Missing-{i}]]"))
            .collect::<Vec<_>>()
            .join(" ");
        sqlx::query("UPDATE notes SET body=$1 WHERE id=$2")
            .bind(noisy)
            .bind(source)
            .execute(&mut *tx)
            .await
            .unwrap();
        refresh_note(&mut tx, owner, project, source).await.unwrap();
        let bounded = unresolved_warnings(&mut tx, owner, project, &[source])
            .await
            .unwrap();
        assert_eq!(bounded.len(), 101);
        assert!(bounded.last().unwrap().contains("More unresolved links"));
        assert!(bounded[99].contains("Missing-99"));
    }
}
