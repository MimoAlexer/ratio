use axum::{
    extract::{Path, Query, State},
    response::Html,
};
use chrono::{DateTime, NaiveDate, Utc};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::FromRow;
use uuid::Uuid;

use super::{forecast, history, issue_link, plan_today, status_label, valid_release_url};
use crate::{AppError, AppResult, AppState, auth::Auth, store, ui};

#[derive(Deserialize, Default)]
pub(super) struct DetailQuery {
    week: Option<NaiveDate>,
    #[serde(default)]
    search: String,
    pace: Option<u16>,
    #[serde(default)]
    model: bool,
}

#[derive(FromRow)]
struct Release {
    id: Uuid,
    project_id: Uuid,
    name: String,
    version: String,
    source_url: String,
    target_on: Option<NaiveDate>,
    published_on: Option<NaiveDate>,
    revision: i64,
}

#[derive(FromRow)]
struct Member {
    id: Uuid,
    number: i64,
    title: String,
    status: String,
    revision: i64,
    archived: bool,
    next_action: String,
    shipped_before: bool,
}

#[derive(FromRow)]
struct Candidate {
    id: Uuid,
    number: i64,
    title: String,
    status: String,
    draft_id: Option<Uuid>,
    draft_name: Option<String>,
}

#[derive(FromRow)]
struct Shipped {
    issue_id: Uuid,
    number: i64,
    title: String,
    issue_revision: i64,
    first_id: Uuid,
    first_name: String,
    first_on: NaiveDate,
}

#[derive(FromRow)]
struct Carried {
    issue_id: Uuid,
    number: i64,
    title: String,
    status: String,
    next_release_id: Option<Uuid>,
    next_name: Option<String>,
    current_release_id: Option<Uuid>,
    current_name: Option<String>,
    current_title: String,
    current_status: String,
    revision: i64,
    eligible: bool,
}

#[derive(FromRow)]
struct Amendment {
    issue_id: Uuid,
    number: i64,
    title: String,
    reason: String,
    replacement: String,
    created_at: DateTime<Utc>,
    email: String,
}

