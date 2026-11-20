use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveDateTime, Utc};
use maud::{Markup, html};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{AppError, AppResult};

#[derive(FromRow)]
struct ReleaseClock {
    project_id: Uuid,
    key: String,
    timezone: String,
    created: NaiveDate,
    observed_through: NaiveDate,
    cutoff: DateTime<Utc>,
    published_on: Option<NaiveDate>,
    today: NaiveDate,
    missing_history: i64,
    first_assignment: Option<NaiveDate>,
}

#[derive(FromRow)]
struct Source {
    issue_id: Uuid,
    local_at: NaiveDateTime,
    occurred_at: DateTime<Utc>,
    status: String,
    membership: Option<bool>,
    source: Option<String>,
}

#[derive(FromRow)]
struct Issue {
    id: Uuid,
    number: i64,
    title: String,
    next_action: String,
}

type Inventory = BTreeMap<Uuid, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Movement {
    Added,
    Reopened,
    Completed,
    Removed,
    Other,
}

impl Movement {
    fn label(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Reopened => "Reopened",
            Self::Completed => "Completed",
            Self::Removed => "Removed",
            Self::Other => "Other changes",
        }
    }

    fn anchor(self) -> &'static str {
        match self {
            Self::Added => "review-added",
            Self::Reopened => "review-reopened",
            Self::Completed => "review-completed",
            Self::Removed => "review-removed",
            Self::Other => "review-other",
        }
    }
}

const MOVEMENTS: [Movement; 4] = [
    Movement::Added,
    Movement::Reopened,
    Movement::Completed,
    Movement::Removed,
];

fn is_open(status: Option<&str>) -> bool {
    matches!(status, Some("backlog" | "ready" | "in_progress"))
}

fn classify(before: Option<&str>, after: Option<&str>, recorded_status: &str) -> Movement {
    match (is_open(before), is_open(after)) {
        (true, false) if recorded_status == "done" => Movement::Completed,
        (true, false) => Movement::Removed,
        (false, true) if before == Some("done") => Movement::Reopened,
        (false, true) => Movement::Added,
        _ => Movement::Other,
    }
}

struct Change {
    source: Source,
    before: Option<String>,
    after: Option<String>,
    movement: Movement,
    open_after: usize,
}

struct Day {
    date: NaiveDate,
    scope: Vec<Uuid>,
    done: Vec<Uuid>,
}

struct Review {
    opening: Vec<Uuid>,
    ending: Vec<Uuid>,
    days: Vec<Day>,
    changes: Vec<Change>,
}

fn open_ids(inventory: &Inventory) -> Vec<Uuid> {
    inventory
        .iter()
        .filter(|(_, s)| is_open(Some(s)))
        .map(|(id, _)| *id)
        .collect()
}

fn reconstruct(
    mut inventory: Inventory,
    events: Vec<Source>,
    start: NaiveDate,
    end: NaiveDate,
) -> Review {
    let opening = open_ids(&inventory);
    let mut open = opening.len();
    let mut days = Vec::new();
    let mut changes = Vec::new();
    let mut events = events.into_iter().peekable();
    for date in start.iter_days().take_while(|date| *date <= end) {
        while let Some(source) = events.next_if(|event| event.local_at.date() == date) {
            let before = inventory.get(&source.issue_id).cloned();
            let in_scope = source.membership.unwrap_or(before.is_some());
            let after = in_scope.then(|| source.status.clone());
            if before == after {
                continue;
            }
            let movement = classify(before.as_deref(), after.as_deref(), &source.status);
            if let Some(ref status) = after {
                inventory.insert(source.issue_id, status.clone());
            } else {
                inventory.remove(&source.issue_id);
            }
            match movement {
                Movement::Added | Movement::Reopened => open += 1,
                Movement::Completed | Movement::Removed => open -= 1,
                Movement::Other => {}
            }
            changes.push(Change {
                source,
                before,
                after,
                movement,
                open_after: open,
            });
        }
        let scope = inventory
            .iter()
            .filter(|(_, s)| s.as_str() != "canceled")
            .map(|(id, _)| *id)
            .collect();
        let done = inventory
            .iter()
            .filter(|(_, s)| s.as_str() == "done")
            .map(|(id, _)| *id)
            .collect();
        days.push(Day { date, scope, done });
    }
    Review {
        opening,
        ending: open_ids(&inventory),
        days,
        changes,
    }
}

