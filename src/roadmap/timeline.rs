use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Days, NaiveDate};
use maud::{Markup, html};
use uuid::Uuid;

use super::calendar::{ReleaseDate, WeekOff};
use super::{Dependency, Milestone};

const LEFT: f64 = 204.0;
const RIGHT: f64 = 790.0;
const WIDTH: usize = 810;
const AXIS: usize = 52;
const ROW: usize = 47;

struct Calendar {
    first: NaiveDate,
    last: NaiveDate,
}

fn after(day: NaiveDate, n: u64) -> NaiveDate {
    day.checked_add_days(Days::new(n)).unwrap_or(NaiveDate::MAX)
}

fn before(day: NaiveDate, n: u64) -> NaiveDate {
    day.checked_sub_days(Days::new(n)).unwrap_or(NaiveDate::MIN)
}

fn monday(day: NaiveDate) -> NaiveDate {
    before(day, u64::from(day.weekday().num_days_from_monday()))
}

impl Calendar {
    fn covering(today: NaiveDate, dates: impl IntoIterator<Item = NaiveDate>) -> Self {
        let (lo, hi) = dates
            .into_iter()
            .fold((today, today), |(lo, hi), d| (lo.min(d), hi.max(d)));
        let pad = (28 - (hi - lo).num_days()).clamp(0, 28) as u64 / 2;
        let first = monday(before(lo, pad.max(2)));
        let last = after(monday(after(hi, pad.max(2))), 7);
        Self { first, last }
    }

    fn x(&self, day: NaiveDate) -> f64 {
        LEFT + (day - self.first).num_days() as f64 / (self.last - self.first).num_days() as f64
            * (RIGHT - LEFT)
    }

    fn ticks(&self) -> Vec<NaiveDate> {
        let span = (self.last - self.first).num_days() as u64;
        if span <= 98 {
            let step = if span <= 56 { 7 } else { 14 };
            return (0..=span)
                .step_by(step)
                .map(|n| after(self.first, n))
                .collect();
        }
        let month = |d: NaiveDate| i64::from(d.year()) * 12 + i64::from(d.month0());
        let first_month = month(self.first) + i64::from(self.first.day() != 1);
        let last_month = month(self.last);
        let count = last_month - first_month + 1;
        let step = [1, 2, 3, 6, 12, 24, 60, 120]
            .into_iter()
            .find(|step| count <= step * 8)
            .unwrap_or_else(|| ((count + 95) / 96) * 12);
        let aligned = first_month + (step - first_month.rem_euclid(step)).rem_euclid(step);
        (aligned..=last_month)
            .step_by(step as usize)
            .filter_map(|n| {
                NaiveDate::from_ymd_opt(n.div_euclid(12) as i32, n.rem_euclid(12) as u32 + 1, 1)
            })
            .collect()
    }

    fn tick_label(&self, day: NaiveDate) -> String {
        let span = (self.last - self.first).num_days();
        day.format(if span > 1464 {
            "%Y"
        } else if span > 98 {
            "%b %y"
        } else {
            "%d %b"
        })
        .to_string()
    }
}

struct Relations {
    before: HashMap<Uuid, Vec<Uuid>>,
}

impl Relations {
    fn new(deps: &[Dependency]) -> Self {
        let mut before: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for edge in deps {
            before
                .entry(edge.dependent_id)
                .or_default()
                .push(edge.prerequisite_id);
        }
        Self { before }
    }

    fn upstream(&self, selected: Option<Uuid>) -> HashSet<Uuid> {
        let mut chain = HashSet::new();
        let mut pending = Vec::from_iter(selected);
        while let Some(id) = pending.pop() {
            if chain.insert(id)
                && let Some(parents) = self.before.get(&id)
            {
                pending.extend(parents);
            }
        }
        chain
    }
}

fn detail(project: Uuid, id: Uuid) -> String {
    format!("/roadmap?project={project}&milestone={id}#milestone-detail")
}

fn status(m: &Milestone) -> &'static str {
    match m.status.as_str() {
        "reached" => "Reached",
        "in_progress" => "In progress",
        _ => "Planned",
    }
}

fn date(d: NaiveDate) -> String {
    d.format("%d %b %Y").to_string()
}

fn y(index: usize) -> usize {
    AXIS + index * ROW + ROW / 2 - 2
}

fn diamond(x: f64, y: usize, radius: f64, class: &str) -> Markup {
    let y = y as f64;
    html! { path class=(class) d=(format!("M {x:.1} {:.1} l {radius} {radius} -{radius} {radius} -{radius} -{radius} Z", y-radius)) {} }
}