pub(super) async fn show(
    auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<DetailQuery>,
) -> AppResult<Html<String>> {
    let release: Release = sqlx::query_as("SELECT id,project_id,name,version,source_url,target_on,published_on,revision FROM releases WHERE owner_id=$1 AND id=$2")
        .bind(auth.user_id).bind(id).fetch_optional(&state.db).await?.ok_or(AppError::NotFound)?;
    let project = store::project(&state.db, auth.user_id, release.project_id).await?;
    let (plan, today) = plan_today(&state.db, auth.user_id).await?;
    let is_draft = release.published_on.is_none();
    let (overview, records) = if is_draft {
        let members: Vec<Member> = sqlx::query_as(
            "SELECT i.id,i.number,i.title,i.status,i.revision,i.archived,i.next_action,
                EXISTS (SELECT 1 FROM release_entries e WHERE e.owner_id=i.owner_id AND e.issue_id=i.id) AS shipped_before
             FROM release_members m JOIN issues i ON i.id=m.issue_id
             WHERE m.release_id=$1 AND m.owner_id=$2 ORDER BY i.number"
        ).bind(id).bind(auth.user_id).fetch_all(&state.db).await?;
        let search = query.search.trim();
        if search.contains('\0') || search.chars().count() > 200 {
            return Err(AppError::BadRequest("Search up to 200 characters.".into()));
        }
        let candidates: Vec<Candidate> = if !project.archived {
            sqlx::query_as(
                "SELECT i.id,i.number,i.title,i.status,r.id AS draft_id,r.name AS draft_name
                 FROM issues i JOIN projects p ON p.id=i.project_id
                 LEFT JOIN release_members m ON m.issue_id=i.id
                 LEFT JOIN releases r ON r.id=m.release_id
                 WHERE i.owner_id=$1 AND i.project_id=$2 AND NOT i.archived AND m.release_id IS DISTINCT FROM $3
                    AND (strpos(lower(i.title),lower($4))>0 OR lower(p.key || '-' || i.number::text)=lower($4))
                 ORDER BY i.updated_at DESC,i.id LIMIT 21"
            ).bind(auth.user_id).bind(project.id).bind(id).bind(search).fetch_all(&state.db).await?
        } else {
            vec![]
        };
        (
            html! {
                (preview(&auth,&release,&project,&members,today))
                @if !project.archived { (edit_scope(&auth,id,&project.key,&members,&candidates,search)) }
            },
            html! {},
        )
    } else {
        manifest(&state, &auth, &project, &release).await?
    };
    let history = history::history(&state.db, auth.user_id, id, query.week, &plan).await?;
    let outlook = if is_draft {
        forecast::forecast(&state.db, auth.user_id, id, query.pace, query.model).await?
    } else {
        html! {}
    };
    Ok(ui::page_in_project(
        &auth,
        &release.name,
        "releases",
        html! {
            header.view-head.release-detail-head {
                div {
                    h1 { (release.name) }
                    p.release-metadata {
                        span { (project.name) }
                        @if is_draft { span.release-stage { "Draft" } }
                        @if !release.version.is_empty() { span.release-version { (release.version) } }
                        @if let Some(target) = release.target_on && release.published_on != Some(target) {
                            span { "Target " time datetime=(target) { (target.format("%d %b %Y")) } }
                        }
                        @if let Some(date) = release.published_on {
                            span {
                                (if release.target_on == Some(date) { "Published on target · " } else { "Published " })
                                time datetime=(date) { (date.format("%d %b %Y")) }
                            }
                        }
                        @if !release.source_url.is_empty() && valid_release_url(&release.source_url) {
                            a href=(&release.source_url) rel="noreferrer" { "GitHub release ↗" }
                        }
                    }
                }
                nav.view-actions.release-related aria-label="Related project views" {
                    a.action-quiet href=(format!("/roadmap?project={}",project.id)) { (ui::icon("roadmap")) "Roadmap" }
                    a.action-quiet href=(format!("/insights?project={}",project.id)) { (ui::icon("insights")) "Cycle time" }
                }
            }
            @if project.archived { p.status-note { "Archived project · read only" } }
            (overview)
            (history)
            (records)
            (outlook)
        },
        Some(project.id),
    ))
}

