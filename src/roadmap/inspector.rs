use std::collections::{HashMap, HashSet, VecDeque};

use chrono::NaiveDate;
use maud::{Markup, html};
use uuid::Uuid;

use super::{Dependency, IssueChoice, Milestone, calendar::ReleaseDate};
use crate::{auth::Auth, store::Project, ui::csrf};

pub(super) struct Selection<'a> {
    pub(super) project: &'a Project,
    pub(super) milestone: &'a Milestone,
    pub(super) milestones: &'a [Milestone],
    pub(super) dependencies: &'a [Dependency],
    pub(super) issues: &'a [IssueChoice],
    pub(super) candidates: &'a [IssueChoice],
    pub(super) search: &'a str,
    pub(super) releases: &'a [ReleaseDate],
    pub(super) release_id: Option<Uuid>,
}

fn status_name(value: &str) -> &str {
    match value {
        "planned" => "Planned",
        "in_progress" => "In progress",
        "reached" => "Reached",
        "backlog" => "Backlog",
        "ready" => "Ready",
        "done" => "Done",
        "canceled" => "Canceled",
        other => other,
    }
}

fn date_text(date: Option<NaiveDate>) -> String {
    date.map(|d| d.to_string()).unwrap_or_default()
}

fn display_date(date: NaiveDate) -> Markup {
    html! { time datetime=(date) { (date.format("%d %b %Y")) } }
}

fn edit_fields(item: Option<&Milestone>) -> Markup {
    let status = item.map_or("planned", |m| &m.status);
    html! {
        label.field { "Title" input type="text" name="title" required maxlength="200"
            value=(item.map_or("", |m| m.title.as_str())); }
        label.field { "Status" select name="status" {
            @for value in ["planned", "in_progress", "reached"] {
                option value=(value) selected[value == status] { (status_name(value)) }
            }
        } }
        div.grid {
            label.field { "Start date" input type="date" name="start_on"
                value=(date_text(item.and_then(|m| m.start_on))); }
            label.field { "End date" input type="date" name="end_on"
                value=(date_text(item.and_then(|m| m.end_on))); }
            label.field { "Target date" input type="date" name="target_on"
                value=(date_text(item.and_then(|m| m.target_on))); }
        }
        p.muted { "Set both span dates to schedule work. A milestone can have a target on its own." }
        label.field { "Details" textarea name="details" maxlength="8000" rows="3" {
            (item.map_or("", |m| m.details.as_str()))
        } }
    }
}

pub(super) fn creation(auth: &Auth, project: Uuid, expanded: bool) -> Markup {
    html! {
        details.panel id="new-milestone" open[expanded] {
            summary { "New milestone" }
            form.stack method="post" action=(format!("/roadmap/{project}/milestones")) {
                (csrf(auth))
                input type="hidden" name="id" value=(Uuid::new_v4());
                (edit_fields(None))
                button type="submit" { "Create milestone" }
            }
        }
    }
}

fn milestone_link(project: Uuid, item: &Milestone) -> Markup {
    html! {
        a href=(format!("/roadmap?project={project}&milestone={}#milestone-detail", item.id)) { (item.title) }
    }
}

// Keep one shortest witness per downstream milestone so a diamond does not repeat its endpoint.
fn downstream(start: Uuid, dependencies: &[Dependency]) -> HashMap<Uuid, Uuid> {
    let mut outgoing: HashMap<_, Vec<_>> = HashMap::new();
    for edge in dependencies {
        outgoing
            .entry(edge.prerequisite_id)
            .or_default()
            .push(edge.dependent_id);
    }
    let mut queue = VecDeque::from([start]);
    let mut parents = HashMap::new();
    while let Some(current) = queue.pop_front() {
        for &next in outgoing.get(&current).into_iter().flatten() {
            if next != start
                && let std::collections::hash_map::Entry::Vacant(slot) = parents.entry(next)
            {
                slot.insert(current);
                queue.push_back(next);
            }
        }
    }
    parents
}

