use axum::{
    Router,
    extract::{Form, State},
    response::Html,
    routing::post,
};
use maud::{Markup, html};
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppError, AppResult, AppState, auth::Auth, ui};

fn issue_numbers(key: &str, title: &str, body: Option<&str>) -> Vec<i64> {
    let mut numbers = std::collections::BTreeSet::new();
    for text in [title, body.unwrap_or_default()] {
        for token in text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
            let Some((prefix, digits)) = token.rsplit_once('-') else {
                continue;
            };
            if prefix.eq_ignore_ascii_case(key)
                && !digits.starts_with('0')
                && digits.bytes().all(|b| b.is_ascii_digit())
                && let Ok(number) = digits.parse::<i64>()
            {
                numbers.insert(number);
            }
        }
    }
    numbers.into_iter().collect()
}

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/integrations/github/references", post(lookup))
}

pub(super) fn form(auth: &Auth) -> Markup {
    html! {
        section.panel {
            h2 { "Find an issue from a pull request" }
            p.muted { "Paste a pull request from a connected repository to find issue keys in its title or description." }
            form method="post" action="/integrations/github/references" {
                (ui::csrf(auth))
                label.field { "Pull request URL" input required type="url" name="url" maxlength="2048" placeholder="https://github.com/owner/repository/pull/42"; }
                button type="submit" { "Find matching issues" }
            }
        }
    }
}

#[derive(Deserialize)]
struct LookupForm {
    csrf: String,
    url: String,
}

#[derive(sqlx::FromRow)]
struct IssueMatch {
    id: Uuid,
    number: i64,
    title: String,
}

async fn lookup(
    State(state): State<AppState>,
    auth: Auth,
    Form(input): Form<LookupForm>,
) -> AppResult<Html<String>> {
    auth.check_csrf(&input.csrf)?;
    let url = super::cards::parse_pull_url(&input.url)?;
    let (binding, pull) = super::cards::resolve_pull(&state, auth.user_id, None, &url).await?;
    let mut tx = state.db.begin().await?;
    let (project, key): (String, String) = sqlx::query_as(
        "SELECT p.name,p.key FROM github_repos r
         JOIN projects p ON (p.owner_id,p.id)=(r.owner_id,r.project_id)
         JOIN github_connections c ON c.owner_id=r.owner_id
         WHERE r.owner_id=$1 AND r.repo_id=$2 AND r.generation=$3 AND r.project_id=$4
           AND NOT r.billing_paused AND NOT p.archived AND (c.expires_at IS NULL OR c.expires_at>now())
         FOR SHARE OF r,p,c",
    )
    .bind(auth.user_id)
    .bind(binding.repo_id)
    .bind(binding.generation)
    .bind(binding.project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let numbers = issue_numbers(&key, &pull.title, pull.body.as_deref());
    let matches: Vec<IssueMatch> = sqlx::query_as(
        "SELECT id,number,title FROM issues WHERE owner_id=$1 AND project_id=$2
         AND NOT archived AND number=ANY($3) ORDER BY number",
    )
    .bind(auth.user_id)
    .bind(binding.project_id)
    .bind(numbers)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let canonical = format!("https://github.com/{}/pull/{}", url.repo, url.number);
    Ok(ui::page(
        &auth,
        "Match a pull request",
        "integrations",
        html! {
            h1 { "Match a pull request" }
            p { a href=(&canonical) rel="noreferrer" { (&url.repo) " #" (pull.number) } " · " (&pull.title) }
            @if matches.is_empty() {
                p.notice { "No active issue in " (&project) " matches a key in this pull request. Open an issue to link the pull request directly." }
                a.button.secondary href=(format!("/issues?project={}", binding.project_id)) { "Browse project issues" }
            } @else {
                @if matches.len() == 1 { p { "Confirm the issue to link in " (&project) "." } }
                @else { p { "This pull request mentions multiple issues in " (&project) ". Choose the issue to link." } }
                ul.issue-list {
                    @for issue in &matches {
                        li.row {
                            a href=(format!("/issues/{}",issue.id)) { (&key) "-" (issue.number) " · " (&issue.title) }
                            form method="post" action=(format!("/issues/{}/github/pulls",issue.id)) {
                                (ui::csrf(&auth))
                                input type="hidden" name="url" value=(&canonical);
                                button type="submit" aria-label=(format!("Link pull request to {key}-{}",issue.number)) { "Link pull request" }
                            }
                        }
                    }
                }
            }
            p { a href="/integrations" { "Return to integrations" } }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::issue_numbers;

    #[test]
    fn matches_the_entire_tracker_key_and_collects_distinct_issues() {
        let title = "Fix (LUMA-2), plus luma-42";
        let body = "LUMA-2, OTHER-2, XLUMA-3, LUMA-4x, LUMA-5-preview, βLUMA-6, _LUMA-7, LUMA-8_";
        assert_eq!(issue_numbers("LUMA", title, Some(body)), vec![2, 42]);
    }

    #[test]
    fn matches_only_canonical_positive_numbers_including_body_only_matches() {
        let body = "LUMA-00 LUMA-012 LUMA-0 LUMA--1 LUMA-9223372036854775808 [LUMA-12]";
        assert_eq!(issue_numbers("LUMA", "", Some(body)), vec![12]);
        assert!(issue_numbers("LUMA", "release prep", None).is_empty());
    }
}
