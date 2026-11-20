use std::collections::BTreeMap;

use axum::{
    Router,
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse},
    routing::get,
};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, store, ui};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/insights", get(insights))
        .route("/assets/cycle-observation.js", get(observation_script))
}

async fn observation_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("observation.js"),
    )
}

const KINDS: [(&str, &str); 4] = [
    ("feature", "Feature"),
    ("bug", "Bug"),
    ("maintenance", "Maintenance"),
    ("security", "Security"),
];

#[derive(Default, Deserialize)]
struct Filters {
    project: Option<String>,
    period: Option<String>,
    kind: Option<String>,
    view: Option<String>,
    page: Option<usize>,
    open_page: Option<usize>,
    records: Option<String>,
}

#[derive(FromRow)]
struct Issue {
    id: Uuid,
    key: String,
    title: String,
    kind: String,
    status: String,
    created_at: DateTime<Utc>,
    first_start: Option<DateTime<Utc>>,
    finish: Option<DateTime<Utc>>,
    finish_date: Option<NaiveDate>,
    blockers: Vec<String>,
    blocker_changes: sqlx::types::Json<Vec<BlockerChange>>,
}

#[derive(Deserialize)]
struct BlockerChange {
    id: Uuid,
    key: String,
    at: DateTime<Utc>,
    added: bool,
}

impl BlockerChange {
    fn label(&self) -> &'static str {
        if self.added {
            "Added blocker"
        } else {
            "Removed blocker"
        }
    }
}

impl Issue {
    fn history(&self) -> String {
        format!("/issues/{}#history", self.id)
    }
}

struct Observation<'a> {
    issue: &'a Issue,
    finish_date: NaiveDate,
    finish: DateTime<Utc>,
    start: DateTime<Utc>,
    days: f64,
}

#[derive(Default)]
struct Cohort<'a> {
    done: Vec<Observation<'a>>,
    open: Vec<&'a Issue>,
    unknown: Vec<&'a Issue>,
}

fn days_between(start: Option<DateTime<Utc>>, end: DateTime<Utc>) -> Option<f64> {
    let elapsed = end - start?;
    (elapsed >= Duration::zero()).then(|| elapsed.num_milliseconds() as f64 / 86_400_000.0)
}

fn cohort(rows: &[Issue]) -> Cohort<'_> {
    let mut result = Cohort::default();
    for issue in rows {
        if issue.status != "done" {
            result.open.push(issue);
            continue;
        }
        match (issue.first_start, issue.finish, issue.finish_date) {
            (Some(start), Some(finish), Some(finish_date)) if start <= finish => {
                result.done.push(Observation {
                    issue,
                    finish_date,
                    finish,
                    start,
                    days: days_between(Some(start), finish).unwrap(),
                });
            }
            _ => result.unknown.push(issue),
        }
    }
    result
        .done
        .sort_by_key(|o| (std::cmp::Reverse(o.finish), o.issue.id));
    result
        .open
        .sort_by_key(|i| (i.first_start.is_none(), i.first_start, i.created_at, i.id));
    result
}

fn window_start(plan: &str, period: &str, today: NaiveDate) -> Option<NaiveDate> {
    let entitlement = super::history_start(today, plan);
    let requested = match period {
        "4w" => 28,
        "6w" => 42,
        "8w" => 56,
        _ => return entitlement,
    };
    entitlement.max(Some(today - Duration::days(requested - 1)))
}

fn quantile(sorted: &[f64], fraction: f64) -> f64 {
    let index = (sorted.len() - 1) as f64 * fraction;
    let lower = index.floor() as usize;
    let upper = index.ceil() as usize;
    sorted[lower] + (sorted[upper] - sorted[lower]) * index.fract()
}

