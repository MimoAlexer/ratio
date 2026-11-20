use std::collections::{BTreeMap, BTreeSet};

use axum::{
    Router,
    extract::{Path, Query, State},
    response::Html,
    routing::get,
};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use maud::{Markup, html};
use rand::{Rng, SeedableRng, rngs::StdRng};
use serde::Deserialize;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, ui};

const SAMPLE_WEEKS: i64 = 26;
const MIN_WEEKS: usize = 12;
const MIN_POSITIVE: usize = 3;
const HORIZON: i64 = 52;
const TRIALS: usize = 4096;
const SEED: u64 = 731_041;

type Tx<'a> = Transaction<'a, Postgres>;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/releases/{id}/forecast/weeks/{week}", get(events))
}

#[derive(FromRow)]
struct Context {
    project: Uuid,
    key: String,
    timezone: String,
    created: NaiveDateTime,
    today: NaiveDate,
    paid: bool,
    published: bool,
}

#[derive(FromRow)]
struct Scope {
    total: i64,
    done: i64,
    kinds: Vec<String>,
}

#[derive(FromRow)]
struct Coverage {
    first_observation: Option<NaiveDateTime>,
    baseline: Option<NaiveDateTime>,
    incomplete: i64,
}

#[derive(Clone, Copy, Default)]
struct Week {
    entries: i64,
    reopenings: i64,
}

impl Week {
    fn net(self) -> i64 {
        self.entries - self.reopenings
    }
}

fn monday(d: NaiveDate) -> NaiveDate {
    d - Duration::days(i64::from(d.weekday().num_days_from_monday()))
}

fn first_full_week(start: NaiveDateTime) -> NaiveDate {
    let first = monday(start.date());
    if first == start.date() && start.time() == NaiveTime::MIN {
        first
    } else {
        first + Duration::weeks(1)
    }
}

struct Sample {
    start: NaiveDate,
    end: NaiveDate, // Exclusive, the local Monday of the current week.
    weeks: BTreeMap<NaiveDate, Week>,
    coverage: Coverage,
}

impl Sample {
    fn positive(&self) -> usize {
        self.weeks.values().filter(|w| w.net() > 0).count()
    }

    fn net(&self) -> i64 {
        self.weeks.values().map(|w| w.net()).sum()
    }

    fn unavailable(&self) -> Option<String> {
        if self.coverage.incomplete > 0 {
            return Some(format!(
                "{} imported issue(s) in this cohort lack a complete status history.",
                self.coverage.incomplete
            ));
        }
        if self.coverage.first_observation.is_none() {
            return Some("No native status history has been recorded for these work types.".into());
        }
        if self.weeks.len() < MIN_WEEKS {
            return Some(format!(
                "{} of {MIN_WEEKS} required complete weeks are available.",
                self.weeks.len()
            ));
        }
        if self.positive() < MIN_POSITIVE {
            return Some(format!(
                "{} of {MIN_POSITIVE} required weeks have positive net progress.",
                self.positive()
            ));
        }
        if self.net() <= 0 {
            return Some("Average weekly net progress must be positive.".into());
        }
        None
    }
}

async fn snapshot(db: &PgPool) -> AppResult<Tx<'_>> {
    let mut tx = db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