fn monday(day: NaiveDate) -> NaiveDate {
    day - Days::new(day.weekday().num_days_from_monday().into())
}

#[derive(Debug, PartialEq, Eq)]
struct Window {
    earliest: NaiveDate,
    latest: NaiveDate,
    week: NaiveDate,
    first: NaiveDate,
    last: NaiveDate,
}

fn select_window(clock: &ReleaseClock, requested: Option<NaiveDate>, plan: &str) -> Option<Window> {
    let shipment = clock.published_on.unwrap_or(clock.observed_through);
    let first_record = clock.created.min(shipment);
    let latest = clock.today.min(clock.observed_through.max(shipment));
    let earliest =
        super::history_start(clock.today, plan).map_or(first_record, |date| date.max(first_record));
    if earliest > latest {
        return None;
    }
    let week = monday(requested.unwrap_or(latest).clamp(earliest, latest));
    Some(Window {
        earliest,
        latest,
        week,
        first: week.max(earliest),
        last: (week + Days::new(6)).min(latest),
    })
}

const CLOCK: &str = r#"
    SELECT r.project_id, p.key, u.timezone,
           (r.created_at AT TIME ZONE u.timezone)::date AS created,
           (LEAST(COALESCE(r.published_at, now()), now()) AT TIME ZONE u.timezone)::date AS observed_through,
           LEAST(COALESCE(r.published_at, now()), now()) AS cutoff, r.published_on,
           (now() AT TIME ZONE u.timezone)::date AS today,
           (SELECT (min(e.occurred_at) AT TIME ZONE u.timezone)::date FROM release_membership_events e
             WHERE e.release_id=r.id AND e.owner_id=r.owner_id AND e.in_scope) AS first_assignment,
           (SELECT count(*) FROM (
                SELECT issue_id FROM release_entries WHERE release_id=r.id
                UNION SELECT issue_id FROM release_members WHERE release_id=r.id
                UNION SELECT issue_id FROM release_carryovers WHERE release_id=r.id
            ) AS known WHERE NOT EXISTS (
                SELECT FROM release_membership_events e WHERE e.owner_id=r.owner_id
                  AND e.release_id=r.id AND e.issue_id=known.issue_id AND e.in_scope
            )) AS missing_history
      FROM releases r JOIN users u ON u.id=r.owner_id
      JOIN projects p ON p.owner_id=r.owner_id AND p.id=r.project_id
     WHERE r.owner_id=$1 AND r.id=$2
"#;

// A previous assignment records the status at that instant. A later canonical transition
// supersedes it. Only this opening inventory crosses the selected history boundary.
const BASELINE: &str = r#"
    WITH membership AS (
        SELECT DISTINCT ON (issue_id) issue_id, in_scope, status, occurred_at
          FROM release_membership_events
         WHERE owner_id=$1 AND release_id=$2 AND occurred_at < $3
         ORDER BY issue_id, occurred_at DESC, id DESC
    )
    SELECT m.issue_id, COALESCE(s.new_status,m.status) AS status FROM membership m
    LEFT JOIN LATERAL (
        SELECT new_status FROM issue_events e
         WHERE e.owner_id=$1 AND e.project_id=$4 AND e.issue_id=m.issue_id
           AND e.occurred_at >= m.occurred_at AND e.occurred_at < $3
           AND e.old_status IS DISTINCT FROM e.new_status
         ORDER BY e.occurred_at DESC,e.revision_after DESC NULLS LAST,e.id DESC LIMIT 1
    ) s ON true WHERE m.in_scope
"#;

// Collapse the two event streams to one before/after observation per issue and instant.
// A canonical status transition wins over the status copied when membership changed.
const EVENTS: &str = r#"
    WITH membership AS (
        SELECT DISTINCT ON (occurred_at,issue_id) occurred_at,issue_id,in_scope,status,source
          FROM release_membership_events
         WHERE owner_id=$1 AND release_id=$2 AND occurred_at >= $3 AND occurred_at < $4 AND occurred_at <= $5
         ORDER BY occurred_at,issue_id,id DESC
    ), transitions AS (
        SELECT DISTINCT ON (occurred_at,issue_id) occurred_at,issue_id,new_status
          FROM issue_events e
         WHERE e.owner_id=$1 AND e.project_id=$6 AND occurred_at >= $3 AND occurred_at < $4 AND occurred_at <= $5
           AND old_status IS DISTINCT FROM new_status
           AND EXISTS (SELECT FROM release_membership_events r WHERE r.owner_id=$1 AND r.release_id=$2
                       AND r.issue_id=e.issue_id AND r.occurred_at<=e.occurred_at)
         ORDER BY occurred_at,issue_id,revision_after DESC NULLS LAST,id DESC
    )
    SELECT issue_id, occurred_at, occurred_at AT TIME ZONE $7 AS local_at, COALESCE(t.new_status,m.status) AS status,
           m.in_scope AS membership, m.source
      FROM membership m FULL JOIN transitions t USING (occurred_at,issue_id)
     ORDER BY occurred_at,issue_id