// Apply the completion window after reconstructing first start across the full native timeline.
// A snapshot without an observed transition cannot manufacture a start or a finish.
async fn observations(
    db: &PgPool,
    owner: Uuid,
    project: Option<Uuid>,
    kind: &str,
    lower: Option<NaiveDate>,
    now: DateTime<Utc>,
    timezone: &str,
) -> AppResult<Vec<Issue>> {
    Ok(sqlx::query_as(
        "WITH recorded AS (
             SELECT i.id, p.key || '-' || i.number::text AS key, i.title, i.kind, i.status,
                    i.created_at, LEAST(i.started_at, e.first_start) AS first_start,
                    GREATEST(i.completed_at, CASE WHEN latest.new_status='done'
                        AND latest.action NOT IN ('import','baseline','import_baseline')
                        THEN latest.occurred_at END) AS finish,
                    ARRAY(SELECT p.key || '-' || b.number::text
                          FROM issue_blockers link JOIN issues b ON b.id=link.blocker_id
                            AND b.owner_id=i.owner_id AND b.project_id=i.project_id
                          WHERE link.issue_id=i.id AND link.owner_id=i.owner_id
                            AND link.project_id=i.project_id AND NOT b.archived
                            AND b.status IN ('ready','backlog','in_progress')
                          ORDER BY b.number) AS blockers,
                    (SELECT COALESCE(jsonb_agg(jsonb_build_object('id', b.id,
                              'key',p.key || '-' || b.number::text, 'at',ev.occurred_at,
                              'added', ev.after_state #>> '{relation,present}' = 'true')
                              ORDER BY ev.occurred_at, ev.revision_after, ev.id), '[]'::jsonb)
                     FROM issue_events ev JOIN issues b
                       ON b.owner_id=i.owner_id AND b.project_id=i.project_id
                      AND b.id::text=ev.after_state #>> '{relation,blocker_id}'
                     WHERE ev.owner_id=i.owner_id AND ev.project_id=i.project_id AND ev.issue_id=i.id
                       AND ev.action IN ('blocker','undo') AND ev.occurred_at <= $5
                       AND ev.after_state #>> '{relation,present}' IN ('true','false')
                       AND ($4::date IS NULL OR ev.occurred_at >= ($4::date::timestamp AT TIME ZONE $6))
                    ) AS blocker_changes
             FROM issues i JOIN projects p ON p.id=i.project_id AND p.owner_id=i.owner_id
             LEFT JOIN LATERAL (
                 SELECT min(occurred_at) AS first_start FROM issue_events
                 WHERE owner_id=i.owner_id AND project_id=i.project_id AND issue_id=i.id
                   AND new_status='in_progress' AND old_status IS DISTINCT FROM new_status
                   AND action NOT IN ('import','baseline','import_baseline')
             ) e ON true
             LEFT JOIN LATERAL (
                 SELECT new_status, action, occurred_at FROM issue_events
                 WHERE owner_id=i.owner_id AND project_id=i.project_id AND issue_id=i.id
                   AND old_status IS DISTINCT FROM new_status
                 ORDER BY occurred_at DESC, revision_after DESC NULLS LAST, id DESC LIMIT 1
             ) latest ON true
             WHERE i.owner_id=$1 AND ($2::uuid IS NULL OR i.project_id=$2)
               AND ($3='all' OR i.kind=$3) AND NOT i.archived AND i.status<>'canceled'
         )
         SELECT recorded.*, (finish AT TIME ZONE $6)::date AS finish_date FROM recorded
         WHERE status<>'done' OR
               ((finish <= $5 OR (finish IS NULL AND $4::date IS NULL)) AND
                ($4::date IS NULL OR finish >= ($4::date::timestamp AT TIME ZONE $6)))",
    )
    .bind(owner)
    .bind(project)
    .bind(kind)
    .bind(lower)
    .bind(now)
    .bind(timezone)
    .fetch_all(db)
    .await?)
}

struct Selection<'a> {
    project: Option<Uuid>,
    kind: &'a str,
    period: &'a str,
    view: &'a str,
}

impl Selection<'_> {
    fn page_url(&self, page: usize, open_page: usize, section: &str) -> String {
        let project = self
            .project
            .map(|p| p.to_string())
            .unwrap_or_else(|| "all".into());
        format!(
            "/insights?project={project}&kind={}&period={}&view={}&page={page}&open_page={open_page}&records={section}#{section}",
            self.kind, self.period, self.view
        )
    }
}

fn project_selection(
    plan: &str,
    project: Option<&str>,
    projects: &[store::Project],
) -> AppResult<Option<Uuid>> {
    match project {
        Some("all") if plan == "max" => Ok(None),
        Some("all") => Err(AppError::Forbidden),
        Some(raw) => {
            let id = Uuid::parse_str(raw)
                .map_err(|_| AppError::BadRequest("Choose a valid project.".into()))?;
            if projects.iter().any(|p| p.id == id) {
                Ok(Some(id))
            } else {
                Err(AppError::NotFound)
            }
        }
        None if plan == "max" => Ok(None),
        None => Ok(projects
            .iter()
            .find(|p| !p.archived)
            .or(projects.first())
            .map(|p| p.id)),
    }
}

