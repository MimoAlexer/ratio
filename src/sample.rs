use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{AppError, AppResult};

type Tx<'a> = Transaction<'a, Postgres>;
type Time = DateTime<Utc>;

const fn id(slot: u64) -> Uuid {
    Uuid::from_u128(0x173266c4_a3c1_4ccc_ac46_000000000000 + slot as u128)
}

pub const DEMO_OWNER: Uuid = id(1);
pub const DEMO_EMAIL: &str = "demo@workspace.test";
pub const DEMO_PROJECT: Uuid = id(2);
const PREVIOUS: Uuid = id(3);
const CURRENT: Uuid = id(4);
const VAULT_CANVAS: &str = include_str!("../migrations/0029_demo_vault_canvas.sql");

fn issue_id(number: i64) -> Uuid {
    id(100 + number as u64)
}

fn milestone_id(number: u64) -> Uuid {
    id(200 + number)
}

/// Install once in an atomically claimed demo identity. Seeing that identity again
/// means the original transaction committed, even when its owner later deleted Luma.
pub async fn seed(db: &PgPool) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let now: Time = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *tx)
        .await?;
    let claimed = sqlx::query(
        "INSERT INTO users (id,email,created_at)
         SELECT $1,$2,$3 WHERE NOT EXISTS
            (SELECT FROM users WHERE lower(email)=lower($2))
          AND NOT EXISTS (SELECT FROM account_deletions WHERE user_id=$1)
         ON CONFLICT DO NOTHING",
    )
    .bind(DEMO_OWNER)
    .bind(DEMO_EMAIL)
    .bind(now - Duration::days(42))
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() == 0 {
        let matching: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT FROM users WHERE id=$1 AND email=$2)
               AND NOT EXISTS(SELECT FROM users WHERE id<>$1 AND lower(email)=lower($2))",
        )
        .bind(DEMO_OWNER)
        .bind(DEMO_EMAIL)
        .fetch_one(&mut *tx)
        .await?;
        if !matching {
            return Err(AppError::Conflict(
                "The reserved demo identity is in use or was deleted.".into(),
            ));
        }
        tx.commit().await?;
        return Ok(());
    }

    let project = sqlx::query(
        "INSERT INTO projects(id,owner_id,key,name,created_at)
         VALUES($1,$2,'LU','Luma',$3) ON CONFLICT DO NOTHING",
    )
    .bind(DEMO_PROJECT)
    .bind(DEMO_OWNER)
    .bind(now - Duration::days(40))
    .execute(&mut *tx)
    .await?;
    if project.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "The reserved demo project ID is in use.".into(),
        ));
    }
    for issue in ISSUES {
        add_issue(&mut tx, now, issue).await?;
    }
    add_notes(&mut tx, now).await?;
    add_releases(&mut tx, now).await?;
    add_milestones(&mut tx, now).await?;
    sqlx::query(VAULT_CANVAS).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

// Times are offsets in whole days from one database clock reading. Each issue lists
// its actual transitions so reopening, cycle time and release scope use the same history.
struct ExampleIssue {
    number: i64,
    title: &'static str,
    kind: &'static str,
    history: &'static [(i64, &'static str)],
    next: &'static str,
}

const fn example(
    number: i64,
    title: &'static str,
    kind: &'static str,
    history: &'static [(i64, &'static str)],
    next: &'static str,
) -> ExampleIssue {
    ExampleIssue {
        number,
        title,
        kind,
        history,
        next,
    }
}