fn svg_labels(title: &str, subtitle: &str, y: usize, kind: &str) -> Markup {
    html! {
        g clip-path="url(#rm-name-clip)" {
            g class="rm-state" aria-hidden="true" transform=(format!("translate(21 {})",y-6)) {
                @match kind {
                    "release" => path d="M0 -6 6 0 0 6 -6 0Z" {},
                    "reached" => path d="M-6 0 -2 4 6 -5" {},
                    "in_progress" => {
                        circle r="5.5" {}
                        path d="M0 -5.5a5.5 5.5 0 0 1 0 11Z" fill="currentColor" {}
                    },
                    _ => circle r="5.5" stroke-dasharray="2 2" {},
                }
            }
            text class="rm-name" x="38" y=(y-2) { (title) }
            text class="rm-meta" x="38" y=(y+15) { (subtitle) }
        }
    }
}

fn stamp(calendar: &Calendar, day: NaiveDate, y: usize, prefix: &str) -> Markup {
    let x = calendar.x(day);
    let anchor = if x - LEFT < 80.0 {
        "start"
    } else if RIGHT - x < 80.0 {
        "end"
    } else {
        "middle"
    };
    html! { text class="rm-date" x=(x) y=(y+19) text-anchor=(anchor) { (prefix) (day.format("%d %b")) } }
}

fn span(calendar: &Calendar, y: usize, start: Option<NaiveDate>, end: Option<NaiveDate>) -> Markup {
    html! {
        @if let (Some(a),Some(b)) = (start,end) {
            line class="rm-span" x1=(calendar.x(a)) x2=(calendar.x(b)) y1=(y) y2=(y) {}
        }
        @for day in [start,end].into_iter().flatten() {
            line class="rm-cap" x1=(calendar.x(day)) x2=(calendar.x(day)) y1=(y-5) y2=(y+5) {}
        }
    }
}

fn agenda_span(start: Option<NaiveDate>, end: Option<NaiveDate>) -> Markup {
    html! {
        @if let Some(d) = start { span { "Start " (date(d)) } }
        @if let Some(d) = end { span { "End " (date(d)) } }
    }
}

fn milestone_row(
    project: Uuid,
    m: &Milestone,
    i: usize,
    calendar: &Calendar,
    selected: Option<Uuid>,
    state: &str,
) -> Markup {
    let y = y(i);
    let class = format!(
        "rm-milestone rm-{}{}",
        m.status,
        if selected == Some(m.id) {
            " rm-selected"
        } else {
            ""
        }
    );
    let point = m.target_on.or(m.end_on).or(m.start_on);
    html! {
        a class=(class) href=(detail(project,m.id)) aria-current=[(selected == Some(m.id)).then_some("true")] aria-label=(format!("{}, {}, open milestone details",m.title,state)) {
            title { (m.title) " · " (state) @if let Some(d) = m.target_on { " · Target " (date(d)) } }
            rect class="rm-hit" x="0" y=(AXIS+i*ROW) width=(WIDTH) height=(ROW) {}
            (svg_labels(&m.title,state,y,&m.status))
            (span(calendar,y,m.start_on,m.end_on))
            @if let Some(target) = m.target_on {
                (diamond(calendar.x(target),y,5.5,"rm-target"))
            }
            @if let Some(day) = point { (stamp(calendar,day,y,if m.target_on.is_some() {"Target · "} else {""})) }
            @else { text class="rm-date" x=(LEFT) y=(y+4) { "Unscheduled" } }
        }
    }
}

fn release_row(r: &ReleaseDate, i: usize, calendar: &Calendar) -> Markup {
    let y = y(i);
    html! {
        a class="rm-release" href=(format!("/releases/{}",r.id)) aria-label=(format!("Open release {} {}",r.name,r.version)) {
            title { (r.name) " · " (r.version) @if let Some(d) = r.published_on { " · Shipped " (date(d)) } }
            rect class="rm-hit" x="0" y=(AXIS+i*ROW) width=(WIDTH) height=(ROW) {}
            (svg_labels(&r.name,&format!("Release · {}",r.version),y,"release"))
            (span(calendar,y,r.start_on,r.end_on))
            @if let Some(d) = r.target_on { (diamond(calendar.x(d),y,7.0,"rm-target")) }
            @if let Some(d) = r.published_on { (diamond(calendar.x(d),y,4.0,"rm-shipped")) }
            @if let Some(d) = r.published_on { (stamp(calendar,d,y,"Shipped · ")) }
            @else if let Some(d) = r.target_on { (stamp(calendar,d,y,"Target · ")) }
            @else if let Some(d) = r.end_on { (stamp(calendar,d,y,"End · ")) }
            @else if let Some(d) = r.start_on { (stamp(calendar,d,y,"Start · ")) }
            @else { text class="rm-date" x=(LEFT) y=(y+4) { "Unscheduled" } }
        }
    }
}

