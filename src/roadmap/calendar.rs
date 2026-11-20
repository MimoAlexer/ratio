use axum::{
    Form,
    extract::{Path, State},
    response::Redirect,
};
use chrono::{Datelike, Days, NaiveDate};
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, store, ui::csrf};

#[derive(Clone, sqlx::FromRow)]
pub(super) struct WeekOff {
    pub(super) week_of: NaiveDate,
    pub(super) note: String,
}

#[derive(Clone, sqlx::FromRow)]
pub(super) struct ReleaseDate {
    pub(super) id: Uuid,
    pub(super) name: String,
    pub(super) version: String,
    pub(super) target_on: Option<NaiveDate>,
    pub(super) published_on: Option<NaiveDate>,
    pub(super) start_on: Option<NaiveDate>,
    pub(super) end_on: Option<NaiveDate>,
    pub(super) span_revision: i64,
}

pub(super) async fn load_calendar(
    db: &PgPool,
    owner: Uuid,
    project: Uuid,
) -> AppResult<(Vec<WeekOff>, Vec<ReleaseDate>)> {
    let (weeks, releases) = tokio::try_join!(
        sqlx::query_as::<_, WeekOff>(
            "SELECT week_of, note FROM roadmap_weeks_off
             WHERE owner_id = $1 AND project_id = $2 ORDER BY week_of",
        )
        .bind(owner)
        .bind(project)
        .fetch_all(db),
        sqlx::query_as::<_, ReleaseDate>(
            "SELECT r.id, r.name, r.version, r.target_on,
                    CASE WHEN r.published_at IS NOT NULL THEN r.published_on END AS published_on,
                    s.start_on, s.end_on, COALESCE(s.revision, 0) AS span_revision
             FROM releases r LEFT JOIN roadmap_release_spans s
               ON (s.owner_id, s.project_id, s.release_id) = (r.owner_id, r.project_id, r.id)
             WHERE r.owner_id = $1 AND r.project_id = $2
             ORDER BY COALESCE(CASE WHEN r.published_at IS NOT NULL THEN r.published_on END,
                               r.target_on, s.start_on) NULLS LAST, r.name, r.id",
        )
        .bind(owner)
        .bind(project)
        .fetch_all(db),
    )?;
    Ok((weeks, releases))
}