fn preview(
    auth: &Auth,
    release: &Release,
    project: &store::Project,
    members: &[Member],
    today: NaiveDate,
) -> Markup {
    let eligible: Vec<_> = members
        .iter()
        .filter(|i| i.status == "done" && !i.archived)
        .collect();
    let open: Vec<_> = members
        .iter()
        .filter(|i| matches!(i.status.as_str(), "backlog" | "ready" | "in_progress") && !i.archived)
        .collect();
    let excluded: Vec<_> = members
        .iter()
        .filter(|i| i.status == "canceled" || i.archived)
        .collect();
    let active_issue = open
        .iter()
        .find(|i| i.status == "in_progress" && !i.next_action.is_empty());
    html! {
        section.release-preview.content-section id="manifest" {
            div.release-snapshot {
                div {
                    h2 { "Publication preview" }
                    @if !members.is_empty() {
                        p.release-ready { strong { (eligible.len()) } " ready to ship" }
                        p.release-meta { (open.len()) " open · " (members.len()) " in scope" @if !excluded.is_empty() { " · " (excluded.len()) " archived or canceled" } }
                    }
                }
                @if let Some(i) = active_issue {
                    div.release-current {
                        span.release-meta { "In progress" }
                        a.release-current-issue href=(format!("/issues/{}",i.id)) {
                            span.release-key { (&project.key) "-" (i.number) } span { (i.title) }
                        }
                        p.release-meta { (i.next_action) }
                    }
                }
            }
            @if members.is_empty() {
                p.muted { "Add issues to this draft to build its manifest." }
            } @else if project.archived {
                ul class="issue-list" { @for i in members { li { (issue_link(i.id,&project.key,i.number,&i.title)) " · " (status_label(&i.status)) } } }
            } @else {
                details.release-publish {
                    summary { (if eligible.is_empty() { "Review draft issues" } else { "Review shipment" }) }
                    form action=(format!("/releases/{}/publish",release.id)) method="post" class="stack release-shipment" {
                        (ui::csrf(auth))
                        input type="hidden" name="command" value=(Uuid::new_v4());
                        input type="hidden" name="revision" value=(release.revision);
                        @if !eligible.is_empty() {
                            fieldset.release-picks {
                                legend { "Ship selected Done issues" }
                                @for i in &eligible { (pick_issue(i, &project.key, true)) }
                            }
                        } @else { p.muted { "At least one Done issue is required to publish." } }
                        @if !open.is_empty() {
                            details.release-carry {
                                summary { "Carry open issues into a new draft (" (open.len()) ")" }
                                p.muted { "Select the issues to move when this release is published." }
                                @for i in &open { (pick_issue(i, &project.key, false)) }
                                label { "New draft name" input name="carry_to" maxlength="150" placeholder="e.g. Next point release"; }
                            }
                        } @else { input type="hidden" name="carry_to" value=""; }
                        @if !excluded.is_empty() {
                            details { summary { "Archived or canceled (" (excluded.len()) ")" }
                                ul { @for i in &excluded { li { (issue_link(i.id,&project.key,i.number,&i.title)) " · "
                                    @if i.archived { "Archived" } @else { "Canceled" } } } }
                            }
                        }
                        @if !eligible.is_empty() {
                            div.release-publish-controls {
                                label { "Shipment date" input type="date" name="published_on" value=(today) max=(today) required; }
                                button.action-primary type="submit" { "Publish selected issues" }
                            }
                            p.release-meta { "Publishing seals the selected titles and revisions. You can record a correction afterward." }
                        }
                    }
                }
            }
        }
    }
}

fn pick_issue(i: &Member, key: &str, shipping: bool) -> Markup {
    html! {
        div.release-pick {
            label {
                input type="checkbox" name=(if shipping { "ship" } else { "carry" }) value=(format!("{}:{}",i.id,i.revision)) checked[shipping];
                span.release-key { (key) "-" (i.number) }
                span { (i.title) }
            }
            a.release-history href=(format!("/issues/{}#history",i.id)) aria-label=(format!("Issue history for {key}-{}",i.number)) { "History" }
            @if !shipping || i.shipped_before {
                small { @if !shipping { (status_label(&i.status)) @if !i.next_action.is_empty() { " · " (i.next_action) } }
                    @else { "Previously shipped" } }
            }
        }
    }
}

fn edit_scope(
    auth: &Auth,
    id: Uuid,
    key: &str,
    members: &[Member],
    candidates: &[Candidate],
    search: &str,
) -> Markup {
    let dest = format!("/releases/{id}/members");
    html! {
        details class="release-scope" open[members.is_empty() || !search.is_empty()] id="scope" {
            summary { "Manage release scope" }
            @if !members.is_empty() {
                h3 { "Current scope" }
                ul class="clean-list release-scope-list" { @for i in members { li {
                    (issue_link(i.id,key,i.number,&i.title)) small { (status_label(&i.status)) }
                    form method="post" action=(&dest) { (ui::csrf(auth))
                        input type="hidden" name="from" value=(id);
                        input type="hidden" name="issue" value=(i.id);
                        button class="secondary" name="action" value="remove" { "Remove" }
                    }
                } } }
            }
            h3 { "Add issues" }
            form method="get" action=(format!("/releases/{id}#scope")) class="toolbar" {
                label { "Search by title or key" input type="search" name="search" value=(search) maxlength="200"; }
                button class="secondary" { "Find issues" }
            }
            @if candidates.is_empty() { p class="muted" { "No matching issues available to add." } }
            ul class="clean-list release-scope-list" { @for i in candidates.iter().take(20) { li {
                (issue_link(i.id,key,i.number,&i.title)) small { (status_label(&i.status)) }
                form method="post" action=(&dest) { (ui::csrf(auth))
                    input type="hidden" name="from" value=(i.draft_id.map(|v| v.to_string()).unwrap_or_default());
                    input type="hidden" name="issue" value=(i.id);
                    button class="secondary" name="action" value="assign" { @if let Some(ref previous)=i.draft_name { "Move from " (previous) } @else { "Add to draft" } }
                }
            } } }
            @if candidates.len()>20 { p class="muted" { "Showing 20 most recently updated matches. Refine the search to find another issue." } }
        }
    }
}