fn connector(x1: f64, y1: usize, x2: f64, y2: usize, highlighted: bool) -> Markup {
    // Enter the dependent's first mark horizontally. Leave room for its glyph and the arrowhead.
    let forward = x2 - x1 >= 26.0;
    let bend = ((x2 - x1).abs() / 2.0).clamp(16.0, 66.0);
    let (start, end, first_control, last_control) = if forward {
        (x1 + 7.0, x2 - 8.0, x1 + bend, x2 - bend)
    } else {
        (
            x1 + 7.0,
            x2 + 8.0,
            (x1 + bend).min(WIDTH as f64 - 3.0),
            (x2 + bend).min(WIDTH as f64 - 3.0),
        )
    };
    html! { path class=(if highlighted {"rm-edge rm-chain"} else {"rm-edge"})
    marker-end=(if highlighted {"url(#rm-arrow-chain)"} else {"url(#rm-arrow)"})
    d=(format!("M {start:.1} {y1} C {first_control:.1} {y1} {last_control:.1} {y2} {end:.1} {y2}")) {} }
}

struct Schedule<'a> {
    project: Uuid,
    milestones: &'a [Milestone],
    releases: &'a [ReleaseDate],
    weeks: &'a [WeekOff],
    deps: &'a [Dependency],
    index: HashMap<Uuid, usize>,
    relations: Relations,
    selected: Option<Uuid>,
}

