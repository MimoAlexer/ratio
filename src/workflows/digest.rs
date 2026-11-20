use axum::{
    Form,
    extract::{Path, Query, State},
    response::{Html, Redirect},
};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, ui};

const PAGE_SIZE: i64 = 12;
const MAX_WEEKLY_SNAPSHOTS_PER_RUN: usize = 256;

#[cfg(test)]
fn monday(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

async fn set_schedule(
    db: &PgPool,
    owner: Uuid,
    enabled: bool,
    email_enabled: bool,
    mail_available: bool,
) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let (plan, first_week): (String,NaiveDate) = sqlx::query_as(
        "SELECT plan,date_trunc('week',now() AT TIME ZONE timezone)::date FROM users WHERE id=$1 AND identity_current FOR UPDATE"
    )
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    if enabled {
        if plan != "max" {
            return Err(AppError::Forbidden);
        }
        if email_enabled && !mail_available {
            return Err(AppError::Unavailable(
                "Email delivery is unavailable. You can schedule an in-app digest.".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO portfolio_digest_schedules (owner_id, enabled, next_week, email_enabled)
             VALUES ($1, true, $2, $3)
             ON CONFLICT (owner_id) DO UPDATE
                 SET enabled=true, email_enabled=EXCLUDED.email_enabled,
                     enabled_at=CASE WHEN portfolio_digest_schedules.enabled THEN portfolio_digest_schedules.enabled_at ELSE now() END,
                     next_week=CASE WHEN portfolio_digest_schedules.enabled THEN portfolio_digest_schedules.next_week ELSE EXCLUDED.next_week END",
        )
        .bind(owner)
        .bind(first_week)
        .bind(email_enabled)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("UPDATE portfolio_digest_schedules SET enabled=false WHERE owner_id=$1")
            .bind(owner)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct ScheduleForm {
    csrf: String,
    enabled: bool,
    #[serde(default)]
    email_enabled: bool,
}

pub(super) async fn schedule(
    State(state): State<AppState>,
    auth: Auth,
    Form(form): Form<ScheduleForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    set_schedule(
        &state.db,
        auth.user_id,
        form.enabled,
        form.email_enabled,
        super::digest_email::available(&state),
    )
    .await?;
    Ok(Redirect::to("/workflows#portfolio-digest"))
}

/// Run from the background worker. A missing week stays due until its snapshot commits.
pub async fn run_due_digests(db: &PgPool) -> AppResult<u64> {
    run_at(db, Utc::now()).await
}

async fn run_at(db: &PgPool, as_of: DateTime<Utc>) -> AppResult<u64> {
    let mut created = 0;
    for _ in 0..MAX_WEEKLY_SNAPSHOTS_PER_RUN {
        let mut tx = db.begin().await?;
        // All schedule and plan writers lock the user first. Skip busy accounts
        // so another worker or a checkout can make progress independently.
        let owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT u.id FROM portfolio_digest_schedules s JOIN users u ON u.id=s.owner_id
             WHERE u.plan='max' AND u.identity_current AND s.enabled AND s.next_week < date_trunc('week',$1::timestamptz AT TIME ZONE u.timezone)::date
             ORDER BY s.next_week, u.id LIMIT 1 FOR UPDATE OF u SKIP LOCKED",
        )
        .bind(as_of)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(owner) = owner else {
            tx.commit().await?;
            break;
        };
        // Re-read after acquiring the user lock. A schedule change may have
        // committed between the claim statement's snapshot and its row lock.
        let due: Option<(NaiveDate,NaiveDate,String)> = sqlx::query_as(
            "SELECT s.next_week,date_trunc('week',$2::timestamptz AT TIME ZONE u.timezone)::date,u.timezone
             FROM portfolio_digest_schedules s JOIN users u ON u.id=s.owner_id
             WHERE s.owner_id=$1 AND u.plan='max' AND u.identity_current AND s.enabled
               AND s.next_week < date_trunc('week',$2::timestamptz AT TIME ZONE u.timezone)::date",
        )
        .bind(owner)
        .bind(as_of)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((week, current_week, timezone)) = due {
            let end = week + Duration::weeks(1);
            // Each figure comes from the same statement snapshot. Digest rows
            // contain native aggregate counts only, so they can be kept after
            // external repository access changes.
            let inserted = sqlx::query(
                "INSERT INTO portfolio_digests
                     (owner_id, week_start, timezone, new_issues, issues_done, milestone_targets, releases_published)
                 SELECT $1, $2, $4,
                     (SELECT count(*) FROM issues WHERE owner_id=$1
                       AND created_at >= ($2::date::timestamp AT TIME ZONE $4::text)
                       AND created_at < ($3::date::timestamp AT TIME ZONE $4::text)),
                     (SELECT count(DISTINCT issue_id) FROM issue_events WHERE owner_id=$1
                       AND new_status='done' AND old_status IS DISTINCT FROM 'done'
                       AND occurred_at >= ($2::date::timestamp AT TIME ZONE $4::text)
                       AND occurred_at < ($3::date::timestamp AT TIME ZONE $4::text)),
                     (SELECT count(*) FROM milestones WHERE owner_id=$1
                       AND target_on >= $2 AND target_on < $3),
                     (SELECT count(*) FROM releases WHERE owner_id=$1
                       AND published_on >= $2 AND published_on < $3)
                 ON CONFLICT (owner_id, week_start) DO NOTHING",
            )
            .bind(owner)
            .bind(week)
            .bind(end)
            .bind(&timezone)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            created += inserted;
            // Catch up every missing in-app review after downtime, and email the latest complete week.
            if inserted == 1 && end == current_week {
                sqlx::query(
                    "INSERT INTO portfolio_digest_email_outbox(owner_id,week_start)
                     SELECT owner_id,$2 FROM portfolio_digest_schedules
                     WHERE owner_id=$1 AND enabled AND email_enabled ON CONFLICT DO NOTHING",
                )
                .bind(owner)
                .bind(week)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query("UPDATE portfolio_digest_schedules SET next_week=$2 WHERE owner_id=$1")
                .bind(owner)
                .bind(end)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    Ok(created)
}

#[derive(FromRow)]
struct Digest {
    week_start: NaiveDate,
    timezone: String,
    new_issues: i64,
    issues_done: i64,
    milestone_targets: i64,
    releases_published: i64,
}

async fn read_history(
    db: &PgPool,
    owner: Uuid,
    before: Option<NaiveDate>,
    page_size: i64,
) -> AppResult<(Vec<Digest>, Option<NaiveDate>)> {
    let mut rows: Vec<Digest> = sqlx::query_as(
        "SELECT week_start, timezone, new_issues, issues_done, milestone_targets, releases_published
         FROM portfolio_digests WHERE owner_id=$1 AND ($2::date IS NULL OR week_start < $2)
         ORDER BY week_start DESC LIMIT $3",
    )
    .bind(owner)
    .bind(before)
    .bind(page_size + 1)
    .fetch_all(db)
    .await?;
    let next = if rows.len() > page_size as usize {
        rows.pop();
        rows.last().map(|r| r.week_start)
    } else {
        None
    };
    Ok((rows, next))
}

fn week_label(week: NaiveDate) -> String {
    let end = week + Duration::days(6);
    if week.year() != end.year() {
        format!(
            "{} – {}",
            week.format("%b %-d, %Y"),
            end.format("%b %-d, %Y")
        )
    } else if week.month() != end.month() {
        format!("{} – {}", week.format("%b %-d"), end.format("%b %-d, %Y"))
    } else {
        format!("{}–{}", week.format("%b %-d"), end.format("%-d, %Y"))
    }
}

fn digest_rows(rows: &[Digest], nested: bool) -> Markup {
    html! {
        @for row in rows {
            article.workflow-digest id=(format!("digest-{}", row.week_start)) {
                header {
                    @if nested { h3 { (week_label(row.week_start)) } }
                    @else { h2 { (week_label(row.week_start)) } }
                    span.workflow-summary { (row.timezone) }
                }
                dl.workflow-stats {
                    div { dt { "New issues" } dd { (row.new_issues) } }
                    div { dt { "Issues marked Done" } dd { (row.issues_done) } }
                    div { dt { "Milestone targets" } dd { (row.milestone_targets) } }
                    div { dt { "Published releases" } dd { (row.releases_published) } }
                }
            }
        }
    }
}

fn older_link(before: NaiveDate) -> Markup {
    html! { a href=(format!("/workflows/digest?before={before}")) { "Earlier digests" } }
}

pub(super) async fn section(state: &AppState, auth: &Auth, plan: &str) -> AppResult<Markup> {
    let db = &state.db;
    let mail_available = super::digest_email::available(state);
    let (timezone, active_week, email_enabled): (String,Option<NaiveDate>,bool) = sqlx::query_as(
        "SELECT u.timezone,s.next_week,coalesce(s.email_enabled,false)
         FROM users u LEFT JOIN portfolio_digest_schedules s ON s.owner_id=u.id AND s.enabled WHERE u.id=$1",
    ).bind(auth.user_id).fetch_one(db).await?;
    let (rows, next) = read_history(db, auth.user_id, None, 1).await?;
    Ok(html! {
        section.workflow-section id="portfolio-digest" aria-labelledby="portfolio-digest-heading" {
            header.workflow-label {
                h2 id="portfolio-digest-heading" { "Weekly portfolio digest" }
                @if plan != "max" { span.workflow-access { "Schedule with Max" } }
            }
            div.workflow-body {
                @if plan != "max" {
                    p { "Review new and completed issues, milestone targets and published releases across projects each week." }
                } @else if let Some(week) = active_week {
                    p { strong { "Scheduled" } " · Next review covers " (week_label(week)) " in " (timezone) "." }
                    details.workflow-delivery {
                        summary { "Manage delivery" }
                        @if email_enabled || mail_available {
                            form.workflow-form method="post" action="/workflows/digest" {
                                (ui::csrf(auth))
                                input type="hidden" name="enabled" value="true";
                                input type="hidden" name="email_enabled" value=(if email_enabled { "false" } else { "true" });
                                @if email_enabled {
                                    p { "Email links are enabled for " (auth.email) "." }
                                    button.action-secondary { "Turn off email links" }
                                } @else {
                                    p { "Send a private digest link to " (auth.email) "." }
                                    button.action-secondary { "Turn on email links" }
                                }
                            }
                        }
                        @if !mail_available { p class="muted" { "Email delivery is unavailable on this server. In-app delivery is scheduled." } }
                        form method="post" action="/workflows/digest" {
                            (ui::csrf(auth))
                            button.action-secondary name="enabled" value="false" { "Pause weekly digests" }
                        }
                    }
                } @else {
                    p { "Receive an in-app review each Monday for the previous week in " (timezone) "." }
                    form.workflow-form method="post" action="/workflows/digest" {
                        (ui::csrf(auth))
                        @if mail_available {
                            label { input type="checkbox" name="email_enabled" value="true"; " Email me a private digest link at " (auth.email) }
                        }
                        button.action-secondary name="enabled" value="true" { "Schedule weekly digests" }
                    }
                    @if !mail_available { small { "Email delivery is unavailable on this server." } }
                }
                (digest_rows(&rows, true))
                @if let Some(before) = next { (older_link(before)) }
            }
        }
    })
}

#[derive(Deserialize)]
pub(super) struct HistoryQuery {
    before: Option<NaiveDate>,
}

pub(super) async fn history(
    State(state): State<AppState>,
    auth: Auth,
    Query(query): Query<HistoryQuery>,
) -> AppResult<Html<String>> {
    let (rows, next) = read_history(&state.db, auth.user_id, query.before, PAGE_SIZE).await?;
    Ok(ui::page(
        &auth,
        "Portfolio digests",
        "workflows",
        html! {
            div.workflow-workspace {
                a.workflow-back href="/workflows#portfolio-digest" { "Back to workflows" }
                header.view-head { h1 { "Portfolio digests" } }
                @if rows.is_empty() {
                    p.empty-state { "No digests in this period." }
                } @else {
                    (digest_rows(&rows, false))
                }
                @if next.is_some() || query.before.is_some() {
                    nav.workflow-jumps aria-label="Digest pages" {
                        @if let Some(before) = next { (older_link(before)) }
                        @if query.before.is_some() { a href="/workflows/digest" { "Latest digests" } }
                    }
                }
            }
        },
    ))
}

pub(super) async fn show(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> AppResult<Html<String>> {
    // A forwarded link never grants access. Each visit checks both the viewer and their current plan.
    let row:Digest = sqlx::query_as(
        "SELECT d.week_start,d.timezone,d.new_issues,d.issues_done,d.milestone_targets,d.releases_published
         FROM portfolio_digests d JOIN users u ON u.id=d.owner_id
         WHERE d.owner_id=$1 AND d.id=$2 AND u.plan='max' AND u.identity_current",
    ).bind(auth.user_id).bind(id).fetch_optional(&state.db).await?.ok_or(AppError::NotFound)?;
    Ok(ui::page(
        &auth,
        "Weekly portfolio digest",
        "workflows",
        html! {
            div.workflow-workspace {
                a.workflow-back href="/workflows/digest" { "All portfolio digests" }
                header.view-head { h1 { "Weekly portfolio digest" } }
                (digest_rows(&[row], false))
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run_before(db: &PgPool, week: NaiveDate) -> AppResult<u64> {
        run_at(db, week.and_hms_opt(0, 0, 0).unwrap().and_utc()).await
    }

    fn date(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    #[test]
    fn reports_show_monday_to_sunday_date_labels() {
        assert_eq!(monday(date("2026-01-01")), date("2025-12-29"));
        assert_eq!(monday(date("2026-11-22")), date("2026-11-16"));
        assert_eq!(monday(date("2026-11-23")), date("2026-11-23"));
        assert_eq!(week_label(date("2025-12-29")), "Dec 29, 2025 – Jan 4, 2026");
        assert_eq!(week_label(date("2026-11-16")), "Nov 16–22, 2026");
    }

    async fn owner(db: &PgPool, plan: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id,email,plan) VALUES ($1,$2,$3)")
            .bind(id)
            .bind(format!("{id}@example.invalid"))
            .bind(plan)
            .execute(db)
            .await
            .unwrap();
        id
    }

    async fn add_week_activity(db: &PgPool, owner: Uuid) {
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects (owner_id,name,key) VALUES ($1,'Digest project','DIG') RETURNING id",
        )
        .bind(owner)
        .fetch_one(db)
        .await
        .unwrap();
        let issue: Uuid = sqlx::query_scalar(
            "INSERT INTO issues (owner_id,project_id,number,title,created_at)
             VALUES ($1,$2,1,'First','2026-11-16 00:00+00') RETURNING id",
        )
        .bind(owner)
        .bind(project)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issues (owner_id,project_id,number,title,created_at)
             VALUES ($1,$2,2,'Monday boundary','2026-11-23 00:00+00')",
        )
        .bind(owner)
        .bind(project)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issue_events (owner_id,project_id,issue_id,old_status,new_status,occurred_at)
             VALUES ($1,$2,$3,'ready','done','2026-11-17 00:00+00'),
                    ($1,$2,$3,'done','ready','2026-11-19 00:00+00'),
                    ($1,$2,$3,'ready','done','2026-11-22 23:59+00')",
        )
        .bind(owner)
        .bind(project)
        .bind(issue)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO milestones (owner_id,project_id,title,target_on)
             VALUES ($1,$2,'Friday target','2026-11-20'), ($1,$2,'Unscheduled',NULL)",
        )
        .bind(owner)
        .bind(project)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO releases (owner_id,project_id,name,published_on,published_at,publish_command)
             VALUES ($1,$2,'Published','2026-11-22','2026-11-23 01:00+00',gen_random_uuid()),
                    ($1,$2,'Draft',NULL,NULL,NULL)",
        )
        .bind(owner)
        .bind(project)
        .execute(db)
        .await
        .unwrap();
    }

    async fn set_next_week(db: &PgPool, owner: Uuid, week: NaiveDate) {
        sqlx::query("UPDATE portfolio_digest_schedules SET next_week=$2 WHERE owner_id=$1")
            .bind(owner)
            .bind(week)
            .execute(db)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn private_week_is_idempotent_under_concurrent_workers(db: PgPool) {
        let mine = owner(&db, "max").await;
        let other = owner(&db, "free").await;
        add_week_activity(&db, mine).await;
        add_week_activity(&db, other).await;
        assert!(matches!(
            set_schedule(&db, other, true, false, false).await,
            Err(AppError::Forbidden)
        ));
        set_schedule(&db, mine, true, false, false).await.unwrap();
        let start = date("2026-11-16");
        let cutoff = date("2026-11-23");
        set_next_week(&db, mine, start).await;
        assert_eq!(run_before(&db, start).await.unwrap(), 0);
        let (a, b) = tokio::join!(run_before(&db, cutoff), run_before(&db, cutoff));
        assert_eq!(a.unwrap() + b.unwrap(), 1);
        assert_eq!(run_before(&db, cutoff).await.unwrap(), 0);
        let (rows, _) = read_history(&db, mine, None, PAGE_SIZE).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].week_start, start);
        assert_eq!(rows[0].new_issues, 1);
        assert_eq!(rows[0].issues_done, 1);
        assert_eq!(rows[0].milestone_targets, 1);
        assert_eq!(rows[0].releases_published, 1);
        assert!(
            read_history(&db, other, None, PAGE_SIZE)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[sqlx::test]
    async fn downgrade_stops_digests_and_reupgrade_requires_opt_in(db: PgPool) {
        let id = owner(&db, "max").await;
        set_schedule(&db, id, true, false, false).await.unwrap();
        set_next_week(&db, id, date("2026-11-02")).await;
        assert_eq!(run_before(&db, date("2026-11-16")).await.unwrap(), 2);
        sqlx::query("UPDATE users SET plan='plus' WHERE id=$1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(run_before(&db, date("2026-11-23")).await.unwrap(), 0);
        sqlx::query("UPDATE users SET plan='max' WHERE id=$1")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(run_before(&db, date("2026-11-23")).await.unwrap(), 0);
        let (recent, cursor) = read_history(&db, id, None, 1).await.unwrap();
        assert_eq!(recent[0].week_start, date("2026-11-09"));
        let (older, end) = read_history(&db, id, cursor, 1).await.unwrap();
        assert_eq!(older[0].week_start, date("2026-11-02"));
        assert!(end.is_none());
        set_schedule(&db, id, true, false, false).await.unwrap();
        let (active, next): (bool, NaiveDate) = sqlx::query_as(
            "SELECT enabled,next_week FROM portfolio_digest_schedules WHERE owner_id=$1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(active);
        assert_eq!(next, monday(Utc::now().date_naive()));
    }

    #[sqlx::test]
    async fn local_monday_observes_dst_and_opt_in_queues_only_the_latest_link(db: PgPool) {
        let mine = owner(&db, "max").await;
        let other = owner(&db, "max").await;
        let state = AppState::for_test(db.clone());
        sqlx::query("UPDATE users SET timezone='America/New_York' WHERE id=$1")
            .bind(mine)
            .execute(&db)
            .await
            .unwrap();
        assert!(matches!(
            set_schedule(&db, mine, true, true, false).await,
            Err(AppError::Unavailable(_))
        ));
        set_schedule(&db, mine, true, true, true).await.unwrap();
        set_next_week(&db, mine, date("2026-02-23")).await;
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Clock','CLK') RETURNING id",
        )
        .bind(mine)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issues(owner_id,project_id,number,title,created_at)
            VALUES ($1,$2,1,'Before window','2026-03-02T04:59:59Z'),
            ($1,$2,2,'At start','2026-03-02T05:00:00Z'),
            ($1,$2,3,'Before end','2026-03-09T03:59:59Z'),
            ($1,$2,4,'Next week','2026-03-09T04:00:00Z')",
        )
        .bind(mine)
        .bind(project)
        .execute(&db)
        .await
        .unwrap();
        let time = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(run_at(&db, time("2026-03-09T04:00:00Z")).await.unwrap(), 2);
        let (id,new_issues,zone):(Uuid,i64,String)=sqlx::query_as("SELECT id,new_issues,timezone FROM portfolio_digests WHERE owner_id=$1 AND week_start='2026-03-02'")
            .bind(mine).fetch_one(&db).await.unwrap();
        assert_eq!((new_issues, zone.as_str()), (2, "America/New_York"));
        let queued: Vec<NaiveDate> = sqlx::query_scalar(
            "SELECT week_start FROM portfolio_digest_email_outbox WHERE owner_id=$1",
        )
        .bind(mine)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(queued, vec![date("2026-03-02")]);
        let actor = |id| Auth {
            user_id: id,
            email: "fixture@example.test".into(),
            csrf: String::new(),
        };
        assert!(
            show(State(state.clone()), actor(mine), Path(id))
                .await
                .unwrap()
                .0
                .contains("America/New_York")
        );
        assert!(matches!(
            show(State(state.clone()), actor(other), Path(id)).await,
            Err(AppError::NotFound)
        ));
        sqlx::query("UPDATE users SET plan='plus' WHERE id=$1")
            .bind(mine)
            .execute(&db)
            .await
            .unwrap();
        assert!(matches!(
            show(State(state), actor(mine), Path(id)).await,
            Err(AppError::NotFound)
        ));
        let active: (bool, bool) = sqlx::query_as(
            "SELECT enabled,email_enabled FROM portfolio_digest_schedules WHERE owner_id=$1",
        )
        .bind(mine)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(active, (false, false));
        // Without consent there is no pending delivery, including after later re-upgrades.
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM portfolio_digest_email_outbox WHERE owner_id=$1 AND status='pending'")
            .bind(mine).fetch_one(&db).await.unwrap(),0);
    }

    #[sqlx::test]
    async fn local_week_is_due_only_after_the_workspaces_monday(db: PgPool) {
        let owner = owner(&db, "max").await;
        sqlx::query("UPDATE users SET timezone='America/New_York' WHERE id=$1")
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();
        set_schedule(&db, owner, true, false, false).await.unwrap();
        set_next_week(&db, owner, date("2026-03-02")).await;
        assert_eq!(
            run_at(&db, "2026-03-09T03:59:59Z".parse().unwrap())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            run_at(&db, "2026-03-09T04:00:00Z".parse().unwrap())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM portfolio_digest_email_outbox WHERE owner_id=$1"
            )
            .bind(owner)
            .fetch_one(&db)
            .await
            .unwrap(),
            0
        );
    }
}
