use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{Datelike, NaiveDate};
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
        label.field { "Target date" input type="date" name="target_on"
            value=(date_text(item.and_then(|m| m.target_on))); }
        details.rm-more-fields open[item.is_some()] {
            summary { "Status, schedule and details" }
            div.form-stack {
                label.field { "Status" select name="status" {
                    @for value in ["planned", "in_progress", "reached"] {
                        option value=(value) selected[value == status] { (status_name(value)) }
                    }
                } }
                div.rm-date-fields {
                    label.field { "Start date" input type="date" name="start_on"
                        value=(date_text(item.and_then(|m| m.start_on))); }
                    label.field { "End date" input type="date" name="end_on"
                        value=(date_text(item.and_then(|m| m.end_on))); }
                }
                p.muted { "Enter both dates to schedule a span." }
                label.field { "Details" textarea name="details" maxlength="8000" rows="3" {
                    (item.map_or("", |m| m.details.as_str()))
                } }
            }
        }
    }
}

pub(super) fn creation(auth: &Auth, project: Uuid, expanded: bool) -> Markup {
    html! {
        details.rm-create id="new-milestone" open[expanded] {
            summary { "New milestone" }
            form.form-stack method="post" action=(format!("/roadmap/{project}/milestones")) {
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

fn dependency_list(
    s: &Selection<'_>,
    prerequisites: &HashSet<Uuid>,
    after: &HashMap<Uuid, Uuid>,
) -> Markup {
    let project = s.project.id;
    let project_items: HashMap<_, _> = s.milestones.iter().map(|m| (m.id, m)).collect();
    html! {
        @if !prerequisites.is_empty() {
            section.rm-relations {
                h3 { "Prerequisites" }
                ul.rm-links {
                    @for m in s.milestones.iter().filter(|m| prerequisites.contains(&m.id)) {
                        li {
                            (milestone_link(project, m))
                            small {
                                @if m.status != "reached" && s.milestone.status != "reached" { "Blocking · " }
                                (status_name(&m.status))
                            }
                        }
                    }
                }
            }
        }
        @if !after.is_empty() {
            section.rm-relations {
                h3 { "Downstream milestones" }
                ul.rm-links {
                    @for m in s.milestones {
                        @if let Some(parent) = after.get(&m.id) {
                            li {
                                (milestone_link(project, m))
                                small {
                                    (status_name(&m.status))
                                    @if let Some(date) = m.target_on { " · Target " (display_date(date)) }
                                    @if *parent == s.milestone.id { " · Direct dependent" }
                                    @else if let Some(via) = project_items.get(parent) { " · Via " (milestone_link(project, via)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn linked_issues(s: &Selection<'_>) -> Markup {
    html! {
        @if !s.issues.is_empty() {
            section.rm-linked {
                h3 { "Linked issues" span.rm-count { (s.issues.len()) } }
                ul.rm-links.rm-issue-links {
                    @for issue in s.issues {
                        li {
                            a href=(format!("/issues/{}", issue.id)) {
                                span.rm-key { (s.project.key) "-" (issue.number) }
                                span { (issue.title) }
                                small { (status_name(&issue.status)) @if issue.archived { " · Archived" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn edit_issue_links(auth: &Auth, s: &Selection<'_>, base: &str) -> Markup {
    let action = format!("{base}/issues");
    html! {
        section.rm-relationship-group {
            h3 { "Issue links" }
            form.form-stack method="get" action="/roadmap#milestone-link-results" {
                input type="hidden" name="project" value=(s.project.id);
                input type="hidden" name="milestone" value=(s.milestone.id);
                label.field { "Search project issues" input type="search" name="issue_q" value=(s.search); }
                button.secondary type="submit" { "Search" }
            }
            div id="milestone-link-results" tabindex="-1" {
                @if s.candidates.is_empty() {
                    p.muted {
                        @if s.search.is_empty() { "No other project issues to link." }
                        @else { "No matching issues available to link." }
                    }
                } @else {
                    form.form-stack method="post" action=(&action) {
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
                        button.secondary type="submit" { "Link issue" }
                    }
                }
            }
            @if !s.issues.is_empty() {
                form.form-stack method="post" action=(&action) {
                    (csrf(auth))
                    input type="hidden" name="operation" value="remove";
                    label.field { "Linked issue" select name="issue_id" required {
                        option value="" { "Choose an issue" }
                        @for issue in s.issues { option value=(issue.id) { (s.project.key) "-" (issue.number) " · " (issue.title) } }
                    } }
                    button.secondary type="submit" { "Unlink issue" }
                }
            }
        }
    }
}

fn edit_prerequisites(
    auth: &Auth,
    s: &Selection<'_>,
    base: &str,
    prerequisites: &HashSet<Uuid>,
    after: &HashMap<Uuid, Uuid>,
) -> Markup {
    let action = format!("{base}/dependencies");
    let eligible: Vec<_> = s
        .milestones
        .iter()
        .filter(|m| {
            m.id != s.milestone.id && !prerequisites.contains(&m.id) && !after.contains_key(&m.id)
        })
        .collect();
    html! {
        @if !eligible.is_empty() || !prerequisites.is_empty() {
            section.rm-relationship-group {
                h3 { "Prerequisites" }
                @if !eligible.is_empty() {
                    form.form-stack method="post" action=(&action) {
                        (csrf(auth))
                        input type="hidden" name="operation" value="add";
                        label.field { "Required milestone" select name="prerequisite_id" required {
                            option value="" { "Choose a milestone" }
                            @for m in eligible {
                                option value=(m.id) { (m.title) " · " (status_name(&m.status)) }
                            }
                        } }
                        button.secondary type="submit" { "Add prerequisite" }
                    }
                }
                @if !prerequisites.is_empty() {
                    form.form-stack method="post" action=(&action) {
                        (csrf(auth))
                        input type="hidden" name="operation" value="remove";
                        label.field { "Existing prerequisite" select name="prerequisite_id" required {
                            option value="" { "Choose a milestone" }
                            @for m in s.milestones.iter().filter(|m| prerequisites.contains(&m.id)) {
                                option value=(m.id) { (m.title) }
                            }
                        } }
                        button.secondary type="submit" { "Remove prerequisite" }
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

fn edit_release(auth: &Auth, s: &Selection<'_>, base: &str) -> Markup {
    html! {
        @if !s.releases.is_empty() {
            form.form-stack method="post" action=(format!("{base}/release")) {
                (csrf(auth))
                input type="hidden" name="revision" value=(s.milestone.revision);
                label.field { "Release" select name="release_id" {
                    option value="" selected[s.release_id.is_none()] { "No release" }
                    @for r in s.releases {
                        option value=(r.id) selected[Some(r.id) == s.release_id] { (release_label(r)) }
                    }
                } }
                button.secondary type="submit" { "Save release" }
            }
        }
    }
}

pub(super) fn inspector(auth: &Auth, selection: Selection<'_>) -> Markup {
    let s = selection;
    let item = s.milestone;
    let base = format!("/roadmap/{}/milestones/{}", s.project.id, item.id);
    let prerequisites: HashSet<_> = s
        .dependencies
        .iter()
        .filter(|edge| edge.dependent_id == item.id)
        .map(|edge| edge.prerequisite_id)
        .collect();
    let after = downstream(item.id, s.dependencies);
    let release = s.releases.iter().find(|r| Some(r.id) == s.release_id);
    html! {
        section.rm-inspector id="milestone-detail" aria-labelledby="selected-milestone-title" tabindex="-1" {
            header.rm-inspector-head {
                div.rm-inspector-context { span { "Selected milestone" } a href="#roadmap-schedule" { "View schedule" } }
                h2 id="selected-milestone-title" { (item.title) }
                @if !item.details.is_empty() { p.reading { (item.details) } }
            }
            dl.rm-facts {
                div { dt { "Status" } dd { (status_name(&item.status)) } }
                div { dt { "Target" } dd { @if let Some(target) = item.target_on { (display_date(target)) } @else { "No target" } } }
                div { dt { "Schedule" } dd {
                    @if let (Some(start), Some(end)) = (item.start_on, item.end_on) {
                        time datetime=(start) { (start.format(if start.year() == end.year() { "%d %b" } else { "%d %b %Y" })) } " – " (display_date(end))
                    } @else { "No scheduled span" }
                } }
                @if let Some(r) = release {
                    div.rm-release-link { dt { "Release" } dd {
                        a href=(format!("/releases/{}", r.id)) { (release_label(r)) }
                        @if let Some(date) = r.published_on { " · Published " (display_date(date)) }
                        @else if let Some(date) = r.target_on { " · Target " (display_date(date)) }
                    } }
                }
            }
            (linked_issues(&s))
            (dependency_list(&s, &prerequisites, &after))
            @if s.issues.is_empty() && prerequisites.is_empty() && after.is_empty() && release.is_none() {
                p.muted { "No linked issues, milestones or release." }
            }
            @if !s.project.archived {
                details.rm-relationship-editor open[!s.search.is_empty()] {
                    summary { "Manage relationships" }
                    (edit_issue_links(auth, &s, &base))
                    (edit_prerequisites(auth, &s, &base, &prerequisites, &after))
                    (edit_release(auth, &s, &base))
                }
                details {
                    summary { "Edit milestone" }
                    form.form-stack method="post" action=(base) {
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