#[rustfmt::skip]
const ISSUES: &[ExampleIssue] = &[
    example(18, "Index a vault on first open", "feature",
        &[(-36,"ready"),(-31,"in_progress"),(-26,"done")], ""),
    example(19, "Keep note IDs stable across renames", "bug",
        &[(-35,"ready"),(-28,"in_progress"),(-25,"done")], ""),
    example(20, "Skip unchanged files on rescan", "maintenance",
        &[(-31,"ready"),(-26,"in_progress"),(-22,"done")], ""),
    example(21, "Preserve heading links on export", "bug",
        &[(-30,"ready"),(-25,"in_progress"),(-21,"done")], ""),
    example(22, "Show indexing progress for large vaults", "feature",
        &[(-28,"ready"),(-24,"in_progress"),(-17,"done")], ""),
    example(23, "Read the previous vault format", "feature",
        &[(-19,"ready"),(-15,"in_progress"),(-13,"done")], ""),
    example(24, "Copy attachments during migration", "feature",
        &[(-17,"ready"),(-14,"in_progress"),(-10,"done")], ""),
    example(25, "Report interrupted migrations", "bug",
        &[(-16,"ready"),(-11,"in_progress"),(-6,"done")], ""),
    example(26, "Restore the vault after a canceled upgrade", "maintenance",
        &[(-15,"ready"),(-10,"in_progress"),(-4,"done")], ""),
    example(27, "Verify migrated vaults on reopen", "maintenance",
        &[(-26,"ready"),(-8,"in_progress")], "Reopen the migrated fixture and compare the original note IDs."),
    example(28, "Keep aliases when importing notes", "feature",
        &[(-15,"ready"),(-9,"in_progress"),(-7,"done")], ""),
    example(29, "Show missing attachments in the import report", "bug",
        &[(-12,"ready"),(-8,"in_progress"),(-5,"done"),(-3,"in_progress"),(-1,"done")], ""),
    example(30, "Add a read-only vault preview", "feature",
        &[(-11,"backlog")], "Revisit after the public beta."),
    example(31, "Document the recovery steps", "maintenance",
        &[(-9,"ready"),(-6,"in_progress"),(-2,"done")], ""),
    example(32, "Rebuild stale backlinks", "bug",
        &[(-11,"ready"),(-5,"in_progress")], "Rename the linked note twice and verify the original backlink disappears."),
    example(33, "Prepare the beta invitation", "feature",
        &[(-8,"ready")], "Check the invitation against the beta entry criteria."),
    example(34, "Reject unsafe paths in archive imports", "security",
        &[(-3,"ready")], "Review the archive validation change and rerun the import checks."),
    example(35, "Run the public beta acceptance checks", "maintenance",
        &[(-6,"backlog"),(-2,"ready")], "Once storage and archive checks pass, sign off the beta checklist."),
    example(36, "Publish the release notes", "feature",
        &[(-5,"backlog")], "Collect the beta feedback before writing the final notes."),
];