"#;

pub(super) async fn history(
    db: &PgPool,
    owner: Uuid,
    release: Uuid,
    week: Option<NaiveDate>,
    plan: &str,
) -> AppResult<Markup> {
    let mut tx = db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let clock = sqlx::query_as::<_, ReleaseClock>(CLOCK)
        .bind(owner)
        .bind(release)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    let Some(window) = select_window(&clock, week, plan) else {
        tx.commit().await?;
        return Ok(html! { section.panel id="review" { h2 { "Release review" }
        p { "This release falls outside your plan’s chart history." } } });
    };
    // A missing imported assignment makes earlier zeroes ambiguous.
    let coverage_start = if clock.missing_history > 0 {
        clock.first_assignment
    } else {
        Some(clock.created)
    };
    let end = window.last.min(clock.observed_through);
    let review =
        if let Some(start) = coverage_start
            .map(|d| d.max(window.first))
            .filter(|d| *d <= end)
        {
            let (from, until): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
            "SELECT $1::date::timestamp AT TIME ZONE $3, ($2::date+1)::timestamp AT TIME ZONE $3")
            .bind(start).bind(end).bind(&clock.timezone).fetch_one(&mut *tx).await?;
            let baseline = sqlx::query_as::<_, (Uuid, String)>(BASELINE)
                .bind(owner)
                .bind(release)
                .bind(from)
                .bind(clock.project_id)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .collect();
            let events = sqlx::query_as::<_, Source>(EVENTS)
                .bind(owner)
                .bind(release)
                .bind(from)
                .bind(until)
                .bind(clock.cutoff)
                .bind(clock.project_id)
                .bind(&clock.timezone)
                .fetch_all(&mut *tx)
                .await?;
            Some(reconstruct(baseline, events, start, end))
        } else {
            None
        };
    let mut ids = BTreeSet::new();
    if let Some(r) = &review {
        ids.extend(r.opening.iter().copied());
        ids.extend(r.days.iter().flat_map(|day| day.scope.iter().copied()));
        ids.extend(r.changes.iter().map(|c| c.source.issue_id));
    }
    let issues = sqlx::query_as::<_, Issue>("SELECT id,number,title,next_action FROM issues WHERE owner_id=$1 AND project_id=$2 AND id=ANY($3)")
        .bind(owner).bind(clock.project_id).bind(ids.into_iter().collect::<Vec<_>>())
        .fetch_all(&mut *tx).await?.into_iter().map(|row| (row.id, row)).collect();
    tx.commit().await?;
    Ok(render(release, &clock, &window, review.as_ref(), &issues))
}

type Issues = BTreeMap<Uuid, Issue>;

fn issue_link(id: Uuid, key: &str, issues: &Issues) -> Markup {
    match issues.get(&id) {
        Some(i) => {
            html! { a href=(format!("/issues/{id}")) { (key) "-" (i.number) " · " (&i.title) } }
        }
        None => html! { span { "Issue " (id) } },
    }
}

fn issue_list(ids: &[Uuid], key: &str, issues: &Issues, next_actions: bool) -> Markup {
    let mut ordered = ids.to_vec();
    ordered.sort_by_key(|id| issues.get(id).map(|i| i.number));
    html! {
        @if ordered.is_empty() { p.muted { "No issues." } }
        @else { ul.issue-list {
            @for id in ordered {
                li {
                    (issue_link(id, key, issues))
                    @if next_actions {
                        @if let Some(action) = issues.get(&id).map(|i| i.next_action.trim()).filter(|a| !a.is_empty()) {
                            div.muted { "Next action · " (action) }
                        }
                    }
                }
            }
        } }
    }
}