async fn insights(
    State(state): State<AppState>,
    auth: Auth,
    Query(filters): Query<Filters>,
) -> AppResult<Html<String>> {
    let (plan, timezone, today, now): (String, String, NaiveDate, DateTime<Utc>) = sqlx::query_as(
        "SELECT plan, timezone, (now() AT TIME ZONE timezone)::date, now() FROM users WHERE id=$1",
    )
    .bind(auth.user_id)
    .fetch_one(&state.db)
    .await?;
    let projects = store::projects(&state.db, auth.user_id).await?;
    let project = project_selection(&plan, filters.project.as_deref(), &projects)?;
    let kind = filters.kind.as_deref().unwrap_or("all");
    let view = filters.view.as_deref().unwrap_or("time");
    let period = filters
        .period
        .as_deref()
        .unwrap_or(if plan == "free" { "plan" } else { "6w" });
    if !matches!(view, "time" | "grouped")
        || !matches!(period, "plan" | "4w" | "6w" | "8w")
        || !(kind == "all" || KINDS.iter().any(|(k, _)| *k == kind))
    {
        return Err(AppError::BadRequest(
            "Choose a listed chart view, period and work type.".into(),
        ));
    }
    if projects.is_empty() {
        return Ok(ui::section_page(
            &auth,
            "Insights",
            "insights",
            html! {
                header.view-head { h1 { "Insights" } }
                section.empty-state { h2 { "Track your first project" }
                    p { "Cycle time appears when an issue moves from In progress to Done." }
                    a.action-primary href="/projects#new-project" { "Create a project" }
                }
            },
            None,
        ));
    }
    let selection = Selection {
        project,
        kind,
        period,
        view,
    };
    let lower = window_start(&plan, period, today);
    let rows = observations(
        &state.db,
        auth.user_id,
        project,
        kind,
        lower,
        now,
        &timezone,
    )
    .await?;
    let data = cohort(&rows);
    let title = project
        .and_then(|id| projects.iter().find(|p| p.id == id))
        .map(|p| p.name.as_str())
        .unwrap_or("All projects");
    let page = filters
        .page
        .unwrap_or(1)
        .max(1)
        .min(data.done.len().div_ceil(PAGE_SIZE).max(1));
    let open_page = filters
        .open_page
        .unwrap_or(1)
        .max(1)
        .min(data.open.len().div_ceil(PAGE_SIZE).max(1));
    let mut sorted_days = data.done.iter().map(|o| o.days).collect::<Vec<_>>();
    sorted_days.sort_by(f64::total_cmp);
    Ok(ui::section_page(
        &auth,
        "Insights",
        "insights",
        html! {
            header.view-head.cycle-top {
                h1 { "Insights" }
                (controls(&selection, &projects, &plan, title, lower, today, &timezone))
            }
            section.content-section.cycle-study aria-labelledby="cycle-heading" {
                @if data.done.is_empty() {
                    h2 id="cycle-heading" { "Cycle time" }
                    div.cycle-empty {
                        strong { "No measured completions" }
                        @if !data.unknown.is_empty() {
                            p { "Completed issues are missing a start or finish date." }
                        } @else if !data.open.is_empty() {
                            p { "Cycle time appears when an issue moves from In progress to Done." }
                        } @else {
                            p { "Try a different period or work type." }
                        }
                    }
                } @else {
                    div.cycle-instrument {
                        div.cycle-heading {
                            h2 id="cycle-heading" { "Median cycle time" }
                            div.cycle-median {
                                strong { (duration_label(quantile(&sorted_days, 0.5))) }
                                a href=(selection.page_url(1, open_page, "observations")) {
                                    (data.done.len()) " " (if data.done.len() == 1 { "completion" } else { "completions" })
                                }
                            }
                        }
                        (plot(&data.done, view, lower.unwrap_or_else(|| data.done.last().unwrap().finish_date), today))
                        (observation(&data.done[0]))
                    }
                    script defer src="/assets/cycle-observation.js" {}
                }
                @if !data.unknown.is_empty() {
                    details { summary { (data.unknown.len()) " excluded with missing or inconsistent dates" }
                        ul { @for issue in &data.unknown {
                            li { a href=(issue.history()) { (issue.key) " · " (issue.title) }
                                " — " (match (issue.first_start, issue.finish) {
                                    (None, _) => "Unknown start",
                                    (_, None) => "Unknown completion",
                                    _ => "Start follows completion",
                                })
                            }
                        } }
                    }
                }
            }
            (sources(&selection, &data, now, page, open_page, filters.records.as_deref()))
        },
        project,
    ))
}

fn controls(
    s: &Selection<'_>,
    projects: &[store::Project],
    plan: &str,
    project_name: &str,
    lower: Option<NaiveDate>,
    today: NaiveDate,
    timezone: &str,
) -> Markup {
    let entitlement = match plan {
        "max" => "All recorded history",
        "plus" => "Last 12 months",
        _ => "Last 30 days",
    };
    let period = match s.period {
        "4w" => "4 weeks",
        "6w" => "6 weeks",
        "8w" => "8 weeks",
        _ => entitlement,
    };
    html! {
        details.cycle-filter {
            summary {
                span.cycle-filter-action { (ui::icon("settings")) "Filters" }
                span.cycle-filter-scope {
                    (project_name) " · " (period)
                    @if let Some((_, label)) = KINDS.iter().find(|(kind, _)| *kind == s.kind) { " · " (label) }
                    @if s.view == "grouped" { " · By work type" }
                }
            }
            form.toolbar method="get" action="/insights" {
                label.field { "Project" select name="project" {
                    @if plan == "max" { option value="all" selected[s.project.is_none()] { "All projects" } }
                    @for p in projects { option value=(p.id) selected[s.project==Some(p.id)] { (p.name) @if p.archived { " (archived)" } } }
                } }
                label.field { "Period" select name="period" {
                    option value="plan" selected[s.period=="plan"] { (entitlement) }
                    @for (id, label) in [("4w","4 weeks"), ("6w","6 weeks"), ("8w","8 weeks")] {
                        @if plan != "free" || id=="4w" { option value=(id) selected[s.period==id] { (label) } }
                    }
                } }
                label.field { "Work type" select name="kind" {
                    option value="all" selected[s.kind=="all"] { "All types" }
                    @for (id, label) in KINDS { option value=(id) selected[s.kind==id] { (label) } }
                } }
                label.field { "View" select name="view" {
                    option value="time" selected[s.view=="time"] { "Completion date" }
                    option value="grouped" selected[s.view=="grouped"] { "By work type" }
                } }
                button.action-primary type="submit" { "Apply filters" }
            }
            p.cycle-coverage {
                @if let Some(start) = lower { time datetime=(start) { (start.format("%d %b %Y")) } " – " }
                @else { "All history through " }
                time datetime=(today) { (today.format("%d %b %Y")) } " · " (timezone)
                br; "Measured from first In progress to latest Done."
            }
        }
    }
}

