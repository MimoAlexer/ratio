use axum::{extract::State, response::Html};
use chrono::{DateTime, Months, Utc};
use maud::{Markup, html};
use sqlx::FromRow;
use uuid::Uuid;

use super::provider::Stripe;
use crate::{AppResult, AppState, auth::Auth, store, ui};

#[derive(FromRow)]
struct Account {
    plan: String,
    customer: bool,
    subscription: bool,
    provider_status: String,
    paid_until: Option<DateTime<Utc>>,
    grace_until: Option<DateTime<Utc>>,
    scheduled_plan: Option<String>,
    plan_changes_at: Option<DateTime<Utc>>,
    pending_plan: Option<String>,
}

#[derive(FromRow)]
struct Resource {
    id: Uuid,
    name: String,
    project_key: String,
    inactive: bool,
    project_archived: bool,
    preferred: bool,
}

pub(super) async fn show(State(state): State<AppState>, auth: Auth) -> AppResult<Html<String>> {
    let owner = auth.user_id;
    let account = sqlx::query_as::<_, Account>(
        "SELECT u.plan, a.customer_id IS NOT NULL AS customer,
                (u.plan <> 'free' OR COALESCE(a.provider_status NOT IN
                   ('free', 'canceled', 'incomplete_expired'), false)) AS subscription,
                COALESCE(a.provider_status, 'free') AS provider_status,
                a.paid_until, a.grace_until, a.scheduled_plan, a.plan_changes_at,
                a.pending_plan
         FROM users u LEFT JOIN billing_accounts a ON a.owner_id=u.id WHERE u.id=$1",
    )
    .bind(owner)
    .fetch_one(&state.db)
    .await?;
    let projects = sqlx::query_as::<_, Resource>(
        "SELECT p.id, p.name, p.key AS project_key, p.archived AS inactive, false AS project_archived,
                EXISTS (SELECT 1 FROM billing_preferences b WHERE b.owner_id=p.owner_id
                        AND b.resource_type='project' AND b.resource_id=p.id) AS preferred
         FROM projects p WHERE p.owner_id=$1 ORDER BY p.archived, p.name, p.id",
    )
    .bind(owner)
    .fetch_all(&state.db)
    .await?;
    let repositories = load_repositories(&state.db, owner, crate::github::enabled(&state)).await?;
    let usage = store::storage_used(&state.db, owner).await?;
    let provider = Stripe::from_env().ok();
    let active_projects = projects.iter().filter(|p| !p.inactive).count();
    let active_repos = repositories.iter().filter(|r| !r.inactive).count();
    let limits = store::limits(&account.plan);

    let body = html! {
        header.page-header {
            h1 { "Billing" }
        }
        section aria-labelledby="current-plan" {
            h2 id="current-plan" { (plan_name(&account.plan)) " plan" }
            (account_status(&account))
            @if let Some(provider) = &provider {
                @if !provider.live_mode() {
                    p.notice { "Stripe test mode is enabled. Checkout uses test payments." }
                }
                @if account.customer {
                    form method="post" action="/billing/portal" {
                        (ui::csrf(&auth))
                        button type="submit" { "Manage billing in Stripe" }
                    }
                    p.muted { "View receipts, update your payment method or review the prorated charge and renewal date for a plan change. "
                        "Stripe keeps a separate receipt email. Review it in the portal after changing your sign-in email." }
                }
            } @else {
                p.notice { "Payments are unavailable for this workspace. Your current plan still applies." }
            }
        }
        section aria-labelledby="workspace-usage" {
            h2 id="workspace-usage" { "Workspace usage" }
            dl.grid {
                div { dt { "Active projects" } dd { (active_projects) " / " (limits.projects) } }
                div { dt { "Connected repositories" } dd { (active_repos) " / " (limits.repos) } }
                div { dt { "Stored content" } dd { (bytes(usage)) " / " (bytes(limits.storage_bytes)) } }
            }
            p { (storage_remaining(usage, limits.storage_bytes)) ". Archived content counts toward storage." }
            @let archived = projects.len() - active_projects;
            @let paused = repositories.len() - active_repos;
            @if archived > 0 {
                p {
                    a href="/projects" { (archived) @if archived == 1 { " archived project" } @else { " archived projects" } }
                    " · Read-only and included in exports. You can reactivate a project when your plan has room."
                }
            }
            @if paused > 0 {
                p {
                    a href="/integrations" { (paused) @if paused == 1 { " paused repository" } @else { " paused repositories" } }
                    " · Manage repository sync in Integrations as your plan allows."
                }
            }
            a href="/transfer" { "Export workspace data" }
        }
        section aria-labelledby="compare-plans" {
            h2 id="compare-plans" { "Compare plans" }
            p.muted { "Monthly prices in USD for one workspace. Every plan includes native history exports." }
            div.grid {
                @for (tier, price) in [("free", 0), ("plus", 5), ("max", 15)] {
                    (plan_card(&auth, &account, tier, price, provider.is_some()))
                }
            }
            @if provider.is_some() && !account.subscription {
                @if let Some(renewal) = Utc::now().date_naive().checked_add_months(Months::new(1)) {
                    p.muted {
                        "Starting today puts your next monthly renewal on " (renewal.format("%b %-d, %Y"))
                        ". Review the total, including any applicable tax, before confirming in Stripe."
                    }
                }
            }
        }
        @if active_projects > 1 || repositories.iter().filter(|r| !r.inactive && !r.project_archived).count() > 1 {
            section aria-labelledby="downgrade-preferences" {
                h2 id="downgrade-preferences" { "Downgrade preferences" }
                p {
                    "Choose which projects and repositories to keep active when a smaller plan takes effect. "
                    "Extra projects become read-only and extra repository syncs pause."
                }
                form method="post" action="/billing/selection" {
                    (ui::csrf(&auth))
                    (resource_choices("Projects", "project", &projects))
                    (resource_choices("Repositories", "repo", &repositories))
                    details {
                        summary { "How selections are applied" }
                        p {
                            "Selected projects come first, followed by projects with a selected repository. "
                            "Project ties go to the oldest project, and repository ties go to the lowest GitHub repository ID. "
                            "Only repositories in an active project can sync."
                        }
                    }
                    button type="submit" { "Save preferences" }
                }
            }
        }
    };
    Ok(ui::page(&auth, "Billing", "billing", body))
}