async fn context(tx: &mut Tx<'_>, owner: Uuid, release: Uuid) -> AppResult<Context> {
    sqlx::query_as("SELECT p.id AS project,p.key,u.timezone,p.created_at AT TIME ZONE u.timezone AS created,
        (now() AT TIME ZONE u.timezone)::date AS today,u.plan IN ('plus','max') AS paid,
        r.published_at IS NOT NULL AS published
        FROM releases r JOIN users u ON u.id=r.owner_id
        JOIN projects p ON p.owner_id=r.owner_id AND p.id=r.project_id WHERE r.owner_id=$1 AND r.id=$2")
        .bind(owner).bind(release).fetch_optional(&mut **tx).await?.ok_or(AppError::NotFound)
}

async fn scope(tx: &mut Tx<'_>, owner: Uuid, release: Uuid) -> AppResult<Scope> {
    Ok(sqlx::query_as("SELECT count(*) AS total,count(*) FILTER (WHERE i.status='done') AS done,
        coalesce(array_agg(DISTINCT i.kind ORDER BY i.kind),ARRAY[]::text[]) AS kinds
        FROM release_members m JOIN issues i ON i.owner_id=m.owner_id AND i.project_id=m.project_id AND i.id=m.issue_id
        WHERE m.owner_id=$1 AND m.release_id=$2 AND i.status<>'canceled'")
        .bind(owner).bind(release).fetch_one(&mut **tx).await?)
}

// Imported current states are baselines. A later observed transition can anchor a
// snapshot with an unknown opening, but a missing or contradictory tail has no end date.
async fn coverage(
    tx: &mut Tx<'_>,
    owner: Uuid,
    ctx: &Context,
    scope: &Scope,
) -> AppResult<Coverage> {
    Ok(sqlx::query_as("WITH imported AS (
        SELECT i.id,i.status FROM transfer_records tr JOIN issues i
          ON i.owner_id=tr.owner_id AND i.project_id=tr.project_id AND i.id=tr.local_id
        WHERE tr.owner_id=$1 AND tr.project_id=$2 AND tr.kind='issue' AND i.kind=ANY($3)
    ), tails AS (
        SELECT i.status,first_event.occurred_at AS first_at,first_event.old_status AS first_old,
               last_event.new_status AS last_status
        FROM imported i LEFT JOIN LATERAL (
            SELECT occurred_at,old_status FROM issue_events
            WHERE owner_id=$1 AND project_id=$2 AND issue_id=i.id
            ORDER BY occurred_at,revision_after NULLS LAST,id LIMIT 1
        ) first_event ON true LEFT JOIN LATERAL (
            SELECT new_status FROM issue_events
            WHERE owner_id=$1 AND project_id=$2 AND issue_id=i.id
            ORDER BY occurred_at DESC,revision_after DESC NULLS LAST,id DESC LIMIT 1
        ) last_event ON true
    )
    SELECT (SELECT min(e.occurred_at) FROM issue_events e JOIN issues i ON i.id=e.issue_id
            AND i.owner_id=e.owner_id AND i.project_id=e.project_id
         WHERE e.owner_id=$1 AND e.project_id=$2 AND i.kind=ANY($3)
           AND e.action NOT IN ('import','baseline','import_baseline')) AT TIME ZONE $4 AS first_observation,
        greatest(
        (SELECT max(e.occurred_at) FROM issue_events e JOIN issues i ON i.id=e.issue_id
            AND i.owner_id=e.owner_id AND i.project_id=e.project_id
         WHERE e.owner_id=$1 AND e.project_id=$2 AND i.kind=ANY($3)
           AND e.action IN ('import','baseline','import_baseline') AND e.old_status IS DISTINCT FROM e.new_status),
        (SELECT max(first_at) FROM tails WHERE first_old IS NOT NULL)
    ) AT TIME ZONE $4 AS baseline,
    (SELECT count(*) FROM tails WHERE last_status IS DISTINCT FROM status) AS incomplete")
        .bind(owner).bind(ctx.project).bind(&scope.kinds).bind(&ctx.timezone)
        .fetch_one(&mut **tx).await?)
}

// The release filter selects work types. Historical progress is independent of
// release membership, including later assignments, removals and archived issues.
const EVENT_FILTER: &str = " FROM issue_events e JOIN issues i ON i.id=e.issue_id
    AND i.owner_id=e.owner_id AND i.project_id=e.project_id
    WHERE e.owner_id=$1 AND e.project_id=$2 AND i.kind=ANY($3)
    AND e.occurred_at >= ($4::date::timestamp AT TIME ZONE $6)
    AND e.occurred_at < ($5::date::timestamp AT TIME ZONE $6)
    AND e.action NOT IN ('import','baseline','import_baseline')
    AND e.old_status IS DISTINCT FROM e.new_status
    AND (e.new_status='done' OR (e.old_status='done' AND e.new_status IN ('backlog','ready','in_progress')))";

async fn sample(tx: &mut Tx<'_>, owner: Uuid, ctx: &Context, scope: &Scope) -> AppResult<Sample> {
    let coverage = coverage(tx, owner, ctx, scope).await?;
    let end = monday(ctx.today);
    let start = coverage.first_observation.map_or(end, |first| {
        let lower = ctx
            .created
            .max(first)
            .max(coverage.baseline.unwrap_or(first));
        first_full_week(lower).max(end - Duration::weeks(SAMPLE_WEEKS))
    });
    let mut weeks = BTreeMap::new();
    let mut date = start;
    while date < end {
        weeks.insert(date, Week::default());
        date += Duration::weeks(1);
    }
    if !weeks.is_empty() && coverage.incomplete == 0 {
        let sql = format!(
            "SELECT date_trunc('week',e.occurred_at AT TIME ZONE $6)::date AS week,
            count(*) FILTER (WHERE e.new_status='done'),count(*) FILTER (WHERE e.old_status='done')
            {EVENT_FILTER} GROUP BY 1"
        );
        let rows: Vec<(NaiveDate, i64, i64)> = sqlx::query_as(&sql)
            .bind(owner)
            .bind(ctx.project)
            .bind(&scope.kinds)
            .bind(start)
            .bind(end)
            .bind(&ctx.timezone)
            .fetch_all(&mut **tx)
            .await?;
        for (day, entries, reopenings) in rows {
            weeks.insert(
                day,
                Week {
                    entries,
                    reopenings,
                },
            );
        }
    }
    Ok(Sample {
        start,
        end,
        weeks,
        coverage,
    })
}

fn working_weeks(first: NaiveDate, off: &BTreeSet<NaiveDate>) -> Vec<NaiveDate> {
    (0..HORIZON)
        .map(|w| first + Duration::weeks(w))
        .filter(|w| !off.contains(w))
        .collect()
}

fn completion<F: FnMut() -> i64>(
    scope: &Scope,
    working: &[NaiveDate],
    mut next: F,
) -> Option<NaiveDate> {
    let mut done = scope.done;
    for &date in working {
        done = done.saturating_add(next()).clamp(0, scope.total);
        if done == scope.total {
            return Some(date);
        }
    }
    None
}

fn percentiles(sample: &Sample, scope: &Scope, working: &[NaiveDate]) -> [Option<NaiveDate>; 2] {
    let observations: Vec<_> = sample.weeks.values().map(|w| w.net()).collect();
    let mut rng = StdRng::seed_from_u64(SEED);
    let mut finished = Vec::with_capacity(TRIALS);
    for _ in 0..TRIALS {
        if let Some(date) = completion(scope, working, || {
            observations[rng.gen_range(0..observations.len())]
        }) {
            finished.push(date);
        }
    }
    finished.sort_unstable();
    // Unfinished trials come after every recorded date in the percentile ranking.
    [50, 85].map(|pct| finished.get((TRIALS * pct).div_ceil(100) - 1).copied())
}

fn interval(first: NaiveDate, median: NaiveDate, upper: NaiveDate) -> Markup {
    let last = upper + Duration::days(6);
    let span = (last - first).num_days() as f64;
    let position = |day: NaiveDate| 2.0 + 96.0 * (day - first).num_days() as f64 / span;
    let label_anchor = |x: f64| {
        if x < 15.0 {
            "start"
        } else if x > 85.0 {
            "end"
        } else {
            "middle"
        }
    };
    let p50 = position(median);
    let p85 = position(upper);
    let x50 = format!("{p50:.2}%");
    let x85 = format!("{p85:.2}%");
    html! { figure.chart.release-readiness-interval {
        svg width="100%" height="103" role="img" aria-label=(format!("Projected readiness: median week of {median}, 85th-percentile week of {upper}. The projection starts {first}.")) {
            defs { pattern id="readiness-hatch" width="6" height="6" patternUnits="userSpaceOnUse" {
                path d="M-1 1 1 -1 M0 6 6 0 M5 7 7 5" stroke="currentColor" fill="none" opacity=".35" {}
            } }
            rect x=(x50) y="32" width=(format!("{:.2}%", 98.0-p50)) height="16" fill="url(#readiness-hatch)" {}
            line x1="2%" x2="98%" y1="40" y2="40" stroke="currentColor" opacity=".65" {}
            @for n in 0..=(upper-first).num_weeks() {
                @let x = format!("{:.2}%", position(first + Duration::weeks(n)));
                line x1=(x) x2=(x) y1="38" y2="45" stroke="currentColor" opacity=".6" {}
            }
            line x1="98%" x2="98%" y1="32" y2="48" stroke="currentColor" {}
            line x1=(x50) x2=(x50) y1="20" y2="40" stroke="currentColor" {}
            text x=(x50) y="16" text-anchor=(label_anchor(p50)) font-size="12" fill="currentColor" { (if median==upper { "P50 / P85" } else { "P50" }) }
            @if median != upper {
                line x1=(x85) x2=(x85) y1="40" y2="57" stroke="currentColor" {}
                text x=(x85) y="70" text-anchor=(label_anchor(p85)) font-size="12" fill="currentColor" { "P85" }
            }
            circle cx=(x85) cy="40" r="5" fill="var(--surface, Canvas)" stroke="currentColor" stroke-width="1.5" {}
            circle cx=(x50) cy="40" r=(if median==upper { 2.5 } else { 4.0 }) fill="var(--accent, currentColor)" {}
            text x="2%" y="95" font-size="12" fill="currentColor" { (first.format("%d %b %Y")) }
            text x="98%" y="95" text-anchor="end" font-size="12" fill="currentColor" { (last.format("%d %b %Y")) }
        }
        figcaption.muted { "Hatching spans the median readiness week through the end of the 85th-percentile week." }
    } }
}

fn estimate(label: &str, date: Option<NaiveDate>) -> Markup {
    html! { p { strong { (label) } " · " @if let Some(date)=date {
        "Week of " (date)
    } @else { "Beyond the " (HORIZON) "-week horizon" } } }
}

pub(super) async fn forecast(
    db: &PgPool,
    owner: Uuid,
    release: Uuid,
    pace: Option<u16>,
    model: bool,
) -> AppResult<Markup> {
    let mut tx = snapshot(db).await?;
    let ctx = context(&mut tx, owner, release).await?;
    if ctx.published {
        return Ok(html! {});
    }
    if !ctx.paid {
        return Ok(
            html! { section.content-section.release-forecast id="forecast" { h2 { "Release readiness" } p { "Fixed-scope forecasts and pace scenarios are available on " a href="/billing" { "Plus and Max" } "." } } },
        );
    }
    if pace == Some(0) {
        return Err(AppError::BadRequest(
            "Enter a weekly pace of at least one issue.".into(),
        ));
    }
    let scope = scope(&mut tx, owner, release).await?;
    if scope.total == 0 || scope.total == scope.done {
        return Ok(
            html! { section.content-section.release-forecast id="forecast" { h2 { "Release readiness" } p {
                @if scope.total == 0 { "Add a non-canceled issue to forecast this draft." }
                @else { "All " (scope.total) " issues in scope are already Done." }
            } } },
        );
    }
    let sample = sample(&mut tx, owner, &ctx, &scope).await?;
    let first = sample.end + Duration::weeks(1);
    let off: Vec<NaiveDate> = sqlx::query_scalar(
        "SELECT week_of FROM roadmap_weeks_off
        WHERE owner_id=$1 AND project_id=$2 AND week_of >= $3 AND week_of < $4 ORDER BY week_of",
    )
    .bind(owner)
    .bind(ctx.project)
    .bind(first)
    .bind(first + Duration::weeks(HORIZON))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let work = working_weeks(first, &off.iter().copied().collect());
    let reason = sample.unavailable();
    let modeled = (model && reason.is_none()).then(|| percentiles(&sample, &scope, &work));
    let url = format!("/releases/{release}");
    Ok(
        html! { section.content-section.release-forecast id="forecast" {
            div.section-head { h2 { "Release readiness" } }
            p { strong { (scope.total-scope.done) (if scope.total-scope.done==1 { " issue remaining" } else { " issues remaining" }) } " · " (scope.done) " of " (scope.total) " Done" }
            p.release-meta { "Fixed scope · next full week " (first) " · " (HORIZON) "-week horizon · " (&ctx.timezone) }
            @if !off.is_empty() { details { summary { (off.len()) " planned weeks off" } ul { @for date in &off { li { "Week of " (date) } } } } }
            h3 { "Historical forecast" }
            @if let Some(reason)=reason {
                p.status-note { (reason) }
            } @else if let Some([median,upper])=modeled {
                (estimate("Median readiness",median))
                (estimate("85th-percentile readiness",upper))
                @if let (Some(median),Some(upper))=(median,upper) { (interval(first,median,upper)) }
                a href=(format!("{url}#forecast")) { "Hide forecast" }
            } @else {
                form method="get" action=(format!("{url}#forecast")) {
                    input type="hidden" name="model" value="true";
                    button.secondary type="submit" { "Model fixed-scope readiness" }
                }
            }
            details {
                summary { "Forecast source and method" }
                p { "Sample uses up to " (SAMPLE_WEEKS) " complete weeks of project issues with these current work types: " (scope.kinds.join(", ")) ". Done entries add one. Returning from Done to open work subtracts one. Assignments and cancellations leave weekly progress unchanged." }
                p.muted { "Project tracking began " (ctx.created.format("%Y-%m-%d %H:%M")) "."
                    @if let Some(date)=sample.coverage.first_observation { " First recorded cohort event: " (date.format("%Y-%m-%d %H:%M")) "." }
                    @if let Some(date)=sample.coverage.baseline { " Latest history baseline: " (date.format("%Y-%m-%d %H:%M")) "." }
                    " Current partial week beginning " (sample.end) " excluded."
                }
                @if modeled.is_some() {
                    p.muted { (TRIALS) " trials resample whole observed weeks with replacement (seed " (SEED) "). Each trial stops the first time all scoped issues are Done. Publication is a separate action." }
                }
                @if sample.coverage.incomplete == 0 && !sample.weeks.is_empty() {
                    p { (sample.weeks.len()) " complete weeks · " (sample.start) " through " (sample.end-Duration::days(1))
                        " · " (sample.positive()) " positive · average " (format!("{:.2}", sample.net() as f64/sample.weeks.len() as f64)) " net issues/week" }
                    div.table-wrap { table {
                        caption { "Weekly counts and issue events" }
                        thead { tr { th scope="col" { "Week of" } th scope="col" { "Done entries" } th scope="col" { "Reopenings" } th scope="col" { "Net" } } }
                        tbody { @for (day, week) in sample.weeks.iter().rev() { tr {
                            th scope="row" { a href=(format!("{url}/forecast/weeks/{day}")) { (day) } }
                            td { (week.entries) } td { (week.reopenings) } td { (week.net()) }
                        } } }
                    } }
                }
            }
            details open[pace.is_some()] {
                summary { "Manual pace scenario" }
                form method="get" action=(format!("{url}#forecast")) class="release-publish-controls" {
                    label { "Net issues per available week" input required type="number" name="pace" min="1" max=(u16::MAX) value=[pace]; }
                    button.secondary type="submit" { "Run scenario" }
                }
                @if let Some(p)=pace {
                    (estimate(&format!("Scenario at {p} net {} per available week", if p == 1 { "issue" } else { "issues" }), completion(&scope,&work,|| i64::from(p))))
                }
            }
        } },
    )
}

#[derive(Deserialize, Default)]
struct EventPage {
    page: Option<u32>,
}

#[derive(FromRow)]
struct Transition {
    issue: Uuid,
    number: i64,
    title: String,
    occurred: NaiveDateTime,
    old_status: Option<String>,
    new_status: String,
}

async fn events(
    State(state): State<AppState>,
    auth: Auth,
    Path((release, week)): Path<(Uuid, NaiveDate)>,
    Query(query): Query<EventPage>,
) -> AppResult<Html<String>> {
    let mut tx = snapshot(&state.db).await?;
    let ctx = context(&mut tx, auth.user_id, release).await?;
    if !ctx.paid {
        return Err(AppError::Forbidden);
    }
    if ctx.published {
        return Err(AppError::NotFound);
    }
    let scope = scope(&mut tx, auth.user_id, release).await?;
    let sample = sample(&mut tx, auth.user_id, &ctx, &scope).await?;
    if sample.coverage.incomplete > 0 || !sample.weeks.contains_key(&week) {
        return Err(AppError::NotFound);
    }
    let page = query.page.unwrap_or(1).max(1);
    let sql = format!("SELECT i.id AS issue,i.number,i.title,e.occurred_at AT TIME ZONE $6 AS occurred,
        e.old_status,e.new_status {EVENT_FILTER} ORDER BY e.occurred_at DESC,e.id DESC LIMIT 26 OFFSET $7");
    let mut rows: Vec<Transition> = sqlx::query_as(&sql)
        .bind(auth.user_id)
        .bind(ctx.project)
        .bind(&scope.kinds)
        .bind(week)
        .bind(week + Duration::weeks(1))
        .bind(&ctx.timezone)
        .bind(i64::from(page - 1) * 25)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let more = rows.len() > 25;
    rows.truncate(25);
    let base = format!("/releases/{release}/forecast/weeks/{week}");
    let counts = sample.weeks[&week];
    Ok(ui::page_in_project(
        &auth,
        "Forecast source events",
        "releases",
        html! {
            a href=(format!("/releases/{release}#forecast")) { "Back to release readiness" }
            h1 { "Issue events for the week of " (week) }
            p { (counts.entries) (if counts.entries == 1 { " Done entry · " } else { " Done entries · " }) (counts.reopenings) (if counts.reopenings == 1 { " reopening · " } else { " reopenings · " }) (counts.net()) " net" }
            @if !rows.is_empty() {
                div.table-wrap { table {
                    thead { tr { th scope="col" { "Time (" (ctx.timezone) ")" } th scope="col" { "Issue" } th scope="col" { "Transition" } } }
                    tbody { @for row in &rows { tr {
                        td { (row.occurred.format("%Y-%m-%d %H:%M:%S")) }
                        td { a href=(format!("/issues/{}#history",row.issue)) { (ctx.key) "-" (row.number) " · " (row.title) } }
                        td { (super::status_label(row.old_status.as_deref().unwrap_or("Created"))) " → " (super::status_label(&row.new_status)) }
                    } } }
                } }
            } @else { p.muted { @if page == 1 { "No Done entries or reopenings were recorded in this cohort this week." } @else { "No more events." } } }
            nav.toolbar aria-label="Source event pages" {
                @if page > 1 { a href=(format!("{base}?page={}",page-1)) { "Newer events" } }
                @if more { a href=(format!("{base}?page={}",page.saturating_add(1))) { "Older events" } }
            }
        },
        Some(ctx.project),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 2, 2).unwrap()
    }

    fn fixed(total: i64, done: i64) -> Scope {
        Scope {
            total,
            done,
            kinds: vec!["feature".into()],
        }
    }

    fn history(net: &[i64]) -> Sample {
        let weeks = net
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                (
                    first() + Duration::weeks(i as i64),
                    Week {
                        entries: n.max(0),
                        reopenings: (-n).max(0),
                    },
                )
            })
            .collect();
        Sample {
            start: first(),
            end: first() + Duration::weeks(net.len() as i64),
            weeks,
            coverage: Coverage {
                first_observation: Some(first().and_time(NaiveTime::MIN)),
                baseline: None,
                incomplete: 0,
            },
        }
    }

    #[test]
    fn boundaries_keep_only_complete_local_calendar_weeks() {
        assert_eq!(
            first_full_week(first().and_hms_opt(0, 0, 0).unwrap()),
            first()
        );
        assert_eq!(
            first_full_week(first().and_hms_opt(0, 0, 1).unwrap()),
            first() + Duration::weeks(1)
        );
        let sunday = first() + Duration::days(6);
        assert_eq!(
            first_full_week(sunday.and_hms_opt(23, 59, 59).unwrap()),
            first() + Duration::weeks(1)
        );
    }

    #[test]
    fn readiness_band_labels_coincident_and_separate_quantiles() {
        let start = first();
        let later = start + Duration::weeks(1);
        let same_week = interval(start, start, start).into_string();
        assert!(same_week.contains("P50 / P85"));
        assert!(same_week.contains(&(start + Duration::days(6)).format("%d %b %Y").to_string()));
        let separate = interval(start, start, later).into_string();
        assert!(separate.contains(">P50</text>") && separate.contains(">P85</text>"));
        assert!(separate.contains(&format!("85th-percentile week of {later}")));
    }

    #[test]
    fn gates_on_complete_weeks_positive_weeks_and_mean_separately() {
        assert!(
            history(&[1; 11])
                .unavailable()
                .unwrap()
                .contains("12 required")
        );
        assert!(
            history(&[10, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                .unavailable()
                .unwrap()
                .contains("3 required")
        );
        assert!(
            history(&[1, 1, 1, -3, 0, 0, 0, 0, 0, 0, 0, 0])
                .unavailable()
                .unwrap()
                .contains("must be positive")
        );
        let mut h = history(&[1, 1, 1, -2, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(h.unavailable().is_none());
        h.coverage.incomplete = 1;
        assert!(h.unavailable().unwrap().contains("lack a complete"));
    }

    #[test]
    fn seeded_whole_weeks_reproduce_example_and_shift_around_vacation() {
        let h = history(&[0, 2, 1, 3, 1, 0, 2, 1, 2, 1, 0, 2]);
        let work = working_weeks(first(), &BTreeSet::new());
        let demo = fixed(6, 0);
        let result = percentiles(&h, &demo, &work);
        assert_eq!(result, [Some(work[4]), Some(work[6])]);
        assert_eq!(result, percentiles(&h, &demo, &work));
        let off = BTreeSet::from([work[2]]);
        assert_eq!(
            percentiles(&h, &demo, &working_weeks(first(), &off)),
            [Some(work[5]), Some(work[7])]
        );
    }

    #[test]
    fn clamps_signed_work_and_keeps_unfinished_trials_in_the_denominator() {
        let work = working_weeks(first(), &BTreeSet::new());
        let mut moves = [-50, 0, 4, 100].into_iter();
        assert_eq!(
            completion(&fixed(5, 3), &work, || moves.next().unwrap()),
            Some(work[3])
        );
        // Two available weeks, each with a 1/3 chance to finish. More than half,
        // but fewer than 85%, reach Done before the horizon.
        let h = history(&[1, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0]);
        assert_eq!(
            percentiles(&h, &fixed(1, 0), &work[..2]),
            [Some(work[1]), None]
        );
        assert_eq!(percentiles(&h, &fixed(99, 0), &work), [None, None]);
    }

    #[sqlx::test]
    async fn sourced_draft_uses_paid_project_events_and_respects_calendar(db: PgPool) {
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email,plan,timezone)
            VALUES ('forecast@example.test','plus','America/Los_Angeles') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let project:Uuid=sqlx::query_scalar("INSERT INTO projects(owner_id,name,key,created_at)
            SELECT id,'Model','MD',(date_trunc('week',now() AT TIME ZONE timezone)-interval '12 weeks')
                AT TIME ZONE timezone FROM users WHERE id=$1 RETURNING id")
            .bind(owner).fetch_one(&db).await.unwrap();
        let release: Uuid = sqlx::query_scalar(
            "INSERT INTO releases(owner_id,project_id,name) VALUES($1,$2,'Draft') RETURNING id",
        )
        .bind(owner)
        .bind(project)
        .fetch_one(&db)
        .await
        .unwrap();
        let issue:Uuid=sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title) VALUES($1,$2,1,'Remaining') RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        sqlx::query("INSERT INTO release_members(owner_id,project_id,release_id,issue_id) VALUES($1,$2,$3,$4)")
            .bind(owner).bind(project).bind(release).bind(issue).execute(&db).await.unwrap();
        // Archived, outside this release, but in its current work-type cohort.
        let source:Uuid=sqlx::query_scalar("INSERT INTO issues(owner_id,project_id,number,title,status,archived) VALUES($1,$2,2,'Storage', 'done',true) RETURNING id")
            .bind(owner).bind(project).fetch_one(&db).await.unwrap();
        let unobserved = forecast(&db, owner, release, None, true)
            .await
            .unwrap()
            .into_string();
        assert!(
            unobserved.contains("No native status history")
                && !unobserved.contains("12 complete weeks")
        );
        // Begin the sample at a real cohort event; idle weeks after it remain observed.
        sqlx::query(
            "INSERT INTO issue_events(owner_id,project_id,issue_id,new_status,action,occurred_at)
            SELECT $1,$2,$3,'ready','create',created_at FROM projects WHERE owner_id=$1 AND id=$2",
        )
        .bind(owner)
        .bind(project)
        .bind(source)
        .execute(&db)
        .await
        .unwrap();
        // Place the last event on the local Sunday night (Monday UTC).
        sqlx::query("INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,occurred_at)
            SELECT $1,$2,$3,'ready','done',(date_trunc('week',now() AT TIME ZONE timezone)
            - make_interval(weeks=>n) - interval '30 minutes') AT TIME ZONE timezone
            FROM users CROSS JOIN generate_series(0,2) n WHERE id=$1")
            .bind(owner).bind(project).bind(source).execute(&db).await.unwrap();
        // Cancellation removes work from scope but does not reopen it.
        sqlx::query("INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,occurred_at)
            SELECT $1,$2,$3,'done','canceled',(date_trunc('week',now() AT TIME ZONE timezone)
            - interval '25 minutes') AT TIME ZONE timezone FROM users WHERE id=$1")
            .bind(owner).bind(project).bind(source).execute(&db).await.unwrap();
        let current: NaiveDate = sqlx::query_scalar(
            "SELECT date_trunc('week',now() AT TIME ZONE timezone)::date FROM users WHERE id=$1",
        )
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO roadmap_weeks_off(owner_id,project_id,week_of) VALUES ($1,$2,$3)")
            .bind(owner)
            .bind(project)
            .bind(current + Duration::weeks(1))
            .execute(&db)
            .await
            .unwrap();
        let read = |pace, model| forecast(&db, owner, release, pace, model);
        let normal = read(None, false).await.unwrap().into_string();
        assert!(normal.contains("12 complete weeks") && normal.contains("3 positive"));
        assert!(normal.contains("Model fixed-scope") && !normal.contains("Median readiness"));
        let projected = read(Some(1), true).await.unwrap().into_string();
        assert!(
            projected.contains("Median readiness")
                && projected.contains("85th-percentile readiness")
        );
        assert!(projected.contains(&format!(
            "Scenario at 1 net issue per available week</strong> · Week of {}",
            current + Duration::weeks(2)
        )));
        let auth = Auth {
            user_id: owner,
            email: "forecast@example.test".into(),
            csrf: String::new(),
        };
        let state = AppState::for_test(db.clone());
        let eventweek = current - Duration::weeks(1);
        let rows = events(
            State(state.clone()),
            auth.clone(),
            Path((release, eventweek)),
            Query(EventPage::default()),
        )
        .await
        .unwrap()
        .0;
        assert!(
            rows.contains("1 Done entry · 0 reopenings · 1 net")
                && rows.contains(&format!("/issues/{source}#history"))
        );
        assert!(matches!(
            events(
                State(state.clone()),
                auth.clone(),
                Path((release, current)),
                Query(EventPage::default())
            )
            .await,
            Err(AppError::NotFound)
        ));
        assert!(matches!(
            forecast(&db, Uuid::new_v4(), release, None, true).await,
            Err(AppError::NotFound)
        ));
        let added: Uuid = sqlx::query_scalar(
            "INSERT INTO issues(owner_id,project_id,number,title)
            VALUES($1,$2,3,'New scope') RETURNING id",
        )
        .bind(owner)
        .bind(project)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO release_members(owner_id,project_id,release_id,issue_id) VALUES($1,$2,$3,$4)")
            .bind(owner).bind(project).bind(release).bind(added).execute(&db).await.unwrap();
        let changed = read(Some(1), true).await.unwrap().into_string();
        assert!(changed.contains("2 issues remaining"));
        assert!(changed.contains(&format!(
            "Scenario at 1 net issue per available week</strong> · Week of {}",
            current + Duration::weeks(3)
        )));
        // A snapshot-only import has no observation date or recoverable transitions.
        sqlx::query("INSERT INTO transfer_records(owner_id,project_id,kind,source_id,local_id,base_hash,path)
            VALUES($1,$2,'issue',$3,$3,'test','Issues/New.md')").bind(owner).bind(project).bind(added)
            .execute(&db).await.unwrap();
        let no_history = read(Some(1), true).await.unwrap().into_string();
        assert!(
            no_history.contains("lack a complete status history")
                && !no_history.contains("Median readiness")
        );
        assert!(no_history.contains("Scenario at 1"));
        sqlx::query("UPDATE issues SET status='done' WHERE id=ANY($1)")
            .bind(vec![issue, added])
            .execute(&db)
            .await
            .unwrap();
        assert!(
            read(None, true)
                .await
                .unwrap()
                .into_string()
                .contains("already Done")
        );
        sqlx::query("UPDATE users SET plan='free' WHERE id=$1")
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();
        let free = read(Some(5), true).await.unwrap().into_string();
        assert!(
            free.contains("Plus and Max")
                && !free.contains("Run scenario")
                && !free.contains("4096")
        );
        assert!(matches!(
            events(
                State(state),
                auth,
                Path((release, eventweek)),
                Query(EventPage::default())
            )
            .await,
            Err(AppError::Forbidden)
        ));
    }
}