fn display_days(value: f64) -> String {
    if value > 0.0 && value < 0.01 {
        return "<0.01".into();
    }
    format!("{value:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn duration_label(days: f64) -> String {
    let value = display_days(days);
    let unit = if value == "1" { "day" } else { "days" };
    format!("{value} {unit}")
}

fn pct(value: f64) -> String {
    format!("{value:.3}%")
}

// Return consecutive groups only. A missing week is a gap, not a zero-day observation.
fn weekly_bands(rows: &[Observation<'_>]) -> Vec<Vec<(NaiveDate, [f64; 3])>> {
    let mut weeks = BTreeMap::<NaiveDate, Vec<f64>>::new();
    for o in rows {
        let monday = o.finish_date
            - Duration::days(i64::from(o.finish_date.weekday().num_days_from_monday()));
        weeks.entry(monday).or_default().push(o.days);
    }
    let mut segments: Vec<Vec<(NaiveDate, [f64; 3])>> = Vec::new();
    for (week, mut days) in weeks {
        if days.len() < 4 {
            continue;
        }
        days.sort_by(f64::total_cmp);
        let point = (
            week,
            [
                quantile(&days, 0.25),
                quantile(&days, 0.5),
                quantile(&days, 0.75),
            ],
        );
        if let Some(last) = segments.last_mut()
            && last
                .last()
                .is_some_and(|(prev, _)| week - *prev == Duration::days(7))
        {
            last.push(point);
            continue;
        }
        segments.push(vec![point]);
    }
    segments
}

fn observation(o: &Observation<'_>) -> Markup {
    html! {
        aside.cycle-readout aria-labelledby="cycle-readout-heading" {
            span.cycle-readout-label id="cycle-readout-heading" { "Latest completion" }
            a.cycle-history-link href=(o.issue.history()) {
                code data-cycle-field="key" { (o.issue.key) }
                span data-cycle-field="title" { (o.issue.title) }
                (ui::icon("arrow"))
            }
            span.cycle-readout-days data-cycle-field="elapsed" { (duration_label(o.days)) }
        }
    }
}

fn mark(o: &Observation<'_>, x: f64, y: f64, dx: f64, current: bool) -> Markup {
    let mut label = format!(
        "{} · {} · {} · {} · completed {}",
        o.issue.key,
        o.issue.title,
        o.issue.kind,
        duration_label(o.days),
        o.finish_date
    );
    if !o.issue.blockers.is_empty() {
        label.push_str(" · Current blockers: ");
        label.push_str(&o.issue.blockers.join(", "));
    }
    if let Some(last) = o.issue.blocker_changes.last() {
        label.push_str(&format!(
            " · {} {} on {}",
            last.label(),
            last.key,
            last.at.date_naive()
        ));
    }
    html! {
        a class=(if current { "cycle-point is-current" } else { "cycle-point" }) href=(o.issue.history()) tabindex="0" aria-label=(label)
            data-key=(&o.issue.key) data-title=(&o.issue.title) data-elapsed=(duration_label(o.days)) {
            title { (label) }
            g transform=(format!("translate({dx:.3} 0)")) color="var(--text, CanvasText)" {
                circle class="cycle-hit" cx=(pct(x)) cy=(y) r="12" fill="transparent" {}
                circle class="cycle-ring" cx=(pct(x)) cy=(y) r="8.5" fill="none" {}
                use href=(format!("#cycle-{}", o.issue.kind)) x=(pct(x)) y=(y) {}
            }
        }
    }
}

fn plot(rows: &[Observation<'_>], view: &str, first: NaiveDate, last: NaiveDate) -> Markup {
    let grouped = view == "grouped";
    let left = if grouped { 28.0 } else { 10.0 };
    let right = 95.0;
    let top = 18.0;
    let bottom = 186.0;
    let max_days = rows.iter().map(|o| o.days).fold(0.0, f64::max).max(0.04);
    let raw_step = max_days / 4.0;
    let power = 10_f64.powf(raw_step.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .find(|s| *s * power >= raw_step)
        .unwrap_or(10.0)
        * power;
    let ceiling = 4.0 * step;
    let span = (last - first).num_days().max(1);
    let x_date = |date: NaiveDate| {
        left + ((date - first).num_days() as f64 / span as f64).clamp(0.0, 1.0) * (right - left)
    };
    let y_days = |days: f64| bottom - days / ceiling * (bottom - top);
    let row_y = |i: usize| top + (i as f64 + 0.5) * (bottom - top) / KINDS.len() as f64;
    let bands = if grouped {
        Vec::new()
    } else {
        weekly_bands(rows)
    };
    // Spread coincident marks only perpendicular to the elapsed-days axis. Even
    // imported issues with identical timestamps retain their own link and hit area.
    let slot = |o: &Observation<'_>| {
        let group = if grouped {
            KINDS
                .iter()
                .position(|(k, _)| *k == o.issue.kind)
                .unwrap_or(0) as i32
        } else {
            o.finish_date.num_days_from_ce()
        };
        (group, (o.days * 100.0).round() as i64)
    };
    let mut slots = BTreeMap::new();
    for o in rows {
        slots.entry(slot(o)).or_insert((0_usize, 0_usize)).1 += 1;
    }
    let has_overlaps = slots.values().any(|(_, count)| *count > 1);
    let offsets = rows.iter().map(|o| {
        let (next, total) = slots.get_mut(&slot(o)).unwrap();
        let width = (7.0 * (*total - 1) as f64).min(24.0);
        let offset = if *total == 1 {
            0.0
        } else {
            width * (*next as f64 / (*total - 1) as f64 - 0.5)
        };
        *next += 1;
        offset
    });
    let date_format = if first.year() == last.year() {
        "%d %b"
    } else {
        "%b %y"
    };
    html! {
        figure.chart-frame.cycle-chart {
            div.cycle-axis-title { (if grouped { "Work type" } else { "Elapsed days" }) }
            svg width="100%" height="229" role="group" aria-label=(if grouped {
                "Elapsed days from first start to latest completion by work type. Select a mark to open its issue history."
            } else { "Elapsed days from first start to latest completion by finish date. Select a mark to open its issue history." }) {
                defs {
                    circle id="cycle-feature" r="4" fill="currentColor" {}
                    path id="cycle-bug" d="M0 -5 5 0 0 5 -5 0Z" fill="currentColor" {}
                    rect id="cycle-maintenance" x="-3.7" y="-3.7" width="7.4" height="7.4" fill="var(--canvas, Canvas)" stroke="currentColor" stroke-width="1.7" {}
                    path id="cycle-security" d="M0 -5 4.6 3.5 -4.6 3.5Z" fill="var(--canvas, Canvas)" stroke="currentColor" stroke-width="1.7" {}
                }
                @for tick in 0..=4 {
                    @let days = step * tick as f64;
                    @if grouped {
                        @let x = pct(left + (right-left) * f64::from(tick) / 4.0);
                        line x1=(x) x2=(x) y1=(top) y2=(bottom) stroke="currentColor" stroke-dasharray=(if tick==0 {"none"} else {"2 4"}) opacity="0.31" {}
                        text x=(x) y="211" text-anchor="middle" font-size="12" fill="currentColor" { (display_days(days)) "d" }
                    } @else {
                        @let y = y_days(days);
                        line x1=(pct(left)) x2=(pct(right)) y1=(y) y2=(y) stroke="currentColor" stroke-dasharray=(if tick==0 {"none"} else {"2 4"}) opacity="0.31" {}
                        text x=(pct(left-2.0)) y=(y+4.0) text-anchor="end" font-size="12" fill="currentColor" { (display_days(days)) "d" }
                    }
                }
                @if grouped {
                    @for (i, (_, label)) in KINDS.iter().enumerate() {
                        text x=(pct(left-2.0)) y=(row_y(i)+4.0) text-anchor="end" font-size="12" fill="currentColor" { (label) }
                    }
                } @else {
                    @for tick in 0..=2 {
                        @let date = first + Duration::days((last-first).num_days() * tick / 2);
                        @if tick == 0 || (last-first).num_days() >= 2 || (tick==2 && last!=first) {
                            text x=(pct(x_date(date))) y="211" text-anchor=(if tick==0 { "start" } else if tick==2 { "end" } else { "middle" }) font-size="12" fill="currentColor" { (date.format(date_format)) }
                        }
                    }
                    @for segment in &bands {
                        @let coords = |(week, q): &(NaiveDate, [f64;3]), i: usize| {
                            // Draw week summaries at the midpoint of the observed part of that week.
                            let a = (*week).max(first);
                            let b = (*week + Duration::days(6)).min(last);
                            let date = a + Duration::days((b-a).num_days() / 2);
                            (x_date(date), y_days(q[i]))
                        };
                        @if segment.len() > 1 {
                            @let path = |i:usize| segment.iter().map(|p| {let (x,y)=coords(p,i); format!("{x:.3},{y:.3}")}).collect::<Vec<_>>().join(" ");
                            @let upper_rev = segment.iter().rev().map(|p| {let(x,y)=coords(p,2);format!("{x:.3},{y:.3}")}).collect::<Vec<_>>().join(" ");
                            svg x="0" y="0" width="100%" height="229" viewBox="0 0 100 229" preserveAspectRatio="none" aria-hidden="true" {
                                polygon points=(format!("{} {upper_rev}", path(0))) fill="currentColor" opacity="0.13" {}
                                polyline points=(path(1)) stroke="var(--accent, currentColor)" stroke-width="1.6" stroke-dasharray="6 3" vector-effect="non-scaling-stroke" fill="none" {}
                            }
                        } @else {
                            @let (x, median)=coords(&segment[0],1);
                            @let (_, low)=coords(&segment[0],0);
                            @let (_, high)=coords(&segment[0],2);
                            line x1=(pct(x)) x2=(pct(x)) y1=(low) y2=(high) stroke="currentColor" stroke-width="8" opacity="0.2" {}
                            line x1=(pct(x-1.0)) x2=(pct(x+1.0)) y1=(median) y2=(median) stroke="var(--accent, currentColor)" stroke-width="1.6" {}
                        }
                    }
                }
                @for (n, (o, offset)) in rows.iter().zip(offsets).enumerate() {
                    @if grouped {
                        @let index = KINDS.iter().position(|(k,_)| *k == o.issue.kind).unwrap_or(0);
                        (mark(o, left + o.days/ceiling * (right-left), row_y(index)+offset, 0.0, n == 0))
                    } @else { (mark(o, x_date(o.finish_date), y_days(o.days), offset, n == 0)) }
                }
            }
            figcaption.chart-legend {
                @for (kind, label) in KINDS {
                    @if rows.iter().any(|o| o.issue.kind == kind) {
                        span {
                            svg width="14" height="14" viewBox="-7 -7 14 14" aria-hidden="true" { use href=(format!("#cycle-{kind}")) {} }
                            " " (label)
                        }
                    }
                }
                span.cycle-x-label { (if grouped { "Elapsed days" } else { "Completion date" }) }
                @if !bands.is_empty() { span { "Median and middle 50% (4+ completions/week)" } }
                @if has_overlaps { span { "Overlapping marks are offset" } }
            }
        }
    }
}

const PAGE_SIZE: usize = 20;

fn pagination(
    s: &Selection<'_>,
    total: usize,
    page: usize,
    open_page: usize,
    is_open: bool,
) -> Markup {
    let active = if is_open { open_page } else { page };
    let pages = total.div_ceil(PAGE_SIZE);
    if pages <= 1 {
        return html! {};
    }
    let url = |n| {
        if is_open {
            s.page_url(page, n, "open-age")
        } else {
            s.page_url(n, open_page, "observations")
        }
    };
    html! {
        nav.row aria-label=(if is_open { "Open issue pages" } else { "Completed issue pages" }) {
            @if active>1 { a href=(url(active-1)) { "Previous" } }
            span.muted { "Page " (active) " of " (pages) }
            @if active<pages { a href=(url(active+1)) { "Next" } }
        }
    }
}

fn status_label(status: &str) -> &str {
    match status {
        "in_progress" => "In progress",
        "backlog" => "Backlog",
        "ready" => "Ready",
        _ => status,
    }
}

fn timestamp(t: DateTime<Utc>) -> Markup {
    html! { time datetime=(t.to_rfc3339()) { (t.format("%Y-%m-%d %H:%M:%S")) } }
}

// On narrow screens the elapsed value expands the precise source timestamps.
fn timing_cell(
    kind: &str,
    key: &str,
    days: Option<f64>,
    first: (&str, DateTime<Utc>),
    last: (&str, Option<DateTime<Utc>>),
) -> Markup {
    let (first_name, first_date) = first;
    let (last_name, last_date) = last;
    let elapsed = days
        .map(duration_label)
        .unwrap_or_else(|| "Unknown duration".into());
    html! {
        td.cycle-kind {
            span { (kind) }
            span.cycle-mobile-when {
                " · "
                @if let Some(date) = last_date {
                    (last_name) " " time datetime=(date.to_rfc3339()) { (date.format("%d %b %Y")) }
                } @else { "Start unknown" }
            }
            details.cycle-timing {
                summary aria-label=(format!("{key} · {elapsed} · Exact timing")) {
                    @if let Some(value) = days { (display_days(value)) " d" } @else { "—" }
                }
                dl {
                    dt { (first_name) } dd { (timestamp(first_date)) " UTC" }
                    dt { (last_name) } dd { @if let Some(date) = last_date { (timestamp(date)) " UTC" } @else { "Unknown" } }
                }
            }
        }
    }
}

fn source(issue: &Issue) -> Markup {
    html! { a.cycle-source-link href=(issue.history()) { code { (issue.key) } span { (&issue.title) } }
        @if !issue.blockers.is_empty() {
            br; a href=(format!("/issues/{}#blockers", issue.id)) { "Blocked by " (issue.blockers.join(", ")) }
        }
        @if !issue.blocker_changes.is_empty() {
            details { summary { "Blocker history (" (issue.blocker_changes.len()) ")" }
                ul { @for change in &issue.blocker_changes.0 {
                    li { (timestamp(change.at)) " UTC · " (change.label()) " "
                        a href=(format!("/issues/{}", change.id)) { (change.key) }
                    }
                } }
            }
        }
    }
}

fn sources(
    s: &Selection<'_>,
    data: &Cohort<'_>,
    now: DateTime<Utc>,
    page: usize,
    open_page: usize,
    records: Option<&str>,
) -> Markup {
    html! {
        @if !data.done.is_empty() {
            details.cycle-records open[records == Some("observations") || (records.is_none() && page > 1)] {
                summary { (ui::icon("chevron")) "Completed issues" span { (data.done.len()) } }
                div.table-wrap.cycle-table id="observations" tabindex="-1" { table.data-table aria-label="Completed issues and cycle times" {
                    thead { tr { th scope="col" { "Issue" } th scope="col" { "Work type" } th scope="col" { "First start (UTC)" } th scope="col" { "Latest Done (UTC)" } th scope="col" { "Days" } } }
                    tbody {
                        @for row in data.done.iter().skip((page-1)*PAGE_SIZE).take(PAGE_SIZE) {
                            tr { td class="cycle-source" { (source(row.issue)) }
                                (timing_cell(&row.issue.kind, &row.issue.key, Some(row.days), ("Started", row.start), ("Done", Some(row.finish))))
                                td class="cycle-date" data-label="Started" { (timestamp(row.start)) }
                                td class="cycle-date" data-label="Done" { (timestamp(row.finish)) }
                                td class="cycle-duration" { (display_days(row.days)) }
                            }
                        }
                    }
                } }
                (pagination(s, data.done.len(), page, open_page, false))
            }
        }
        details.cycle-records open[records == Some("open-age") || (records.is_none() && open_page > 1)] {
            summary { (ui::icon("chevron")) "Open issue age" span { (data.open.len()) } }
            div id="open-age" tabindex="-1" {
                @if data.open.is_empty() { p.muted { "No open issues in this selection." } }
                @else {
                    p.muted { "Time since first In progress, including reopened work." }
                    div.table-wrap.cycle-table { table.data-table aria-label="Open issues and time since first start" {
                        thead { tr { th scope="col" { "Issue" } th scope="col" { "Status" } th scope="col" { "Created (UTC)" } th scope="col" { "First start (UTC)" } th scope="col" { "Days since start" } } }
                        tbody {
                            @for issue in data.open.iter().skip((open_page-1)*PAGE_SIZE).take(PAGE_SIZE) {
                                @let age = days_between(issue.first_start, now);
                                tr {
                                    td class="cycle-source" { (source(issue)) }
                                    (timing_cell(status_label(&issue.status), &issue.key, age, ("Created", issue.created_at), ("Started", issue.first_start)))
                                    td class="cycle-date" data-label="Created" { (timestamp(issue.created_at)) }
                                    td class="cycle-date" data-label="Started" { @if let Some(start)=issue.first_start { (timestamp(start)) } @else { "—" } }
                                    td class="cycle-duration" { @if let Some(age)=age { (display_days(age)) } @else { "—" } }
                                }
                            }
                        }
                    } }
                    (pagination(s, data.open.len(), page, open_page, true))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_window_clamps_requested_period_to_plan() {
        let today = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        assert_eq!(
            window_start("free", "plan", today),
            Some(NaiveDate::from_ymd_opt(2024, 2, 1).unwrap())
        );
        assert_eq!(
            window_start("free", "8w", today),
            window_start("free", "plan", today)
        );
        assert_eq!(
            window_start("plus", "plan", today),
            Some(NaiveDate::from_ymd_opt(2023, 3, 2).unwrap())
        );
        assert_eq!(window_start("max", "plan", today), None);
        assert_eq!(
            window_start("max", "4w", today),
            Some(today - Duration::days(27))
        );
    }

    #[test]
    fn quantiles_interpolate_fractional_calendar_days() {
        assert_eq!(quantile(&[0.5, 1.0, 4.0, 5.5], 0.5), 2.5);
        assert_eq!(quantile(&[0.5, 1.0, 4.0, 5.5], 0.25), 0.875);
        assert_eq!(quantile(&[0.8], 0.5), 0.8);
    }

    #[test]
    fn completion_excludes_unknown_and_reversed_starts() {
        let end = DateTime::from_timestamp(1_000_000, 0).unwrap();
        assert_eq!(days_between(None, end), None);
        assert_eq!(days_between(Some(end + Duration::days(1)), end), None);
        assert_eq!(
            days_between(Some(end - Duration::hours(6)), end),
            Some(0.25)
        );
        assert_eq!(days_between(Some(end), end), Some(0.0));
    }

    #[test]
    fn all_project_cohorts_require_max_and_unknown_ids_are_private() {
        let p = store::Project {
            id: Uuid::new_v4(),
            owner_id: Uuid::new_v4(),
            name: "Papertrail".into(),
            key: "PT".into(),
            archived: false,
        };
        let id = p.id;
        let projects = [p];
        assert!(matches!(
            project_selection("plus", Some("all"), &projects),
            Err(AppError::Forbidden)
        ));
        assert!(matches!(
            project_selection("max", Some(&Uuid::new_v4().to_string()), &projects),
            Err(AppError::NotFound)
        ));
        assert_eq!(
            project_selection("free", None, &projects).unwrap(),
            Some(id)
        );
        assert_eq!(
            project_selection("max", Some("all"), &projects).unwrap(),
            None
        );
    }

    #[test]
    fn tied_completions_have_separate_links_and_sparse_weeks_break_the_band() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        let mut issues = Vec::new();
        for (offset, count) in [(0, 4), (7, 3), (14, 4)] {
            let end = (date + Duration::days(offset))
                .and_hms_opt(10, 0, 0)
                .unwrap()
                .and_utc();
            for n in 0..count {
                issues.push(Issue {
                    id: Uuid::new_v4(),
                    key: format!("PT-{offset}-{n}"),
                    title: "Tied completion".into(),
                    kind: "bug".into(),
                    status: "done".into(),
                    created_at: end - Duration::days(2),
                    first_start: Some(end - Duration::hours(6)),
                    finish: Some(end),
                    finish_date: Some(end.date_naive()),
                    blockers: vec![],
                    blocker_changes: sqlx::types::Json(vec![]),
                });
            }
        }
        let selection = cohort(&issues);
        let segments = weekly_bands(&selection.done);
        assert_eq!(segments.iter().map(Vec::len).collect::<Vec<_>>(), [1, 1]);
        let chart = plot(&selection.done, "time", date, date + Duration::days(17)).into_string();
        assert_eq!(chart.matches("tabindex=\"0\"").count(), 11);
        assert!(chart.contains("translate(-10.500 0)") && chart.contains("translate(10.500 0)"));
        assert_eq!(chart.matches("href=\"#cycle-bug\"").count(), 12); // 11 observations and their legend.
        assert!(!chart.contains("href=\"#cycle-security\""));
    }

    #[sqlx::test]
    async fn reopened_issue_uses_first_start_and_latest_real_completion(db: PgPool) {
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('cycle-test@example.invalid') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,name,key) VALUES($1,'Papertrail','PT') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issues(owner_id,project_id,number,title,status) VALUES
                ($1,$2,1,'Certificate','in_progress'),($1,$2,2,'Unknown start','done'),
                ($1,$2,3,'Canceled work','canceled'),($1,$2,4,'Imported older completion','done')",
        )
        .bind(owner)
        .bind(project)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("UPDATE issues SET created_at='2025-12-14T10:00:00Z' WHERE project_id=$1")
            .bind(project)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE issues SET completed_at='2026-01-25T12:00:00Z' WHERE project_id=$1 AND number=4")
            .bind(project).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,action,occurred_at)
            SELECT i.owner_id,i.project_id,i.id,v.before,v.after,v.action,v.at::timestamptz FROM issues i JOIN (VALUES
                (1,'ready','in_progress','status','2026-01-01T11:30:00Z'),
                (1,'in_progress','done','status','2026-01-05T11:30:00Z'),
                (1,'done','in_progress','status','2026-01-06T11:30:00Z'),
                (2,NULL,'done','status','2026-02-01T11:30:00Z'),
                (4,NULL,'done','import','2026-02-02T11:30:00Z')
            ) v(n,before,after,action,at) ON i.number=v.n WHERE i.project_id=$1").bind(project).execute(&db).await.unwrap();
        let now = "2026-02-04T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let cutoff = NaiveDate::from_ymd_opt(2026, 2, 1);
        let initial = observations(
            &db,
            owner,
            Some(project),
            "all",
            cutoff,
            now,
            "Europe/Zurich",
        )
        .await
        .unwrap();
        let observed = cohort(&initial);
        assert!(observed.done.is_empty());
        assert_eq!(observed.open.len(), 1);
        assert_eq!(observed.open[0].key, "PT-1");
        let selection = Selection {
            project: Some(project),
            kind: "all",
            period: "plan",
            view: "time",
        };
        let overview = sources(&selection, &observed, now, 1, 1, None).into_string();
        assert!(!overview.contains("class=\"cycle-records\" open"));
        let open_table = sources(&selection, &observed, now, 1, 1, Some("open-age")).into_string();
        assert!(open_table.contains("class=\"cycle-records\" open"));
        assert!(
            selection
                .page_url(1, 2, "open-age")
                .ends_with("open_page=2&records=open-age#open-age")
        );
        // The issue existed before first start; both its original active period
        // and the reopened waiting time belong to its open age.
        assert!(open_table.contains("Days since start"));
        assert!(open_table.contains("33.94"));
        assert_eq!(observed.unknown.len(), 1);
        sqlx::query("UPDATE issues SET status='done' WHERE project_id=$1 AND number=1")
            .bind(project)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,occurred_at)
            SELECT owner_id,project_id,id,prior,'done',recorded_at::timestamptz FROM issues CROSS JOIN (
                VALUES ('in_progress','2026-01-31T23:30:00Z'),('done','2026-02-03T00:00:00Z')
            ) v(prior,recorded_at) WHERE project_id=$1 AND number=1").bind(project).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,action,after_state,occurred_at)
            SELECT i.owner_id,i.project_id,i.id,'done','done','blocker',
              jsonb_build_object('relation',jsonb_build_object('blocker_id',b.id,'present',v.present)),v.at::timestamptz
            FROM issues i JOIN issues b ON b.project_id=i.project_id AND b.number=3
            CROSS JOIN (VALUES (true,'2026-01-31T23:03Z'), (false,'2026-01-31T23:10Z')) v(present,at)
            WHERE i.project_id=$1 AND i.number=1").bind(project).execute(&db).await.unwrap();
        let after = observations(
            &db,
            owner,
            Some(project),
            "all",
            cutoff,
            now,
            "Europe/Zurich",
        )
        .await
        .unwrap();
        let observed = cohort(&after);
        assert!(observed.open.is_empty());
        assert_eq!(observed.done.len(), 1);
        assert_eq!(observed.done[0].days, 30.5);
        assert_eq!(Some(observed.done[0].finish_date), cutoff);
        assert_eq!(observed.unknown.len(), 1);
        assert!(
            source(observed.done[0].issue)
                .into_string()
                .contains("PT-3")
        );
        let first = observed.done[0].finish_date;
        let group_chart = plot(&observed.done, "grouped", first, first).into_string();
        assert!(group_chart.contains(&observed.done[0].issue.history()));
        assert!(group_chart.contains("30.5 days"));
        // An imported transition with no measured completion must never reuse an
        // old finish from before the last reopen.
        sqlx::query("INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,action,occurred_at)
            SELECT owner_id,project_id,id,prev,new_s,op,at::timestamptz FROM issues CROSS JOIN
              (VALUES ('done','in_progress','status','2026-02-03T02:00Z'),
                      ('in_progress','done','import','2026-02-03T03:00Z')) v(prev,new_s,op,at)
            WHERE project_id=$1 AND number=1").bind(project).execute(&db).await.unwrap();
        let imported = observations(&db, owner, Some(project), "all", None, now, "UTC")
            .await
            .unwrap();
        assert!(
            cohort(&imported).done.is_empty(),
            "an imported Done has no recorded current completion"
        );
        sqlx::query(
            "UPDATE issues SET completed_at='2026-02-03T00:30Z' WHERE project_id=$1 AND number=1",
        )
        .bind(project)
        .execute(&db)
        .await
        .unwrap();
        let dated = observations(&db, owner, Some(project), "all", None, now, "UTC")
            .await
            .unwrap();
        assert_eq!(
            cohort(&dated).done[0].finish,
            "2026-02-03T00:30:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert!(
            observations(
                &db,
                Uuid::new_v4(),
                Some(project),
                "all",
                cutoff,
                now,
                "UTC"
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            observations(&db, owner, Some(project), "bug", cutoff, now, "UTC")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