fn status_label(status: Option<&str>) -> &str {
    match status {
        None => "Outside release",
        Some("in_progress") => "In progress",
        Some("backlog") => "Backlog",
        Some("ready") => "Ready",
        Some("done") => "Done",
        Some("canceled") => "Canceled",
        Some(other) => other,
    }
}

fn week_url(release: Uuid, date: NaiveDate) -> String {
    format!("/releases/{release}?week={date}#review")
}

fn render(
    release: Uuid,
    clock: &ReleaseClock,
    window: &Window,
    review: Option<&Review>,
    issues: &Issues,
) -> Markup {
    let ship = clock
        .published_on
        .filter(|date| *date >= window.first && *date <= window.last);
    html! {
        section.panel.stack id="review" aria-label="Release review" {
            h2 { "Release review" }
            nav.toolbar aria-label="Review week" {
                @if window.week > monday(window.earliest) {
                    a href=(week_url(release, window.week - Days::new(7))) { "← Previous week" }
                }
                form.row method="get" action=(format!("/releases/{release}#review")) {
                    label { "Week of " input type="date" name="week" value=(window.first) min=(window.earliest) max=(window.latest) required; }
                    button.secondary type="submit" { "View week" }
                }
                @if window.week < monday(window.latest) {
                    a href=(week_url(release, window.week + Days::new(7))) { "Next week →" }
                }
            }
            p.muted { (window.first.format("%b %d, %Y")) " – " (window.last.format("%b %d, %Y")) " · " (&clock.timezone) }
            @if window.first == clock.created || (window.first < clock.created && clock.created <= window.last) {
                @if clock.missing_history == 0 {
                    p.notice { "History starts here. Release tracking began " (clock.created) "." }
                }
            } @else if window.first == window.earliest && window.first > clock.created {
                p.notice { "Chart history is available from " (window.earliest) " on your plan. Opening counts include earlier assignments." }
            }
            @if clock.missing_history > 0 {
                p.notice { "History starts here. " (clock.missing_history) " release item"
                    @if clock.missing_history > 1 { "s have" } @else { " has" }
                    " no recorded assignment history. Counts cover observed assignments only." }
            }
            @if let Some(r) = review {
                section.stack {
                    h3 { "Observed burnup" }
                    p.muted { "Scope counts assigned issues except Canceled. Done counts completed issues still in scope." }
                    @if clock.published_on.is_some() && window.last >= clock.observed_through {
                        p.muted { "Observations end at publication." }
                    } @else if clock.published_on.is_none() && window.last == clock.today {
                        p.muted { "Today’s observation includes changes recorded so far." }
                    }
                    (burnup(r, window, ship))
                    table {
                        caption { "Daily observations" }
                        thead { tr { th scope="col" { "Date" } th scope="col" { "Scope" } th scope="col" { "Done" } } }
                        tbody {
                            @for day in &r.days {
                                tr {
                                    th scope="row" { (day.date) }
                                    td { a href=(format!("#scope-{}", day.date)) { (day.scope.len()) } }
                                    td { a href=(format!("#done-{}", day.date)) { (day.done.len()) } }
                                }
                            }
                        }
                    }
                    @for day in &r.days {
                        details {
                            summary { (day.date) " · " (day.scope.len()) " in scope · " (day.done.len()) " done" }
                            h4 id=(format!("scope-{}", day.date)) tabindex="-1" { "Scope on " (day.date) }
                            (issue_list(&day.scope, &clock.key, issues, false))
                            h4 id=(format!("done-{}", day.date)) tabindex="-1" { "Done on " (day.date) }
                            (issue_list(&day.done, &clock.key, issues, false))
                        }
                    }
                }
            }
            @if let Some(day) = ship {
                p { a href="#manifest" { span aria-hidden="true" { "◆ " } "Shipped " (day) " · Open published manifest" } }
            }
            @if let Some(r) = review {
                section.stack {
                    h3 { "Open work waterfall" }
                    (waterfall(r))
                    (waterfall_sources(r, &clock.key, issues))
                }
            } @else {
                p.muted { "No scope observations in this week." }
            }
        }
    }
}

const ACCENT: &str = "var(--accent, currentColor)";