async fn add_issue(tx: &mut Tx<'_>, now: Time, issue: &ExampleIssue) -> AppResult<()> {
    let issue_id = issue_id(issue.number);
    let critical = issue.number == 34;
    let resumed = issue.number == 32;
    let body = match issue.number {
        27 => {
            "Compare the note identifiers, attachments and backlinks before and after a full shutdown."
        }
        32 => "A second note rename leaves an obsolete backlink in the incremental index.",
        34 => {
            "Manual security review found an import validation gap. The beta depends on checking the archive fix."
        }
        35 => {
            "The beta opens when migration verification and archive validation are both complete."
        }
        _ => "",
    };
    let priority = match issue.number {
        34 => "urgent",
        27 | 32 | 35 => "high",
        _ => "normal",
    };
    let severity = if critical { "critical" } else { "none" };
    let (first_day, initial) = issue.history[0];
    let &(last_day, status) = issue
        .history
        .last()
        .expect("demo history starts with creation");
    let created = now + Duration::days(first_day);
    let started = issue
        .history
        .iter()
        .find(|(_, s)| *s == "in_progress")
        .map(|(day, _)| now + Duration::days(*day));
    let completed = (status == "done").then(|| now + Duration::days(last_day));
    let checkpoint = resumed.then(|| now - Duration::days(1));
    let revision = issue.history.len() as i64 + i64::from(resumed);
    sqlx::query(
        "INSERT INTO issues(id,owner_id,project_id,number,title,body,kind,priority,manual_severity,
                            security_critical,next_action,checkpoint_at,revision,status,created_at,
                            started_at,completed_at,updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)",
    )
    .bind(issue_id)
    .bind(DEMO_OWNER)
    .bind(DEMO_PROJECT)
    .bind(issue.number)
    .bind(issue.title)
    .bind(body)
    .bind(issue.kind)
    .bind(priority)
    .bind(severity)
    .bind(critical)
    .bind(issue.next)
    .bind(checkpoint)
    .bind(revision)
    .bind(status)
    .bind(created)
    .bind(started)
    .bind(completed)
    .bind(checkpoint.unwrap_or(now + Duration::days(last_day)))
    .execute(&mut **tx)
    .await?;

    let creation = json!({"fields": {
        "title":issue.title, "body":body, "status":initial, "priority":priority, "kind":issue.kind,
        "labels":[], "next_action":if resumed {""} else {issue.next}, "manual_severity":severity,
        "finding_closed":false, "archived":false, "checkpoint_at":null
    }});
    let mut prior = None;
    let mut history = Vec::new();
    for (index, &(day, state)) in issue.history.iter().enumerate() {
        let before = prior.map(|status: &str| json!({"fields":{"status":status}}));
        let after = if index == 0 {
            creation.clone()
        } else {
            json!({"fields":{"status":state}})
        };
        history.push((
            now + Duration::days(day),
            prior,
            state,
            if prior.is_none() { "create" } else { "status" },
            before,
            after,
        ));
        prior = Some(state);
    }
    if let Some(at) = checkpoint {
        history.push((
            at,
            prior,
            status,
            "checkpoint",
            Some(json!({"fields":{"checkpoint_at": null,"next_action":""}})),
            json!({"fields":{"checkpoint_at":at,"next_action":issue.next}}),
        ));
    }
    for (index, (at, old, new, action, before, after)) in history.into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO issue_events(id,owner_id,project_id,issue_id,old_status,new_status,action,
                                      before_state,after_state,revision_after,occurred_at,ingested_at)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$11)",
        )
        .bind(id(1000 + issue.number as u64 * 10 + index as u64))
        .bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(issue_id).bind(old).bind(new).bind(action)
        .bind(before).bind(after).bind(index as i64 + 1).bind(at).execute(&mut **tx).await?;
    }
    Ok(())
}