impl Schedule<'_> {
    fn state(&self, m: &Milestone) -> String {
        let blocked = m.status != "reached"
            && self.relations.before.get(&m.id).is_some_and(|parents| {
                parents
                    .iter()
                    .filter_map(|id| self.index.get(id))
                    .any(|&i| self.milestones[i].status != "reached")
            });
        if blocked {
            format!("{} · Blocked", status(m))
        } else {
            status(m).into()
        }
    }

    fn graph(&self, calendar: &Calendar, today: NaiveDate) -> Markup {
        let rows = (self.milestones.len() + self.releases.len()).max(1);
        let height = AXIS + rows * ROW;
        let ticks = calendar.ticks();
        let chain = self.relations.upstream(self.selected);
        html! {
            div class="rm-scroll" tabindex="0" role="region" aria-label="Milestone timeline" {
                svg class="rm-svg" xmlns="http://www.w3.org/2000/svg" viewBox=(format!("0 0 {WIDTH} {height}")) role="group" aria-label="Calendar of milestones, prerequisites and releases" {
                    defs {
                        @for (id, class) in [("rm-arrow","rm-tip"),("rm-arrow-chain","rm-tip-chain")] {
                            marker id=(id) markerWidth="6" markerHeight="6" refX="5" refY="3" orient="auto" markerUnits="userSpaceOnUse" {
                                path class=(class) d="M 0 0 L 6 3 L 0 6 Z" {}
                            }
                        }
                        pattern id="rm-hatch" width="6" height="6" patternUnits="userSpaceOnUse" {
                            path class="rm-hatch-line" d="M -1 1 l 2 -2 M 0 6 l 6 -6 M 5 7 l 2 -2" {}
                        }
                        clipPath id="rm-name-clip" { rect x="0" y="0" width=(LEFT-18.0) height=(height) {} }
                    }
                    rect class="rm-axis-band" x="0" y="0" width=(WIDTH) height=(AXIS) {}
                    @if let Some(index) = self.selected.and_then(|id|self.index.get(&id)) {
                        rect class="rm-active-row" x="0" y=(AXIS+index*ROW) width=(WIDTH) height=(ROW) {}
                        rect class="rm-active-tick" x="0" y=(y(*index)-11) width="2" height="22" {}
                    }
                    @for w in self.weeks {
                        @let start = w.week_of.max(calendar.first);
                        @let end = after(w.week_of,7).min(calendar.last);
                        @if start < end {
                            rect class="rm-week" x=(calendar.x(start)) y=(AXIS) width=(calendar.x(end)-calendar.x(start)) height=(rows*ROW) {
                                title { "Unavailable week of " (date(w.week_of)) @if !w.note.is_empty() { " · " (w.note) } }
                            }
                        }
                    }
                    text class="rm-axis-title" x="13" y="40" { "Milestone / release" }
                    line class="rm-rule" x1=(LEFT-10.0) x2=(LEFT-10.0) y1="0" y2=(height) {}
                    @let last_tick = ticks.len().saturating_sub(1);
                    @for (index, d) in ticks.into_iter().enumerate() {
                        @let x = calendar.x(d);
                        line class="rm-grid" x1=(x) x2=(x) y1=(AXIS-8) y2=(height) {}
                        @if last_tick < 7 || index == last_tick || index % 2 == 0 {
                            text class="rm-axis" x=(x) y="40" text-anchor=(if x-LEFT < 24.0 { "start" } else if RIGHT-x < 24.0 { "end" } else { "middle" }) { (calendar.tick_label(d)) }
                        }
                    }
                    @for i in 0..=rows {
                        line class="rm-rule" x1="0" x2=(WIDTH) y1=(AXIS+i*ROW) y2=(AXIS+i*ROW) {}
                    }
                    @let now = calendar.x(today);
                    line class="rm-today-line" x1=(now) x2=(now) y1="23" y2=(height) {}
                    path class="rm-active-tick" d=(format!("M {now:.1} 26 l -4 -5 h 8 Z")) {}
                    text class="rm-today" x=(now) y="16" text-anchor="middle" { "Today" }
                    @for edge in self.deps {
                        @if let (Some(&a),Some(&b)) = (self.index.get(&edge.prerequisite_id),self.index.get(&edge.dependent_id)) {
                            @let from = &self.milestones[a];
                            @let to = &self.milestones[b];
                            @if let (Some(s),Some(t)) = (from.target_on.or(from.end_on).or(from.start_on), to.start_on.or(to.target_on).or(to.end_on)) {
                                (connector(calendar.x(s),y(a),calendar.x(t),y(b), chain.contains(&from.id) && chain.contains(&to.id)))
                            }
                        }
                    }
                    @for (i,m) in self.milestones.iter().enumerate() {
                        (milestone_row(self.project,m,i,calendar,self.selected,&self.state(m)))
                    }
                    @for (i,r) in self.releases.iter().enumerate() {
                        (release_row(r,self.milestones.len()+i,calendar))
                    }
                }
            }
            div class="rm-legend" {
                span { i class="rm-key-span" {} "Scheduled span" }
                span { i class="rm-key-target" {} "Target" }
                @if self.releases.iter().any(|r|r.published_on.is_some()) { span { i class="rm-key-shipped" {} "Shipped" } }
                @if !self.deps.is_empty() { span { b aria-hidden="true" { "→" } "Prerequisite to dependent" } }
                @if !self.weeks.is_empty() { span { i class="rm-key-off" {} "Unavailable week" } }
            }
        }
    }

    fn agenda(&self) -> Markup {
        let mut ordered: Vec<_> = self.milestones.iter().collect();
        ordered.sort_by_key(|m| {
            (
                m.start_on.or(m.target_on).or(m.end_on).is_none(),
                m.start_on.or(m.target_on).or(m.end_on),
                &m.title,
            )
        });
        html! {
            div class="rm-agenda-list" {
                @for m in ordered {
                    article class=(if self.selected==Some(m.id) {"rm-entry rm-entry-selected"} else {"rm-entry"}) {
                        div class="rm-entry-first" {
                            a href=(detail(self.project,m.id)) aria-current=[(self.selected == Some(m.id)).then_some("true")] { (m.title) }
                            span { (self.state(m)) }
                        }
                        @let parents = self.relations.before.get(&m.id).map(Vec::as_slice).unwrap_or_default();
                        @let primary = if let Some(d) = m.target_on { format!("Target {}",date(d)) }
                            else if let Some(d) = m.start_on { format!("Start {}",date(d)) }
                            else if let Some(d) = m.end_on { format!("End {}",date(d)) }
                            else { "Unscheduled".to_owned() };
                        @if m.start_on.is_some() || m.end_on.is_some() || !parents.is_empty() {
                            details.rm-agenda-detail {
                                summary {
                                    span { (primary) }
                                    @if !parents.is_empty() {
                                        span.rm-agenda-count { (parents.len()) " " @if parents.len() == 1 { "prerequisite" } @else { "prerequisites" } }
                                    }
                                }
                                @if m.start_on.is_some() || m.end_on.is_some() {
                                    p.rm-when { (agenda_span(m.start_on,m.end_on)) }
                                }
                                @if !parents.is_empty() {
                                    ul.rm-before aria-label="Prerequisites" {
                                        @for id in parents {
                                            @if let Some(&i) = self.index.get(id) {
                                                @let p = &self.milestones[i];
                                                li { a href=(detail(self.project,p.id)) { (p.title) } " · " (status(p)) }
                                            }
                                        }
                                    }
                                }
                            }
                        } @else { p.rm-when { (primary) } }
                    }
                }
                @for r in self.releases {
                    article class="rm-entry" {
                        div class="rm-entry-first" {
                            a href=(format!("/releases/{}",r.id)) { (r.name) }
                            span { "Release" @if !r.version.is_empty() { " · " (r.version) } }
                        }
                        @let primary = if let Some(d) = r.published_on { format!("Shipped {}",date(d)) }
                            else if let Some(d) = r.target_on { format!("Target {}",date(d)) }
                            else if let Some(d) = r.start_on { format!("Start {}",date(d)) }
                            else if let Some(d) = r.end_on { format!("End {}",date(d)) }
                            else { "Unscheduled".to_owned() };
                        @if r.start_on.is_some() || r.end_on.is_some() || (r.published_on.is_some() && r.target_on.is_some()) {
                            details.rm-agenda-detail {
                                summary { span { (primary) } }
                                p.rm-when {
                                    (agenda_span(r.start_on,r.end_on))
                                    @if let (Some(_),Some(d)) = (r.published_on,r.target_on) { span { "Target " (date(d)) } }
                                }
                            }
                        } @else { p.rm-when { (primary) } }
                    }
                }
                @for w in self.weeks {
                    article class="rm-entry" {
                        div class="rm-entry-first" { strong { "Unavailable week" } }
                        p class="rm-when" { (date(w.week_of)) " – " (date(after(w.week_of,6))) }
                        @if !w.note.is_empty() { p { (w.note) } }
                    }
                }
            }
        }
    }
}