fn burnup(review: &Review, window: &Window, shipment: Option<NaiveDate>) -> Markup {
    let left = 45.0;
    let max = review
        .days
        .iter()
        .map(|d| d.scope.len())
        .max()
        .unwrap_or(0)
        .max(1);
    let mut ticks = vec![0, max / 2, max];
    ticks.dedup();
    let y = |value: usize| 143.0 - 105.0 * value as f64 / max as f64;
    let x = |date: NaiveDate| left + 512.0 * (date - window.week).num_days() as f64 / 6.0;
    html! {
        div.review-chart-frame tabindex="0" role="region" aria-label="Observed burnup chart" {
            svg.chart viewBox="0 0 600 178" width="100%" role="group" aria-label="Daily observed scope and Done, in issues" {
                @for tick in ticks {
                    line x1="40" x2="578" y1=(y(tick)) y2=(y(tick)) stroke="currentColor" opacity="0.2" {}
                    text x="32" y=(y(tick)+4.0) text-anchor="end" fill="currentColor" font-size="11" { (tick) }
                }
                g fill="none" stroke-width="2" {
                    @for pair in review.days.windows(2) {
                        @let (a,b) = (&pair[0],&pair[1]);
                        line x1=(x(a.date)) y1=(y(a.scope.len())) x2=(x(b.date)) y2=(y(b.scope.len())) stroke="currentColor" {}
                        line x1=(x(a.date)) y1=(y(a.done.len())) x2=(x(b.date)) y2=(y(b.done.len())) stroke=(ACCENT) stroke-dasharray="5 4" {}
                    }
                }
                @for day in &review.days {
                    @let xx = x(day.date);
                    @for (name, value) in [("scope", day.scope.len()), ("done", day.done.len())] {
                        @let yy = y(value);
                        a href=(format!("#{name}-{}", day.date)) aria-label=(format!("{}: {value} {name} issues", day.date)) {
                            title { (day.date) " · " (value) " " (name) }
                            @if name == "scope" {
                                rect x=(xx-10.0) y=(yy-10.0) width="20" height="20" fill="transparent" {}
                                rect x=(xx-5.5) y=(yy-5.5) width="11" height="11" fill="var(--surface, Canvas)" stroke="currentColor" stroke-width="1.8" {}
                            } @else {
                                circle cx=(xx) cy=(yy) r=(if day.scope.len()==value { 4 } else { 10 }) fill="transparent" {}
                                circle cx=(xx) cy=(yy) r="3.6" fill=(ACCENT) {}
                            }
                        }
                    }
                }
                @for date in window.week.iter_days().take(7) {
                    @let observed = review.days.iter().any(|day| day.date == date);
                    text x=(x(date)) y="167" text-anchor="middle" font-size="11" fill=(if observed { "currentColor" } else { "var(--faint, GrayText)" }) {
                        @if !observed { title { (date) " · No observation" } }
                        (date.format("%a %d"))
                    }
                }
                @if let Some(date) = shipment {
                    @let xx = x(date);
                    a href="#manifest" aria-label=(format!("Open published manifest from {date}")) {
                        title { "Published manifest · " (date) }
                        path d=(format!("M {xx} 4 l 7 7 -7 7 -7 -7 Z")) fill=(ACCENT) {}
                    }
                }
            }
        }
        p.muted { "□ Scope (solid) · ● Done (dashed)" }
    }
}

struct Step {
    label: &'static str,
    anchor: &'static str,
    from: usize,
    to: usize,
}

fn steps(review: &Review) -> Vec<Step> {
    let mut open = review.opening.len();
    let mut bars = vec![Step {
        label: "Opening",
        anchor: "review-opening",
        from: 0,
        to: open,
    }];
    for movement in MOVEMENTS {
        let delta = review
            .changes
            .iter()
            .filter(|c| c.movement == movement)
            .count();
        let from = open;
        match movement {
            Movement::Added | Movement::Reopened => open += delta,
            Movement::Completed | Movement::Removed => open -= delta,
            Movement::Other => {}
        }
        bars.push(Step {
            label: movement.label(),
            anchor: movement.anchor(),
            from,
            to: open,
        });
    }
    bars.push(Step {
        label: "Carryover",
        anchor: "review-carryover",
        from: 0,
        to: review.ending.len(),
    });
    bars
}