#[derive(Deserialize)]
pub(super) struct AvailabilityForm {
    csrf: String,
    date: NaiveDate,
    action: AvailabilityAction,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum AvailabilityAction {
    Save,
    Remove,
}

const NOTE_LIMIT: usize = 200;

fn week_start(date: NaiveDate) -> AppResult<NaiveDate> {
    if !(1..=9999).contains(&date.year()) {
        return Err(AppError::BadRequest(
            "Choose a four-digit calendar year.".into(),
        ));
    }
    date.checked_sub_days(Days::new(date.weekday().num_days_from_monday().into()))
        .ok_or_else(|| AppError::BadRequest("Choose a valid calendar date.".into()))
}

pub(super) async fn save_availability(
    State(state): State<AppState>,
    auth: Auth,
    Path(project): Path<Uuid>,
    Form(form): Form<AvailabilityForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let monday = week_start(form.date)?;
    let mut tx = state.db.begin().await?;
    store::lock_project(&mut tx, auth.user_id, project).await?;
    match form.action {
        AvailabilityAction::Save => {
            let note = form.note.trim();
            if note.contains('\0') || note.chars().count() > NOTE_LIMIT {
                return Err(AppError::BadRequest(format!(
                    "Keep the note to {NOTE_LIMIT} characters."
                )));
            }
            sqlx::query(
                "INSERT INTO roadmap_weeks_off (owner_id, project_id, week_of, note)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (owner_id, project_id, week_of) DO UPDATE SET note = EXCLUDED.note",
            )
            .bind(auth.user_id)
            .bind(project)
            .bind(monday)
            .bind(note)
            .execute(&mut *tx)
            .await?;
        }
        AvailabilityAction::Remove => {
            sqlx::query(
                "DELETE FROM roadmap_weeks_off
                 WHERE owner_id = $1 AND project_id = $2 AND week_of = $3",
            )
            .bind(auth.user_id)
            .bind(project)
            .bind(monday)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(Redirect::to(&format!(
        "/roadmap?project={project}#availability"
    )))
}

pub(super) fn calendar_editor(
    auth: &Auth,
    project: Uuid,
    archived: bool,
    weeks: &[WeekOff],
) -> Markup {
    let action = format!("/roadmap/{project}/availability");
    html! {
        details {
            summary { "Unavailable weeks" @if !weeks.is_empty() { " (" (weeks.len()) ")" } }
            @if weeks.is_empty() {
                p id="availability" class="muted" { "No weeks marked unavailable." }
            } @else {
                ul id="availability" {
                    @for week in weeks {
                        li class="row" {
                            span {
                                "Week of " time datetime=(week.week_of) { (week.week_of.format("%-d %b %Y")) }
                                @if !week.note.is_empty() { " · " (week.note) }
                            }
                            @if !archived {
                                form method="post" action=(&action) {
                                    (csrf(auth))
                                    input type="hidden" name="action" value="remove";
                                    input type="hidden" name="date" value=(week.week_of);
                                    button type="submit" class="secondary" aria-label=(format!("Remove unavailable week of {}", week.week_of)) { "Remove" }
                                }
                            }
                        }
                    }
                }
            }
            @if !archived {
                form method="post" action=(&action) class="stack" {
                    (csrf(auth))
                    input type="hidden" name="action" value="save";
                    label class="field" { "Any date in the week" input type="date" name="date" required min="0001-01-01" max="9999-12-31"; }
                    label class="field" { "Note (optional)" input name="note" maxlength=(NOTE_LIMIT); }
                    p class="muted" { "Weeks start on Monday. Saving the same week updates its note." }
                    button type="submit" { "Save unavailable week" }
                }
            }
        }
    }
}

#[derive(Deserialize)]
pub(super) struct ReleaseSpanForm {
    pub(super) csrf: String,
    pub(super) revision: i64,
    pub(super) start_on: String,
    pub(super) end_on: String,
}

pub(super) async fn save_release_span(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, release)): Path<(Uuid, Uuid)>,
    Form(form): Form<ReleaseSpanForm>,
) -> AppResult<Redirect> {
    auth.check_csrf(&form.csrf)?;
    let start = super::writes::calendar_date(&form.start_on)?;
    let end = super::writes::calendar_date(&form.end_on)?;
    if start.is_some() != end.is_some() || start > end {
        return Err(AppError::BadRequest(
            "Enter both span dates, with the end on or after the start.".into(),
        ));
    }

    let mut tx = state.db.begin().await?;
    store::lock_project(&mut tx, auth.user_id, project).await?;
    let (published, revision): (bool, i64) = sqlx::query_as(
        "SELECT r.published_at IS NOT NULL, COALESCE(s.revision, 0)
         FROM releases r LEFT JOIN roadmap_release_spans s
           ON (s.owner_id, s.project_id, s.release_id) = (r.owner_id, r.project_id, r.id)
         WHERE r.owner_id = $1 AND r.project_id = $2 AND r.id = $3 FOR UPDATE OF r",
    )
    .bind(auth.user_id)
    .bind(project)
    .bind(release)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if published {
        return Err(AppError::Conflict(
            "This release has already shipped.".into(),
        ));
    }
    if revision != form.revision {
        return Err(AppError::Conflict(
            "The release schedule changed. Reload the roadmap before saving.".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO roadmap_release_spans (owner_id, project_id, release_id, start_on, end_on, revision)
         VALUES ($1, $2, $3, $4, $5, 1)
         ON CONFLICT (owner_id, project_id, release_id) DO UPDATE
         SET start_on = EXCLUDED.start_on, end_on = EXCLUDED.end_on,
             revision = roadmap_release_spans.revision + 1",
    )
    .bind(auth.user_id)
    .bind(project)
    .bind(release)
    .bind(start)
    .bind(end)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!(
        "/roadmap?project={project}#release-schedules"
    )))
}

pub(super) fn release_editor(
    auth: &Auth,
    project: Uuid,
    archived: bool,
    releases: &[ReleaseDate],
) -> Markup {
    html! {
        details {
            summary { "Release schedules" @if !releases.is_empty() { " (" (releases.len()) ")" } }
            div id="release-schedules" class="stack" {
                @if releases.is_empty() {
                    p class="muted" { "No releases in this project. " a href=(format!("/releases?project={project}")) { "Open releases" } }
                }
                @for release in releases {
                    section class="stack" {
                        h3 {
                            a href=(format!("/releases/{}", release.id)) { (release.name) }
                            @if !release.version.is_empty() { " " span class="muted" { (release.version) } }
                        }
                        p class="muted" {
                            @if let Some(target) = release.target_on {
                                "Target " time datetime=(target) { (target.format("%-d %b %Y")) }
                            } @else { "No target date" }
                            @if let Some(shipped) = release.published_on {
                                " · Shipped " time datetime=(shipped) { (shipped.format("%-d %b %Y")) }
                            }
                        }
                        @if !archived && release.published_on.is_none() {
                            form method="post" action=(format!("/roadmap/{project}/releases/{}/span", release.id)) class="stack" {
                                (csrf(auth))
                                input type="hidden" name="revision" value=(release.span_revision);
                                div class="row" {
                                    label class="field" { "Span start" input type="date" name="start_on" min="0001-01-01" max="9999-12-31" value=(release.start_on.map(|date| date.to_string()).unwrap_or_default()); }
                                    label class="field" { "Span end" input type="date" name="end_on" min="0001-01-01" max="9999-12-31" value=(release.end_on.map(|date| date.to_string()).unwrap_or_default()); }
                                }
                                button type="submit" class="secondary" aria-label=(format!("Save schedule for {}", release.name)) { "Save schedule" }
                            }
                        } @else if let (Some(start), Some(end)) = (release.start_on, release.end_on) {
                            p { "Scheduled " time datetime=(start) { (start.format("%-d %b %Y")) } " – " time datetime=(end) { (end.format("%-d %b %Y")) } }
                        }
                    }
                }
                @if !archived && releases.iter().any(|release| release.published_on.is_none()) {
                    p class="muted" { "Clear both span dates and save to remove a schedule." }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_in_a_week_share_one_monday_across_year_boundaries() {
        let monday = NaiveDate::from_ymd_opt(2025, 12, 29).unwrap();
        for days in 0..7 {
            let date = monday.checked_add_days(Days::new(days)).unwrap();
            assert_eq!(week_start(date).unwrap(), monday);
        }
        let next = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        assert_eq!(week_start(next).unwrap(), next);
    }

    #[test]
    fn rejects_dates_outside_form_range() {
        assert!(week_start(NaiveDate::MIN).is_err());
        assert!(week_start(NaiveDate::MAX).is_err());
    }
}