fn edge_button(auth: &Auth, action: &str, field: &str, value: Uuid, label: &str) -> Markup {
    html! {
        form method="post" action=(action) {
            (csrf(auth))
            input type="hidden" name=(field) value=(value);
            input type="hidden" name="operation" value="remove";
            button.secondary type="submit" { (label) }
        }
    }
}

fn dependencies(auth: &Auth, s: &Selection<'_>, base: &str) -> Markup {
    let item = s.milestone;
    let project = s.project.id;
    let project_items: HashMap<_, _> = s.milestones.iter().map(|m| (m.id, m)).collect();
    let prerequisites: HashSet<_> = s
        .dependencies
        .iter()
        .filter(|e| e.dependent_id == item.id)
        .map(|e| e.prerequisite_id)
        .collect();
    let after = downstream(item.id, s.dependencies);
    let action = format!("{base}/dependencies");
    let eligible: Vec<_> = s
        .milestones
        .iter()
        .filter(|m| m.id != item.id && !prerequisites.contains(&m.id) && !after.contains_key(&m.id))
        .collect();
    html! {
        section {
            h3 { "Prerequisites" }
            @if prerequisites.is_empty() { p.muted { "No prerequisites." } }
            @else {
                ul.issue-list {
                    @for m in s.milestones.iter().filter(|m| prerequisites.contains(&m.id)) {
                        li.row {
                            (milestone_link(project, m))
                            span.status {
                                @if m.status != "reached" && item.status != "reached" { "Blocking · " }
                                (status_name(&m.status))
                            }
                            @if !s.project.archived {
                                (edge_button(auth, &action, "prerequisite_id", m.id, "Remove prerequisite"))
                            }
                        }
                    }
                }
            }
            @if !s.project.archived && !eligible.is_empty() {
                details {
                    summary { "Add prerequisite" }
                    form.row method="post" action=(action) {
                        (csrf(auth))
                        input type="hidden" name="operation" value="add";
                        label.field { "Required milestone" select name="prerequisite_id" required {
                            option value="" { "Choose a milestone" }
                            @for m in eligible {
                                option value=(m.id) { (m.title) " · " (status_name(&m.status)) }
                            }
                        } }
                        button type="submit" { "Add prerequisite" }
                    }
                }
            }
        }
        @if !after.is_empty() {
            section {
                h3 { "Downstream milestones" }
                p.muted { "Review these targets when rescheduling. Each date has its own save." }
                ul.issue-list {
                    @for m in s.milestones {
                        @if let Some(parent) = after.get(&m.id) {
                            li {
                                (milestone_link(project, m)) " · " (status_name(&m.status))
                                @if let Some(date) = m.target_on { " · Target " (display_date(date)) }
                                @if *parent == item.id { " · Direct dependent" }
                                @else if let Some(via) = project_items.get(parent) { " · Via " (milestone_link(project, via)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn linked_issues(auth: &Auth, s: &Selection<'_>, base: &str) -> Markup {
    let action = format!("{base}/issues");
    html! {
        section {
            h3 { "Linked issues" }
            @if s.issues.is_empty() { p.muted { "No linked issues." } }
            @else {
                ul.issue-list {
                    @for issue in s.issues {
                        li.row {
                            a href=(format!("/issues/{}", issue.id)) {
                                (s.project.key) "-" (issue.number) " · " (issue.title)
                            }
                            span.status { (status_name(&issue.status)) }
                            @if issue.archived { span.muted { "Archived" } }
                            @if !s.project.archived {
                                (edge_button(auth, &action, "issue_id", issue.id, "Unlink"))
                            }
                        }
                    }
                }
            }
            @if !s.project.archived {
                details open[!s.search.is_empty()] {
                    summary { "Link an issue" }
                    form.row method="get" action="/roadmap#milestone-detail" {
                        input type="hidden" name="project" value=(s.project.id);
                        input type="hidden" name="milestone" value=(s.milestone.id);
                        label.field { "Search project issues" input type="search" name="issue_q" value=(s.search); }
                        button.secondary type="submit" { "Search" }
                    }
                    @if s.candidates.is_empty() { p.muted { "No matching issues available to link." } }
                    @else {
                        form.row method="post" action=(action) {
                            (csrf(auth))
                            input type="hidden" name="operation" value="add";
                            label.field { "Issue" select name="issue_id" required {
                                option value="" { "Choose an issue" }
                                @for issue in s.candidates {
                                    option value=(issue.id) {
                                        (s.project.key) "-" (issue.number) " · " (issue.title) " · " (status_name(&issue.status))
                                    }
                                }
                            } }
                            button type="submit" { "Link issue" }
                        }
                    }
                }
            }
        }
    }
}

fn release_label(release: &ReleaseDate) -> String {
    if release.version.is_empty() || release.name == release.version {
        release.name.clone()
    } else {
        format!("{} · {}", release.name, release.version)
    }
}

fn linked_release(auth: &Auth, s: &Selection<'_>, base: &str) -> Markup {
    let linked = s.releases.iter().find(|r| Some(r.id) == s.release_id);
    html! {
        @if let Some(release) = linked {
            p {
                "Release · " a href=(format!("/releases/{}", release.id)) { (release_label(release)) }
                @if let Some(date) = release.published_on { " · Published " (display_date(date)) }
                @else if let Some(date) = release.target_on { " · Target " (display_date(date)) }
            }
        }
        @if !s.project.archived && !s.releases.is_empty() {
            details {
                summary { @if linked.is_some() { "Change release" } @else { "Assign release" } }
                form.row method="post" action=(format!("{base}/release")) {
                    (csrf(auth))
                    input type="hidden" name="revision" value=(s.milestone.revision);
                    label.field { "Release" select name="release_id" {
                        option value="" selected[linked.is_none()] { "No release" }
                        @for r in s.releases {
                            option value=(r.id) selected[Some(r.id) == s.release_id] { (release_label(r)) }
                        }
                    } }
                    button type="submit" { "Save release" }
                }
            }
        }
    }
}

pub(super) fn inspector(auth: &Auth, selection: Selection<'_>) -> Markup {
    let s = selection;
    let item = s.milestone;
    let base = format!("/roadmap/{}/milestones/{}", s.project.id, item.id);
    html! {
        section.panel.stack id="milestone-detail" aria-labelledby="selected-milestone-title" tabindex="-1" {
            div.row {
                h2 id="selected-milestone-title" { (item.title) }
                span.status { (status_name(&item.status)) }
            }
            p {
                @if let (Some(start), Some(end)) = (item.start_on, item.end_on) {
                    "Scheduled " (display_date(start)) " – " (display_date(end))
                } @else { "No scheduled span" }
                @if let Some(target) = item.target_on { " · Target " (display_date(target)) }
                @else { " · No target" }
            }
            @if !item.details.is_empty() { p { (item.details) } }
            (linked_release(auth, &s, &base))
            (dependencies(auth, &s, &base))
            (linked_issues(auth, &s, &base))
            @if !s.project.archived {
                details {
                    summary { "Edit milestone" }
                    form.stack method="post" action=(base) {
                        (csrf(auth))
                        input type="hidden" name="revision" value=(item.revision);
                        (edit_fields(Some(item)))
                        button type="submit" { "Save milestone" }
                    }
                }
                details {
                    summary { "Delete milestone" }
                    p { "Deleting this milestone permanently removes its schedule and its links to issues, releases and other milestones." }
                    form method="post" action=(format!("{base}/delete")) {
                        (csrf(auth))
                        input type="hidden" name="revision" value=(item.revision);
                        button.danger type="submit" { "Permanently delete milestone" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downstream_keeps_a_shortest_witness_and_excludes_selected() {
        let [selected, left, right, joined, unrelated] = [1, 2, 3, 4, 5].map(Uuid::from_u128);
        let edges = [
            (left, joined),
            (selected, left),
            (selected, right),
            (right, joined),
        ]
        .map(|(prerequisite_id, dependent_id)| Dependency {
            prerequisite_id,
            dependent_id,
        });
        let after = downstream(selected, &edges);
        assert_eq!(after.len(), 3);
        assert_eq!(after[&left], selected);
        assert_eq!(after[&right], selected);
        assert_eq!(after[&joined], left);
        assert!(!after.contains_key(&unrelated));
        assert!(!after.contains_key(&selected));
    }
}