fn waterfall(review: &Review) -> Markup {
    let bars = steps(review);
    let last = bars.len() - 1;
    let peak = bars
        .iter()
        .map(|bar| bar.to.max(bar.from))
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let y = |n: usize| 142.0 - 110.0 * n as f64 / peak;
    html! {
        div.review-chart-frame tabindex="0" role="region" aria-label="Open work waterfall chart" {
            svg.chart viewBox="0 0 600 178" width="100%" role="group" aria-label="Weekly changes to open issue inventory" {
                defs {
                    pattern id="review-decrease" width="6" height="6" patternUnits="userSpaceOnUse" {
                        path d="M-1 1 1 -1 M0 6 6 0 M5 7 7 5" fill="none" stroke="currentColor" stroke-width="1" opacity=".7" {}
                    }
                }
                line x1="13" x2="584" y1="142" y2="142" stroke="currentColor" opacity="0.24" {}
                @for (i, bar) in bars.iter().enumerate() {
                    @let xx = 26 + i*96;
                    @let high = y(bar.from.max(bar.to));
                    @let height = (y(bar.from) - y(bar.to)).abs().max(2.0);
                    @let total = i == 0 || i == last;
                    @let growth = !total && bar.to > bar.from;
                    @let sign = if total || bar.from == bar.to { "" } else if growth { "+" } else { "−" };
                    @let value = bar.from.abs_diff(bar.to);
                    @if i < last {
                        line x1=(xx+54) x2=(xx+96) y1=(y(bar.to)) y2=(y(bar.to)) stroke="currentColor" stroke-dasharray="3 3" opacity=".55" {}
                    }
                    a href=(format!("#{}", bar.anchor)) aria-label=(format!("{}: {sign}{value} issues", bar.label)) {
                        title { (bar.label) " · " (sign) (value) }
                        @let fill = if total || value == 0 { "currentColor" } else if growth { ACCENT } else { "url(#review-decrease)" };
                        rect x=(xx) y=(high) width="54" height=(height) fill=(fill)
                            stroke=(if growth { ACCENT } else { "currentColor" }) stroke-width="1" {}
                        text x=(xx+27) y=(high-8.0) text-anchor="middle" font-size="13" fill="currentColor" { (sign) (value) }
                        text x=(xx+27) y="165" text-anchor="middle" font-size="11" fill="currentColor" { (bar.label) }
                    }
                }
            }
        }
    }
}