async fn manifest(
    state: &AppState,
    auth: &Auth,
    project: &store::Project,
    release: &Release,
) -> AppResult<(Markup, Markup)> {
    let id = release.id;
    let shipped: Vec<Shipped> = sqlx::query_as(
        "SELECT e.issue_id,e.number,e.title,e.issue_revision,f.id AS first_id,f.name AS first_name,f.published_on AS first_on
         FROM release_entries e CROSS JOIN LATERAL (
           SELECT r.id,r.name,r.published_on FROM release_entries ee JOIN releases r ON r.id=ee.release_id
           WHERE ee.owner_id=e.owner_id AND ee.issue_id=e.issue_id AND r.published_at IS NOT NULL
           ORDER BY r.published_on,r.published_at,r.id LIMIT 1
         ) f WHERE e.owner_id=$1 AND e.release_id=$2 ORDER BY e.number"
    ).bind(auth.user_id).bind(id).fetch_all(&state.db).await?;
    let carried: Vec<Carried> = sqlx::query_as(
        "SELECT c.issue_id,c.number,c.title,c.status,c.next_release_id,r.name AS next_name,
             m.release_id AS current_release_id,cur.name AS current_name,i.title AS current_title,i.status AS current_status,i.revision,
             (c.next_release_id IS NULL AND m.issue_id IS NULL AND NOT i.archived
              AND c.status IN ('ready','in_progress','backlog') AND i.status IN ('ready','in_progress','backlog')) AS eligible
         FROM release_carryovers c JOIN issues i ON i.id=c.issue_id
         LEFT JOIN releases r ON r.id=c.next_release_id LEFT JOIN release_members m ON m.issue_id=c.issue_id
         LEFT JOIN releases cur ON cur.id=m.release_id WHERE c.owner_id=$1 AND c.release_id=$2 ORDER BY c.number"
    ).bind(auth.user_id).bind(id).fetch_all(&state.db).await?;
    let amendments: Vec<Amendment> = sqlx::query_as(
        "SELECT a.issue_id,e.number,e.title,a.reason,a.replacement,a.created_at,u.email FROM release_amendments a
         JOIN release_entries e ON e.release_id=a.release_id AND e.issue_id=a.issue_id JOIN users u ON u.id=a.actor_id
         WHERE a.owner_id=$1 AND a.release_id=$2 ORDER BY a.created_at,a.id"
    ).bind(auth.user_id).bind(id).fetch_all(&state.db).await?;
    let first = shipped.iter().filter(|row| row.first_id == id).count();
    let overview = html! {
        nav.release-facts aria-label="Shipment summary" {
            a href="#manifest" { strong { (shipped.len()) } " shipped" }
            @if !carried.is_empty() {
                a href="#carryover" { strong { (carried.len()) } " left out" }
            }
            @if !amendments.is_empty() {
                a href="#corrections" { strong { (amendments.len()) } " " (if amendments.len() == 1 { "correction" } else { "corrections" }) }
            }
        }
    };
    let records = html! {
        section.content-section.release-manifest id="manifest" tabindex="-1" aria-labelledby="manifest-heading" {
            h2 id="manifest-heading" { "Shipped issues" }
            @if shipped.is_empty() { p.muted { "No issues were recorded in this shipment." } }
            @else {
                @let repeats = shipped.len() - first;
                @if repeats > 0 {
                    p.release-meta { (first) " first shipped here · " (repeats) " previously shipped" }
                }
                ul.clean-list.release-manifest-rows {
                    @for entry in shipped.iter().take(3) { (manifest_row(entry, project, id)) }
                }
                @if shipped.len() > 3 {
                    details.release-overflow {
                        summary { "Show " (shipped.len()-3) " more " (if shipped.len() == 4 { "issue" } else { "issues" }) }
                        ul.clean-list.release-manifest-rows {
                            @for entry in &shipped[3..] { (manifest_row(entry, project, id)) }
                        }
                    }
                }
            }
        }
        @if !carried.is_empty() {
            details.release-scope {
                summary { "Left out of this shipment · " (carried.len()) }
                div id="carryover" tabindex="-1" {
                    ul class="issue-list" { @for c in &carried { li {
                        (issue_link(c.issue_id,&project.key,c.number,&c.title)) " · " (status_label(&c.status)) " at publication"
                        @if let (Some(next),Some(name))=(c.next_release_id,&c.next_name) {
                            " · carried into " a href=(format!("/releases/{next}")) { (name) }
                        }
                        @if c.current_release_id != c.next_release_id {
                            @if let (Some(current),Some(name))=(c.current_release_id,&c.current_name) {
                                " · currently in " a href=(format!("/releases/{current}")) { (name) }
                            }
                        }
                    } } }
                    @if !project.archived && carried.iter().any(|c| c.eligible) {
                        details { summary { "Move open carryover to a new draft" }
                            form method="post" action=(format!("/releases/{id}/carryover")) class="stack" {
                                (ui::csrf(auth))
                                fieldset { legend { "Open issues without a current draft" }
                                    @for c in carried.iter().filter(|c| c.eligible) {
                                        div { label { input type="checkbox" name="carry" value=(format!("{}:{}",c.issue_id,c.revision)); " " (project.key) "-" (c.number) " · " (c.current_title) " · " (status_label(&c.current_status)) } }
                                    }
                                }
                                label { "New draft name" input name="carry_to" maxlength="150" required; }
                                button { "Move selected issues" }
                            }
                        }
                    }
                }
            }
        }
        @if !amendments.is_empty() {
            details.release-scope {
                summary { "Recorded corrections · " (amendments.len()) }
                ol id="corrections" tabindex="-1" { @for a in &amendments { li {
                    p { (issue_link(a.issue_id,&project.key,a.number,&a.title)) " · " (a.created_at.format("%Y-%m-%d %H:%M UTC")) " · " (a.email) }
                    p { strong { "Correction " } (a.replacement) }
                    p class="muted" { "Reason · " (a.reason) }
                } } }
            }
        }
        @if !project.archived && !shipped.is_empty() {
            details class="release-scope" { summary { "Record a manifest correction" }
                form method="post" action=(format!("/releases/{id}/amend")) class="stack" {
                    (ui::csrf(auth))
                    label { "Shipped entry" select name="issue" { @for e in &shipped { option value=(e.issue_id) { (project.key) "-" (e.number) " · " (e.title) } } } }
                    label { "Correction" textarea name="replacement" maxlength="2000" required {} }
                    label { "Reason for the change" textarea name="reason" maxlength="2000" required {} }
                    button { "Record correction" }
                }
            }
        }
    };
    Ok((overview, records))
}