async fn load_repositories(
    db: &sqlx::PgPool,
    owner: Uuid,
    github_enabled: bool,
) -> AppResult<Vec<Resource>> {
    Ok(sqlx::query_as::<_, Resource>(
        "SELECT r.id,
           COALESCE(CASE WHEN $2::boolean AND r.repo_grant_until > now() AND EXISTS
               (SELECT 1 FROM github_connections c WHERE c.owner_id=r.owner_id
                AND (c.expires_at IS NULL OR c.expires_at > now()))
                THEN NULLIF(r.display_name,'') END, concat('Repository ', left(r.id::text,8))) AS name,
           p.key AS project_key, r.billing_paused AS inactive, p.archived AS project_archived,
           EXISTS (SELECT 1 FROM billing_preferences b WHERE b.owner_id=r.owner_id
                   AND b.resource_type='repo' AND b.resource_id=r.id) AS preferred
         FROM github_repos r JOIN projects p ON p.owner_id=r.owner_id AND p.id=r.project_id
         WHERE r.owner_id=$1 ORDER BY p.key, name, r.id",
    )
    .bind(owner).bind(github_enabled)
    .fetch_all(db)
    .await?)
}

fn plan_name(plan: &str) -> &'static str {
    match plan {
        "plus" => "Plus",
        "max" => "Max",
        _ => "Free",
    }
}

fn when(date: DateTime<Utc>) -> String {
    date.format("%b %-d, %Y at %H:%M UTC").to_string()
}

fn account_status(account: &Account) -> Markup {
    html! {
        @if let Some(until) = account.grace_until {
            @if account.plan != "free" {
                p.notice {
                    "Payment overdue. Update your payment method by " (when(until)) ". "
                    @if let Some(paid) = account.paid_until.filter(|paid| *paid > until) {
                        "Your " (plan_name(&account.plan)) " access is paid through " (when(paid)) "."
                    } @else {
                        "After that deadline, your workspace moves to Free."
                    }
                }
            } @else {
                p.notice { "Paid access has ended. Your workspace data is available on Free." }
            }
        } @else if let (Some(plan), Some(date)) = (&account.scheduled_plan, account.plan_changes_at) {
            p { "Scheduled change to " strong { (plan_name(plan)) } " on " (when(date)) "." }
        } @else if account.plan != "free" {
            @if let Some(until) = account.paid_until {
                p { "Paid through " (when(until)) "." }
            }
        } @else if matches!(account.provider_status.as_str(), "past_due" | "unpaid" | "incomplete") {
            p.notice { "Payment needs attention. Paid access starts after Stripe confirms payment." }
        } @else if account.subscription {
            p { "Stripe reports a subscription. Your workspace currently has Free access. Review the subscription in the billing portal." }
        } @else if account.pending_plan.is_none() {
            p { "Your workspace has no recurring charge." }
        }
        @if let Some(plan) = &account.pending_plan {
            p { "Checkout for " (plan_name(plan)) " is pending. Access updates after payment confirmation." }
        }
    }
}