async fn add_notes(tx: &mut Tx<'_>, now: Time) -> AppResult<()> {
    const EXCERPT: &str =
        "Rebuild outgoing links after a rename so a note no longer appears in its old backlinks.";
    let decisions = format!(
        "# Link resolution\n\n{EXCERPT} ^rename-check\n\nThe two-rename regression is tracked in [[LU-32]]. Check the [[Vault migration]] fixtures before shipping it."
    );
    let notes = [
        (
            id(11),
            "Link resolution",
            "decisions/link-resolution.md",
            decisions.as_str(),
            -19,
        ),
        (
            id(12),
            "Vault migration",
            "engineering/vault-migration.md",
            "# Reopen verification\n\nRecord the original IDs and attachment count, migrate a copy, then reopen it twice. [[LU-27]] tracks the comparison.\n\nSee [[Link resolution]] for the linked-note case.",
            -25,
        ),
        (
            id(13),
            "Public beta checklist",
            "releases/beta-checklist.md",
            "# Entry criteria\n\nComplete [[Vault migration]] and inspect the manual archive finding in [[LU-34]].\n\nWhen both pass, run [[LU-35]] and collect feedback before publishing.",
            -2,
        ),
    ];
    for (note, title, path, body, day) in notes {
        let revision = if note == id(11) { 2_i64 } else { 1 };
        let updated = if revision == 2 { -5 } else { day };
        sqlx::query(
            "INSERT INTO notes(id,owner_id,project_id,title,path,body,created_at,updated_at,revision)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        ).bind(note).bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(title).bind(path).bind(body)
        .bind(now + Duration::days(day)).bind(now + Duration::days(updated)).bind(revision)
        .execute(&mut **tx).await?;
    }
    sqlx::query(
        "INSERT INTO note_issue_sources(owner_id,project_id,note_id,issue_id,source_anchor,excerpt,note_revision)
         VALUES($1,$2,$3,$4,'block-rename-check',$5,1)",
    ).bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(id(11)).bind(issue_id(32)).bind(EXCERPT)
    .execute(&mut **tx).await?;
    crate::notes::reindex(tx, DEMO_OWNER, DEMO_PROJECT).await?;
    Ok(())
}
async fn add_releases(tx: &mut Tx<'_>, now: Time) -> AppResult<()> {
    let published = now - Duration::days(16);
    for (release, name, version, created, target) in [
        (
            PREVIOUS,
            "Incremental indexing",
            "1.3",
            now - Duration::days(38),
            published,
        ),
        (
            CURRENT,
            "Public beta",
            "1.4",
            published,
            now + Duration::days(17),
        ),
    ] {
        sqlx::query(
            "INSERT INTO releases(id,owner_id,project_id,name,version,created_at,target_on)
             VALUES($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(release)
        .bind(DEMO_OWNER)
        .bind(DEMO_PROJECT)
        .bind(name)
        .bind(version)
        .bind(created)
        .bind(target.date_naive())
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO roadmap_release_spans(owner_id,project_id,release_id,start_on,end_on)
         SELECT owner_id,project_id,id,(created_at AT TIME ZONE 'UTC')::date,target_on
         FROM releases WHERE owner_id=$1 AND project_id=$2 AND id=$3 AND published_at IS NULL",
    )
    .bind(DEMO_OWNER)
    .bind(DEMO_PROJECT)
    .bind(CURRENT)
    .execute(&mut **tx)
    .await?;

    // Previous scope includes five shipments and one open verification carried to 1.4.
    // In the current draft the read-only preview was explicitly removed after planning.
    for (index, (release, number, day, in_scope)) in [
        (PREVIOUS, 18, -35, true),
        (PREVIOUS, 19, -34, true),
        (PREVIOUS, 20, -30, true),
        (PREVIOUS, 21, -29, true),
        (PREVIOUS, 22, -27, true),
        (PREVIOUS, 27, -24, true),
        (CURRENT, 27, -16, true),
        (CURRENT, 23, -16, true),
        (CURRENT, 24, -16, true),
        (CURRENT, 25, -15, true),
        (CURRENT, 26, -14, true),
        (CURRENT, 28, -14, true),
        (CURRENT, 29, -11, true),
        (CURRENT, 30, -10, true),
        (CURRENT, 31, -8, true),
        (CURRENT, 32, -9, true),
        (CURRENT, 33, -6, true),
        (CURRENT, 30, -3, false),
        (CURRENT, 34, -2, true),
        (CURRENT, 35, -5, true),
        (CURRENT, 36, -4, true),
    ]
    .into_iter()
    .enumerate()
    {
        let carry = release == CURRENT && number == 27;
        let at = now
            + Duration::days(day)
            + if carry {
                Duration::zero()
            } else {
                Duration::minutes(30)
            };
        sqlx::query(
            "INSERT INTO release_membership_events(id,owner_id,project_id,release_id,issue_id,in_scope,status,occurred_at,source)
             VALUES($1,$2,$3,$4,$5,$6,
                 (SELECT new_status FROM issue_events WHERE issue_id=$5 AND occurred_at<=$7
                  ORDER BY occurred_at DESC,revision_after DESC LIMIT 1),$7,$8)",
        ).bind(id(2000 + index as u64)).bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(release)
        .bind(issue_id(number)).bind(in_scope).bind(at).bind(if carry {"carryover"} else {"assignment"})
        .execute(&mut **tx).await?;
    }
    sqlx::query(
        "INSERT INTO release_members(owner_id,project_id,release_id,issue_id)
         SELECT owner_id,project_id,release_id,issue_id
         FROM (SELECT DISTINCT ON(issue_id) * FROM release_membership_events
               WHERE owner_id=$1 AND release_id=$2 ORDER BY issue_id,occurred_at DESC,id DESC) latest
         WHERE in_scope",
    ).bind(DEMO_OWNER).bind(CURRENT).execute(&mut **tx).await?;
    sqlx::query(
        "INSERT INTO release_entries(owner_id,project_id,release_id,issue_id,number,title,issue_revision)
         SELECT owner_id,project_id,$3,id,number,title,revision FROM issues
         WHERE owner_id=$1 AND project_id=$2 AND number BETWEEN 18 AND 22",
    ).bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(PREVIOUS).execute(&mut **tx).await?;
    sqlx::query(
        "INSERT INTO release_carryovers(owner_id,project_id,release_id,issue_id,number,title,status,issue_revision,next_release_id)
         SELECT i.owner_id,i.project_id,$2,i.id,i.number,i.title,e.new_status,e.revision_after,$3
         FROM issues i JOIN LATERAL
            (SELECT new_status,revision_after FROM issue_events WHERE issue_id=i.id AND occurred_at<=$4
             ORDER BY occurred_at DESC,revision_after DESC LIMIT 1) e ON true
         WHERE i.id=$1",
    ).bind(issue_id(27)).bind(PREVIOUS).bind(CURRENT).bind(published).execute(&mut **tx).await?;
    sqlx::query(
        "UPDATE releases r SET revision=1+
           (SELECT count(*) FROM release_membership_events e WHERE e.release_id=r.id AND source='assignment')
         WHERE r.owner_id=$1 AND r.project_id=$2",
    ).bind(DEMO_OWNER).bind(DEMO_PROJECT).execute(&mut **tx).await?;
    sqlx::query(
        "UPDATE releases SET published_on=$2,published_at=$3,publish_command=$4,revision=revision+1
         WHERE id=$1 AND published_at IS NULL",
    )
    .bind(PREVIOUS)
    .bind(published.date_naive())
    .bind(published)
    .bind(id(5))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn add_milestones(tx: &mut Tx<'_>, now: Time) -> AppResult<()> {
    let today = now.date_naive();
    let milestones = [
        (
            1,
            "Incremental index",
            "Rebuild only changed notes and keep stable IDs across renames.",
            "reached",
            -29,
            -16,
            &[18, 19, 20, 21, 22][..],
        ),
        (
            2,
            "Local storage migration",
            "Reopen a migrated vault and compare IDs and attachments.",
            "in_progress",
            -18,
            4,
            &[23, 24, 25, 26, 27][..],
        ),
        (
            3,
            "Archive dependency",
            "Resolve the manually flagged archive validation issue before beta.",
            "in_progress",
            -3,
            2,
            &[34][..],
        ),
        (
            4,
            "Public beta",
            "Invite beta testers after storage verification and archive checks pass.",
            "planned",
            -4,
            10,
            &[32, 33, 35][..],
        ),
        (
            5,
            "v1.4 release",
            "Publish the release notes after beta acceptance.",
            "planned",
            4,
            17,
            &[36][..],
        ),
    ];
    for (number, title, details, status, from, to, members) in milestones {
        let milestone = milestone_id(number);
        sqlx::query(
            "INSERT INTO milestones(id,owner_id,project_id,title,details,status,start_on,end_on,target_on,created_at,updated_at)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$8,$9,$10)",
        ).bind(milestone).bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(title).bind(details)
        .bind(status).bind(today + Duration::days(from)).bind(today + Duration::days(to))
        .bind(now - Duration::days(32)).bind(now + Duration::days(to.min(-1))).execute(&mut **tx).await?;
        for &issue in members {
            sqlx::query(
                "INSERT INTO milestone_issues(owner_id,project_id,milestone_id,issue_id) VALUES($1,$2,$3,$4)",
            ).bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(milestone).bind(issue_id(issue)).execute(&mut **tx).await?;
        }
    }
    for (prerequisite, dependent) in [(1, 2), (2, 4), (3, 4), (4, 5)] {
        sqlx::query(
            "INSERT INTO milestone_dependencies(owner_id,project_id,prerequisite_id,dependent_id) VALUES($1,$2,$3,$4)",
        ).bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(milestone_id(prerequisite))
        .bind(milestone_id(dependent)).execute(&mut **tx).await?;
    }
    for (milestone, release) in [(1, PREVIOUS), (5, CURRENT)] {
        sqlx::query(
            "INSERT INTO milestone_releases(owner_id,project_id,milestone_id,release_id) VALUES($1,$2,$3,$4)",
        ).bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(milestone_id(milestone)).bind(release)
        .execute(&mut **tx).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn vault_map_upgrades_an_existing_demo_once_and_preserves_edits(db: PgPool) {
        seed(&db).await.unwrap();
        let board = id(0x1000);
        let links: (String, i32, i32, i64) = sqlx::query_as(
            "SELECT title, jsonb_array_length(document->'nodes'), jsonb_array_length(document->'edges'),
                (SELECT count(*) FROM jsonb_array_elements(document->'nodes') n WHERE n ? 'source')
             FROM canvas_boards WHERE owner_id=$1 AND project_id=$2 AND id=$3",
        )
        .bind(DEMO_OWNER).bind(DEMO_PROJECT).bind(board).fetch_one(&db).await.unwrap();
        assert_eq!(links, ("Vault indexing".into(), 6, 5, 5));

        // Simulate the moment before migration 0029 on an older sample where
        // the developer has already deleted one of the source notes.
        sqlx::query("DELETE FROM canvas_boards WHERE id=$1")
            .bind(board)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM notes WHERE id=$1")
            .bind(id(11))
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(VAULT_CANVAS).execute(&db).await.unwrap();
        let pinned: Vec<Uuid> = sqlx::query_scalar(
            "SELECT (node->'source'->>'id')::uuid FROM canvas_boards,
                    jsonb_array_elements(document->'nodes') node
             WHERE id=$1 AND node ? 'source'",
        )
        .bind(board)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(pinned.len(), 4);
        assert!(pinned.contains(&issue_id(32)) && !pinned.contains(&id(11)));

        sqlx::query("UPDATE canvas_boards SET title='My indexing plan' WHERE id=$1")
            .bind(board)
            .execute(&db)
            .await
            .unwrap();
        seed(&db).await.unwrap();
        let title: String = sqlx::query_scalar("SELECT title FROM canvas_boards WHERE id=$1")
            .bind(board)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(title, "My indexing plan");
        sqlx::query("DELETE FROM canvas_boards WHERE id=$1")
            .bind(board)
            .execute(&db)
            .await
            .unwrap();
        seed(&db).await.unwrap();
        let boards: i64 = sqlx::query_scalar("SELECT count(*) FROM canvas_boards WHERE id=$1")
            .bind(board)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(boards, 0);
    }

    #[sqlx::test]
    async fn install_builds_connected_history_and_respects_later_changes(db: PgPool) {
        let (first, second) = tokio::join!(seed(&db), seed(&db));
        first.unwrap();
        second.unwrap();
        let project: Uuid = sqlx::query_scalar(
            "SELECT p.id FROM projects p JOIN users u ON p.owner_id=u.id
             WHERE u.email=$1 AND u.plan='free' AND p.key='LU'",
        )
        .bind(DEMO_EMAIL)
        .fetch_one(&db)
        .await
        .unwrap();

        let (notes, sources, links, milestones, dependencies, critical, published, drafts):
            (i64,i64,i64,i64,i64,i64,i64,i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM notes WHERE project_id=$1),
                    (SELECT count(*) FROM note_issue_sources WHERE project_id=$1),
                    (SELECT count(*) FROM note_links WHERE project_id=$1 AND dest_note IS NOT NULL),
                    (SELECT count(*) FROM milestones WHERE project_id=$1),
                    (SELECT count(*) FROM milestone_dependencies WHERE project_id=$1),
                    (SELECT count(*) FROM issues WHERE project_id=$1 AND security_critical),
                    (SELECT count(*) FROM releases WHERE project_id=$1 AND published_at IS NOT NULL),
                    (SELECT count(*) FROM releases WHERE project_id=$1 AND published_at IS NULL)",
        ).bind(project).fetch_one(&db).await.unwrap();
        assert!((2..=3).contains(&notes) && sources >= 1 && links >= 1);
        assert_eq!((milestones, critical, published, drafts), (5, 1, 1, 1));
        assert!(dependencies >= 2);
        let draft_spans: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM releases r JOIN roadmap_release_spans s ON s.release_id=r.id
             WHERE r.project_id=$1 AND r.published_at IS NULL
               AND s.start_on<=(now() AT TIME ZONE 'UTC')::date
               AND s.end_on=r.target_on AND s.end_on>(now() AT TIME ZONE 'UTC')::date",
        )
        .bind(project)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(draft_spans, 1);
        let (stale, weeks, shipments, open, done): (i64,i64,i64,i64,i64) = sqlx::query_as(
            "SELECT
               (SELECT count(*) FROM issues i WHERE i.project_id=$1 AND (i.status,i.revision)
                 IS DISTINCT FROM (SELECT ROW(e.new_status,e.revision_after) FROM issue_events e
                   WHERE e.issue_id=i.id ORDER BY e.occurred_at DESC,e.revision_after DESC LIMIT 1)),
               (SELECT count(DISTINCT date_trunc('week',occurred_at)) FROM issue_events WHERE project_id=$1),
               (SELECT count(*) FROM release_entries WHERE project_id=$1),
               (SELECT count(*) FROM release_members m JOIN issues i ON i.id=m.issue_id
                 JOIN releases r ON r.id=m.release_id WHERE m.project_id=$1 AND r.published_at IS NULL AND i.status='in_progress'),
               (SELECT count(*) FROM release_members m JOIN issues i ON i.id=m.issue_id
                 JOIN releases r ON r.id=m.release_id WHERE m.project_id=$1 AND r.published_at IS NULL AND i.status='done')",
        ).bind(project).fetch_one(&db).await.unwrap();
        assert_eq!(stale, 0);
        assert!(weeks >= 3 && shipments >= 1 && open >= 1 && done >= 1);

        sqlx::query("UPDATE projects SET name='My edited project' WHERE id=$1")
            .bind(project)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM notes WHERE project_id=$1")
            .bind(project)
            .execute(&db)
            .await
            .unwrap();
        seed(&db).await.unwrap();
        let after: (String,i64) = sqlx::query_as(
            "SELECT name,(SELECT count(*) FROM notes WHERE project_id=$1) FROM projects WHERE id=$1",
        ).bind(project).fetch_one(&db).await.unwrap();
        assert_eq!(after, ("My edited project".to_string(), 0));
    }

    #[sqlx::test]
    async fn reserved_email_on_an_existing_personal_account_is_never_claimed(db: PgPool) {
        let personal: Uuid = sqlx::query_scalar("INSERT INTO users(email) VALUES($1) RETURNING id")
            .bind(DEMO_EMAIL)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(matches!(seed(&db).await, Err(crate::AppError::Conflict(_))));
        let untouched: (Uuid, i64) =
            sqlx::query_as("SELECT id,(SELECT count(*) FROM projects) FROM users WHERE email=$1")
                .bind(DEMO_EMAIL)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(untouched, (personal, 0));
    }

    #[sqlx::test]
    async fn collision_with_a_personal_uuid_rolls_back_the_entire_seed(db: PgPool) {
        let personal: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES ('owner@workspace.test') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO projects(id,owner_id,name,key) VALUES($1,$2,'Personal','P')")
            .bind(Uuid::from_u128(0x173266c4_a3c1_4ccc_ac46_000000000002))
            .bind(personal)
            .execute(&db)
            .await
            .unwrap();
        assert!(seed(&db).await.is_err());
        let after: (i64,i64,i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM users),(SELECT count(*) FROM projects),(SELECT count(*) FROM issues)",
        ).fetch_one(&db).await.unwrap();
        assert_eq!(after, (1, 1, 0));
    }
}