fn manifest_row(entry: &Shipped, project: &store::Project, release: Uuid) -> Markup {
    html! {
        li {
            a.release-manifest-issue href=(format!("/issues/{}",entry.issue_id)) {
                span.release-key { (&project.key) "-" (entry.number) } span { (&entry.title) }
            }
            small {
                "Revision " (entry.issue_revision)
                @if entry.first_id != release {
                    " · First shipped " (entry.first_on) " in "
                    a href=(format!("/releases/{}#manifest",entry.first_id)) { (&entry.first_name) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::releases::{IndexQuery, index};
    use sqlx::PgPool;

    fn auth(id: Uuid) -> Auth {
        Auth {
            user_id: id,
            email: "release-owner@test.invalid".into(),
            csrf: "a".repeat(64),
        }
    }

    #[sqlx::test]
    async fn native_manifests_count_repeat_shipments_and_keep_original_titles(db: PgPool) {
        let state = AppState::for_test(db.clone());
        let owner: Uuid = sqlx::query_scalar("INSERT INTO users(email,plan) VALUES ('release-owner@test.invalid','max') RETURNING id")
            .fetch_one(&db).await.unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Example','EX') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
        let issue: Uuid = sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title,status) VALUES ($1,$2,1,'Current title','done') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        let carry: Uuid = sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title,status,revision) VALUES ($1,$2,2,'Revised remaining work','in_progress',2) RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        let mut releases = Vec::new();
        for (name, offset) in [("First release", 1), ("Second release", 0)] {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO releases(owner_id,project_id,name) VALUES ($1,$2,$3) RETURNING id",
            )
            .bind(owner)
            .bind(project)
            .bind(name)
            .fetch_one(&db)
            .await
            .unwrap();
            sqlx::query("INSERT INTO release_entries(owner_id,project_id,release_id,issue_id,title,number,issue_revision) VALUES ($1,$2,$3,$4,'Snapshot title',1,1)")
                .bind(owner).bind(project).bind(id).bind(issue).execute(&db).await.unwrap();
            if offset == 0 {
                sqlx::query("INSERT INTO release_carryovers(owner_id,project_id,release_id,issue_id,number,title,status,issue_revision) VALUES ($1,$2,$3,$4,2,'Original remaining work','backlog',1)")
                    .bind(owner).bind(project).bind(id).bind(carry).execute(&db).await.unwrap();
            }
            sqlx::query("UPDATE releases SET published_on=current_date-$2::int,published_at=now(),publish_command=gen_random_uuid() WHERE id=$1")
                .bind(id).bind(offset).execute(&db).await.unwrap();
            releases.push(id);
        }
        let html = index(
            auth(owner),
            State(state.clone()),
            Query(IndexQuery {
                portfolio: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(html.contains(
            "<strong>1</strong> distinct issues · 2 manifest entries across 2 published releases"
        ));
        let detail = show(
            auth(owner),
            State(state.clone()),
            Path(releases[1]),
            Query(DetailQuery::default()),
        )
        .await
        .unwrap()
        .0;
        assert!(detail.contains("Snapshot title"));
        assert!(detail.contains("1 previously shipped"));
        assert!(
            detail.find("class=\"release-facts\"").unwrap() < detail.find("id=\"review\"").unwrap()
        );
        assert!(detail.find("id=\"review\"").unwrap() < detail.find("id=\"manifest\"").unwrap());
        assert!(detail.contains("Original remaining work"));
        assert!(detail.contains("Revised remaining work"));
        assert!(detail.contains(&format!("{carry}:2")));
        assert!(detail.contains(&format!("/releases/{}#manifest", releases[0])));
        assert!(matches!(
            show(
                auth(Uuid::new_v4()),
                State(state.clone()),
                Path(releases[1]),
                Query(DetailQuery::default())
            )
            .await,
            Err(AppError::NotFound)
        ));

        let deletion = sqlx::query("DELETE FROM release_entries WHERE release_id=$1")
            .bind(releases[1])
            .execute(&db)
            .await;
        assert!(deletion.is_err());
        let rename = sqlx::query("UPDATE releases SET name='Rewritten' WHERE id=$1")
            .bind(releases[1])
            .execute(&db)
            .await;
        assert!(rename.is_err());
        sqlx::query("DELETE FROM users WHERE id=$1")
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM release_entries")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