fn waterfall_sources(review: &Review, key: &str, issues: &Issues) -> Markup {
    html! {
        table {
            caption { "Weekly reconciliation" }
            thead { tr { th scope="col" { "Stage" } th scope="col" { "Issues" } th scope="col" { "Open after stage" } } }
            tbody { @for bar in steps(review) {
                tr { th scope="row" { a href=(format!("#{}", bar.anchor)) { (bar.label) } }
                    td { (bar.to.abs_diff(bar.from)) } td { (bar.to) } }
            } }
        }
        details {
            summary { "Opening issues · " (review.opening.len()) }
            h4 id="review-opening" tabindex="-1" { "Opening issues" }
            (issue_list(&review.opening, key, issues, false))
        }
        @for movement in MOVEMENTS.into_iter().chain([Movement::Other]) {
            @let count = review.changes.iter().filter(|c| c.movement == movement).count();
            @if count > 0 || movement != Movement::Other {
                details {
                    summary { (movement.label()) " · " (count) }
                    h4 id=(movement.anchor()) tabindex="-1" { (movement.label()) }
                    @if count == 0 { p.muted { "No recorded changes." } }
                    @else {
                        table {
                            thead { tr { th scope="col" { "When" } th scope="col" { "Issue" } th scope="col" { "Change" } th scope="col" { "Open after event" } } }
                            tbody { @for c in review.changes.iter().filter(|c| c.movement == movement) {
                                tr {
                                    td { time datetime=(c.source.occurred_at.to_rfc3339()) title=(c.source.occurred_at.to_rfc3339()) {
                                        (c.source.local_at.format("%b %d %H:%M:%S"))
                                    } }
                                    td { (issue_link(c.source.issue_id, key, issues)) }
                                    td {
                                        (status_label(c.before.as_deref())) " → "
                                        @if c.after.is_none() && c.movement == Movement::Completed { "Done · left release" }
                                        @else { (status_label(c.after.as_deref())) }
                                        @if let Some(source) = &c.source.source { " · " (source) }
                                        @if c.movement == Movement::Reopened {
                                            " · " a href=(format!("/issues/{}#history", c.source.issue_id)) { "Issue history" }
                                        }
                                    }
                                    td { (c.open_after) }
                                }
                            } }
                        }
                    }
                }
            }
        }
        details {
            summary { "Carryover issues · " (review.ending.len()) }
            h4 id="review-carryover" tabindex="-1" { "Carryover issues and current next actions" }
            (issue_list(&review.ending, key, issues, true))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(iso: &str) -> NaiveDate {
        iso.parse().unwrap()
    }

    fn event(id: Uuid, day: u32, hour: u32, membership: Option<bool>, status: &str) -> Source {
        Source {
            issue_id: id,
            occurred_at: Utc::now(),
            local_at: NaiveDate::from_ymd_opt(2025, 3, day)
                .unwrap()
                .and_hms_opt(hour, 0, 0)
                .unwrap(),
            status: status.into(),
            membership,
            source: None,
        }
    }

    #[test]
    fn reconciles_repeated_completions_reopenings_cancellation_and_scope_moves() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        let inventory = Inventory::from([(a, "ready".into()), (b, "canceled".into())]);
        let sources = vec![
            event(a, 24, 1, None, "done"),
            event(b, 24, 2, None, "in_progress"),
            event(c, 24, 3, Some(true), "done"),
            event(a, 25, 1, None, "ready"),
            event(a, 25, 2, Some(false), "done"),
            event(b, 25, 3, None, "canceled"),
            event(c, 25, 4, Some(false), "done"),
            event(a, 25, 5, None, "ready"),
        ];
        let r = reconstruct(inventory, sources, date("2025-03-24"), date("2025-03-26"));
        assert_eq!(r.opening, vec![a]);
        assert_eq!(r.days[0].scope, vec![a, b, c]);
        assert_eq!(r.days[0].done, vec![a, c]);
        assert!(r.days[1].scope.is_empty());
        assert_eq!(
            r.changes.iter().map(|c| c.movement).collect::<Vec<_>>(),
            vec![
                Movement::Completed,
                Movement::Added,
                Movement::Other,
                Movement::Reopened,
                Movement::Completed,
                Movement::Removed,
                Movement::Other,
            ]
        );
        assert_eq!(
            steps(&r).into_iter().map(|bar| bar.to).collect::<Vec<_>>(),
            vec![1, 2, 3, 1, 0, 0]
        );
        assert!(r.ending.is_empty());
    }

    #[test]
    fn clips_to_entitled_calendar_days_and_keeps_partial_weeks() {
        let mut clock = ReleaseClock {
            project_id: Uuid::nil(),
            key: "P".into(),
            timezone: "UTC".into(),
            created: date("2025-01-01"),
            observed_through: date("2025-04-03"),
            cutoff: Utc::now(),
            published_on: None,
            today: date("2025-04-03"),
            missing_history: 0,
            first_assignment: None,
        };
        let first = select_window(&clock, Some(date("2024-01-01")), "free").unwrap();
        assert_eq!(
            (first.week, first.first, first.last),
            (date("2025-03-03"), date("2025-03-05"), date("2025-03-09"))
        );
        let latest = select_window(&clock, Some(date("2100-01-01")), "max").unwrap();
        assert_eq!(
            (latest.week, latest.last),
            (date("2025-03-31"), clock.today)
        );
        clock.created = date("2020-01-01");
        clock.today = date("2024-03-01");
        clock.observed_through = clock.today;
        assert_eq!(
            select_window(&clock, None, "plus").unwrap().earliest,
            date("2023-03-02")
        );
        clock.today = date("2025-04-03");
        clock.observed_through = date("2025-01-20");
        clock.published_on = Some(date("2025-01-15"));
        assert!(select_window(&clock, None, "free").is_none());
    }

    #[test]
    fn burnup_labels_the_whole_week_and_links_only_observed_days() {
        let week = date("2025-03-24");
        let end = date("2025-03-26");
        let window = Window {
            earliest: week,
            latest: end,
            week,
            first: week,
            last: end,
        };
        let review = reconstruct(Inventory::new(), vec![], week, end);
        let chart = burnup(&review, &window, None).into_string();
        assert!(chart.contains("Sun 30"));
        assert_eq!(chart.matches("No observation").count(), 4);
        assert_eq!(chart.matches("href=\"#done-").count(), 3);
        assert!(!chart.contains("href=\"#done-2025-03-27\""));
    }

    #[sqlx::test]
    async fn review_uses_local_week_boundaries_and_sealed_release_evidence(db: PgPool) {
        let owner = Uuid::new_v4();
        let project = Uuid::new_v4();
        let release = Uuid::new_v4();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        sqlx::query("INSERT INTO users(id,email,timezone) VALUES ($1,'review@example.test','Europe/Zurich')")
            .bind(owner).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO projects(id,owner_id,name,key) VALUES ($1,$2,'Papertrail','PT')")
            .bind(project)
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO releases(id,owner_id,project_id,name,created_at) VALUES ($1,$2,$3,'Beta','2025-03-01Z')")
            .bind(release).bind(owner).bind(project).execute(&db).await.unwrap();
        for (idx, title, id) in [
            (1, "Opening issue", a),
            (2, "Next week only", b),
            (3, "Portable entry", c),
        ] {
            sqlx::query(
                "INSERT INTO issues(id,owner_id,project_id,number,title) VALUES ($1,$2,$3,$4,$5)",
            )
            .bind(id)
            .bind(owner)
            .bind(project)
            .bind(i64::from(idx))
            .bind(title)
            .execute(&db)
            .await
            .unwrap();
        }
        sqlx::query("INSERT INTO release_entries(owner_id,project_id,release_id,issue_id,number,title,issue_revision) VALUES ($1,$2,$3,$4,3,'Portable entry',1)")
            .bind(owner).bind(project).bind(release).bind(c).execute(&db).await.unwrap();
        let empty = history(&db, owner, release, Some(date("2025-03-24")), "max")
            .await
            .unwrap()
            .into_string();
        assert!(empty.contains("No scope observations"));
        assert!(!empty.contains("Daily observations"));
        for (id, at, in_scope, status) in [
            (a, "2025-03-23T22:59:59Z", true, "ready"), // local Sunday, opening baseline
            (a, "2025-03-23T23:00:00Z", false, "ready"), // exact local Monday start
            (b, "2025-03-30T22:00:00Z", true, "ready"), // next Monday after DST jump
        ] {
            sqlx::query("INSERT INTO release_membership_events(owner_id,project_id,release_id,issue_id,in_scope,status,occurred_at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
                .bind(owner).bind(project).bind(release).bind(id).bind(in_scope).bind(status)
                .bind(at.parse::<DateTime<Utc>>().unwrap()).execute(&db).await.unwrap();
        }
        for (id, at) in [(a, "2025-03-23T23:00:00Z"), (b, "2025-03-31T12:00:00Z")] {
            sqlx::query("INSERT INTO issue_events(owner_id,project_id,issue_id,old_status,new_status,occurred_at) VALUES ($1,$2,$3,'ready','done',$4)")
                .bind(owner).bind(project).bind(id).bind(at.parse::<DateTime<Utc>>().unwrap()).execute(&db).await.unwrap();
        }
        sqlx::query("UPDATE releases SET published_on='2025-03-26',published_at='2025-03-31T11:00:00Z',publish_command=gen_random_uuid() WHERE id=$1")
            .bind(release).execute(&db).await.unwrap();
        let page = history(&db, owner, release, Some(date("2025-03-24")), "max")
            .await
            .unwrap()
            .into_string();
        for href in page.split("href=\"#").skip(1) {
            let id = href.split('"').next().unwrap();
            assert!(
                id == "manifest" || page.contains(&format!("id=\"{id}\"")),
                "missing target {id}"
            );
        }
        assert!(page.contains("Opening issues · 1"));
        assert!(page.contains("Completed · 1"));
        assert!(page.contains("Removed · 0"));
        assert!(page.contains("Done · left release"));
        assert!(page.contains("Shipped 2025-03-26"));
        assert!(page.contains("has no recorded assignment history"));
        assert!(page.contains(&format!("/issues/{a}")));
        assert!(!page.contains("Next week only"));
        let next = history(&db, owner, release, Some(date("2025-03-31")), "max")
            .await
            .unwrap()
            .into_string();
        assert!(next.contains("Next week only"));
        assert!(next.contains("Carryover issues · 1"));
        assert!(next.contains("Completed · 0"));
        assert!(!next.contains("Shipped 2025-03-26"));
        assert!(matches!(
            history(&db, Uuid::new_v4(), release, None, "max").await,
            Err(AppError::NotFound)
        ));
    }
}