#[derive(Debug, PartialEq)]
enum PlanAction {
    Current,
    Unavailable,
    Start,
    Resume,
    PendingOther,
    Manage,
}

fn plan_action(account: &Account, tier: &str, available: bool) -> PlanAction {
    if account.plan == tier {
        PlanAction::Current
    } else if !available {
        PlanAction::Unavailable
    } else if account.subscription {
        PlanAction::Manage
    } else if let Some(pending) = &account.pending_plan {
        if pending == tier {
            PlanAction::Resume
        } else {
            PlanAction::PendingOther
        }
    } else if tier == "free" {
        PlanAction::Manage
    } else {
        PlanAction::Start
    }
}

fn plan_card(auth: &Auth, account: &Account, tier: &str, price: u8, available: bool) -> Markup {
    let limits = store::limits(tier);
    let action = plan_action(account, tier, available);
    html! {
        article.panel {
            h3 { (plan_name(tier)) }
            p.price { strong { "$" (price) } " / month" }
            ul {
                li { (limits.projects) @if limits.projects == 1 { " active project" } @else { " active projects" }
                     ", " (limits.repos) @if limits.repos == 1 { " repository" } @else { " repositories" } }
                li { (bytes(limits.storage_bytes)) " storage · " (bytes(limits.image_bytes)) " per image" }
                @match tier {
                    "max" => {
                        li { "All recorded release history" }
                        li { "Cross-project saved views and personal templates" }
                        li { "Scheduled private portfolio digest" }
                    }
                    "plus" => {
                        li { "365 days of release history" }
                        li { "Personal saved views and templates" }
                    }
                    _ => {
                        li { "30 days of release history" }
                        li { "Built-in views" }
                    }
                }
            }
            @match action {
                PlanAction::Current => p { strong { "Current plan" } },
                PlanAction::Start | PlanAction::Resume => {
                    form method="post" action="/billing/checkout" {
                        (ui::csrf(auth))
                        input type="hidden" name="plan" value=(tier);
                        button type="submit" {
                            @if action == PlanAction::Resume { "Resume checkout for " } @else { "Choose " } (plan_name(tier))
                        }
                    }
                }
                PlanAction::Manage => p.muted { "Manage changes in the billing portal." },
                PlanAction::PendingOther => p.muted { "Another plan has a pending checkout." },
                PlanAction::Unavailable => {}
            }
        }
    }
}

fn resource_choices(title: &str, kind: &str, resources: &[Resource]) -> Markup {
    html! {
        @if resources.iter().any(|r| !r.inactive && !r.project_archived) {
            fieldset { legend { (title) }
                div.stack { @for r in resources.iter().filter(|r| !r.inactive && !r.project_archived) {
                    label.choice {
                        input type="checkbox" name=(format!("{kind}_{}", r.id)) value="on" checked[r.preferred];
                        (r.project_key) " · " (r.name)
                    }
                } }
            }
        }
    }
}

fn bytes(size: i64) -> String {
    for (unit, name) in [(1_000_000_000, "GB"), (1_000_000, "MB"), (1_000, "kB")] {
        if size >= unit {
            let formatted = format!("{:.1}", size as f64 / unit as f64);
            return format!("{} {name}", formatted.trim_end_matches(".0"));
        }
    }
    format!("{size} B")
}