pub(super) fn timeline(
    project: Uuid,
    milestones: &[Milestone],
    deps: &[Dependency],
    selected: Option<Uuid>,
    today: NaiveDate,
    weeks: &[WeekOff],
    releases: &[ReleaseDate],
) -> Markup {
    let dates = milestones
        .iter()
        .flat_map(|m| [m.start_on, m.end_on, m.target_on])
        .chain(
            releases
                .iter()
                .flat_map(|r| [r.start_on, r.end_on, r.target_on, r.published_on]),
        )
        .flatten();
    // A workspace can mark time away before its first milestone is scheduled.
    let mut dates = dates.peekable();
    let use_availability = dates.peek().is_none();
    let calendar = Calendar::covering(
        today,
        dates.chain(
            weeks
                .iter()
                .filter(|_| use_availability)
                .flat_map(|w| [w.week_of, after(w.week_of, 6)]),
        ),
    );
    let schedule = Schedule {
        project,
        milestones,
        deps,
        releases,
        weeks,
        selected,
        index: milestones
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id, i))
            .collect(),
        relations: Relations::new(deps),
    };
    html! {
        section class="rm-timeline" id="roadmap-schedule" aria-label="Roadmap calendar" tabindex="-1" {
            @if milestones.is_empty() && releases.is_empty() && weeks.is_empty() {
                div class="rm-empty" { h2 { "No milestones yet" } p { "Milestones, releases and unavailable weeks will appear on this schedule." } }
            } @else {
                div class="rm-views" {
                    input class="rm-toggle" id="rm-toggle-agenda" type="checkbox" aria-controls="rm-plot rm-agenda";
                    div class="rm-toolbar" {
                        div.rm-chart-title {
                            h2 { "Project schedule" }
                            p { (calendar.first.format("%d %b")) @if calendar.first.year()!=calendar.last.year() { " " (calendar.first.year()) } " – " (date(calendar.last)) }
                        }
                        label for="rm-toggle-agenda" class="rm-switch" {
                            span.rm-wide-label { "Agenda view" }
                            span.rm-narrow-label { "Timeline view" }
                        }
                    }
                    div id="rm-plot" { (schedule.graph(&calendar,today)) }
                    div id="rm-agenda" { (schedule.agenda()) }
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn nov(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, day).unwrap()
    }

    #[test]
    fn date_positions_follow_elapsed_days_including_today() {
        let calendar = Calendar::covering(nov(20), [nov(3), nov(30)]);
        assert!(calendar.first <= nov(3));
        assert!(calendar.last >= nov(30));
        assert_eq!(calendar.first.weekday(), chrono::Weekday::Mon);
        let (first, middle, last) = (calendar.x(nov(5)), calendar.x(nov(10)), calendar.x(nov(20)));
        assert!(((middle - first) * 2.0 - (last - middle)).abs() < 0.001);
        assert!(calendar.ticks().len() <= 9);
    }

    #[test]
    fn calendar_handles_one_day_and_multi_year_schedules() {
        let one = Calendar::covering(nov(20), [nov(20)]);
        assert!((one.last - one.first).num_days() >= 28);
        let long = Calendar::covering(nov(20), [NaiveDate::from_ymd_opt(2042, 4, 6).unwrap()]);
        let ticks = long.ticks();
        assert!(ticks.len() >= 2 && ticks.len() <= 9);
        assert!(ticks.windows(2).all(|t| t[0] < t[1]));
    }

    #[test]
    fn release_markers_and_week_bands_have_text_equivalents() {
        let id = Uuid::from_u128(12);
        let weeks = [WeekOff {
            week_of: nov(16),
            note: "<away>".into(),
        }];
        let releases = [ReleaseDate {
            id,
            name: "Release A".into(),
            version: "0.2".into(),
            start_on: Some(nov(5)),
            end_on: Some(nov(15)),
            span_revision: 1,
            target_on: Some(nov(18)),
            published_on: Some(nov(20)),
        }];
        let output =
            timeline(Uuid::nil(), &[], &[], None, nov(20), &weeks, &releases).into_string();
        for expected in [
            "rm-week",
            "Start 05 Nov 2026",
            "End 15 Nov 2026",
            "rm-shipped",
            "rm-target",
            "Target 18 Nov 2026",
            "Shipped 20 Nov 2026",
            "&lt;away&gt;",
            "/releases/00000000-0000-0000-0000-00000000000c",
        ] {
            assert!(output.contains(expected), "missing {expected}");
        }
        assert!(!output.contains("<away>"));
    }

    #[test]
    fn unscheduled_releases_are_visible_in_both_views() {
        let releases = [ReleaseDate {
            id: Uuid::nil(),
            name: "Preview".into(),
            version: "0.1".into(),
            start_on: None,
            end_on: None,
            span_revision: 0,
            target_on: None,
            published_on: None,
        }];
        let out = timeline(Uuid::nil(), &[], &[], None, nov(20), &[], &releases).into_string();
        assert_eq!(out.matches(">Unscheduled<").count(), 2);
        assert!(!out.contains("No milestones yet"));
    }

    #[test]
    fn a_release_span_sets_the_axis_without_a_target_or_shipment() {
        let start = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let end = NaiveDate::from_ymd_opt(2027, 2, 15).unwrap();
        let releases = [ReleaseDate {
            id: Uuid::nil(),
            name: "Beta".into(),
            version: "2".into(),
            start_on: Some(start),
            end_on: Some(end),
            target_on: None,
            published_on: None,
            span_revision: 1,
        }];
        let calendar = Calendar::covering(nov(20), [start, end]);
        let out = timeline(Uuid::nil(), &[], &[], None, nov(20), &[], &releases).into_string();
        assert!(out.contains(&format!(
            "class=\"rm-span\" x1=\"{}\" x2=\"{}\"",
            calendar.x(start),
            calendar.x(end)
        )));
        assert_eq!(out.matches("class=\"rm-cap\"").count(), 2);
        assert!(out.contains("Start 03 Aug 2026"));
        assert!(out.contains("End 15 Feb 2027"));
        assert!(out.contains("End · 15 Feb"));
        assert!(!out.contains(">Unscheduled<"));
        assert!(!out.contains("class=\"rm-shipped\""));
    }

    #[test]
    fn selected_chain_excludes_unrelated_and_downstream_dependencies() {
        let [a, b, c, d] = [1, 2, 3, 4].map(Uuid::from_u128);
        let edges = [(a, b), (b, c), (a, d)].map(|(prerequisite_id, dependent_id)| Dependency {
            prerequisite_id,
            dependent_id,
        });
        assert_eq!(
            Relations::new(&edges).upstream(Some(c)),
            HashSet::from([a, b, c])
        );
        assert!(Relations::new(&edges).upstream(None).is_empty());
    }
}