fn storage_remaining(used: i64, limit: i64) -> String {
    match used.cmp(&limit) {
        std::cmp::Ordering::Less => format!("{} available", bytes(limit - used)),
        std::cmp::Ordering::Equal => "At the storage limit".into(),
        std::cmp::Ordering::Greater => format!("{} over the limit", bytes(used - limit)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn free() -> Account {
        Account {
            plan: "free".into(),
            customer: false,
            subscription: false,
            provider_status: "free".into(),
            paid_until: None,
            grace_until: None,
            scheduled_plan: None,
            plan_changes_at: None,
            pending_plan: None,
        }
    }

    #[sqlx::test]
    async fn repository_labels_require_enabled_integration_and_fresh_grants(db: sqlx::PgPool) {
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users(email) VALUES('billing-visibility@example.invalid') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let project: Uuid = sqlx::query_scalar(
            "INSERT INTO projects(owner_id,key,name) VALUES($1,'A','Project') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO github_connections(owner_id,user_remote_id,access_cipher) VALUES($1,1,'fixture')")
            .bind(owner).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO github_repos(owner_id,repo_id,installation_id,project_id,display_name,repo_grant_until) VALUES($1,1,1,$2,'private/repository',now()+interval '5 minutes')")
            .bind(owner).bind(project).execute(&db).await.unwrap();
        assert_eq!(
            load_repositories(&db, owner, true).await.unwrap()[0].name,
            "private/repository"
        );
        let obscured = load_repositories(&db, owner, false).await.unwrap()[0]
            .name
            .clone();
        assert!(obscured.starts_with("Repository "));
        assert!(
            load_repositories(&db, Uuid::new_v4(), true)
                .await
                .unwrap()
                .is_empty()
        );
        sqlx::query(
            "UPDATE github_connections SET expires_at=now()-interval '1 second' WHERE owner_id=$1",
        )
        .bind(owner)
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(
            load_repositories(&db, owner, true).await.unwrap()[0].name,
            obscured
        );
    }

    #[test]
    fn checkout_actions_require_a_provider_and_existing_subscriptions_use_one_portal() {
        let mut account = free();
        assert_eq!(
            plan_action(&account, "plus", false),
            PlanAction::Unavailable
        );
        assert_eq!(plan_action(&account, "free", false), PlanAction::Current);
        assert_eq!(plan_action(&account, "plus", true), PlanAction::Start);
        account.pending_plan = Some("plus".into());
        assert_eq!(plan_action(&account, "plus", true), PlanAction::Resume);
        assert_eq!(plan_action(&account, "max", true), PlanAction::PendingOther);
        account.plan = "plus".into();
        account.subscription = true;
        account.customer = true;
        assert_eq!(plan_action(&account, "max", true), PlanAction::Manage);
    }

    #[test]
    fn grace_and_scheduled_changes_show_the_deadline_and_resulting_plan() {
        let deadline = Utc.with_ymd_and_hms(2027, 1, 4, 16, 0, 0).unwrap();
        let mut account = free();
        account.plan = "max".into();
        account.provider_status = "past_due".into();
        account.grace_until = Some(deadline);
        let message = account_status(&account).into_string();
        assert!(message.contains("Jan 4, 2027"));
        assert!(message.contains("16:00 UTC"));
        assert!(message.contains("Free"));
        account.grace_until = None;
        account.scheduled_plan = Some("plus".into());
        account.plan_changes_at = Some(deadline);
        let message = account_status(&account).into_string();
        assert!(message.contains("Plus"));
        assert!(message.contains("Jan 4, 2027"));
    }

    #[test]
    fn active_subscription_without_a_paid_invoice_reports_free_access_and_portal_review() {
        let mut account = free();
        account.subscription = true;
        for status in ["active", "trialing", "paused"] {
            account.provider_status = status.into();
            let message = account_status(&account).into_string();
            assert!(message.contains("Free access"));
            assert!(message.contains("billing portal"));
            assert!(!message.contains("no recurring charge"));
        }
    }

    #[test]
    fn failed_payment_does_not_shorten_the_displayed_prepaid_term() {
        let mut account = free();
        account.plan = "plus".into();
        account.grace_until = Some(Utc.with_ymd_and_hms(2027, 1, 4, 16, 0, 0).unwrap());
        account.paid_until = Some(Utc.with_ymd_and_hms(2027, 1, 10, 16, 0, 0).unwrap());
        let message = account_status(&account).into_string();
        assert!(message.contains("Jan 4, 2027"));
        assert!(message.contains("Jan 10, 2027"));
        assert!(!message.contains("moves to Free"));
    }

    #[test]
    fn archived_content_and_overage_have_distinct_storage_readouts() {
        assert_eq!(storage_remaining(120, 100), "20 B over the limit");
        assert_eq!(storage_remaining(100, 100), "At the storage limit");
        assert_eq!(storage_remaining(80, 100), "20 B available");
    }
}
