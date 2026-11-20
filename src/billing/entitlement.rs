#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Plan {
    Free,
    Plus,
    Max,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Access {
    pub plan: Plan,
    pub paid_until: Option<i64>,
    pub grace_until: Option<i64>,
    pub scheduled: Option<(Plan, i64)>,
}
#[derive(Default)]
pub(super) struct Evidence<'a> {
    pub status: &'a str,
    pub listed: Option<Plan>,
    pub payment: Option<(Plan, i64)>,
    pub failed_at: Option<i64>,
    pub cancellation: Option<i64>,
    pub schedule: Option<(Plan, i64)>,
}
impl Plan {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "free" => Some(Self::Free),
            "plus" => Some(Self::Plus),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Plus => "plus",
            Self::Max => "max",
        }
    }
}

pub(super) fn resolve(previous: &Access, e: Evidence<'_>, now: i64) -> Access {
    let prior = previous.paid_until.map(|end| (previous.plan, end));
    let verified = match e.status {
        "active" | "past_due" | "unpaid" | "canceled" | "paused" => e.payment,
        _ => None,
    };
    // Prefer the higher already-paid tier until its own term ends. A cheaper
    // renewal cannot extend the expensive tier's expiration.
    let current = [prior, verified]
        .into_iter()
        .flatten()
        .filter(|(plan, end)| *plan != Plan::Free && *end > now)
        .max();
    let (plan, paid_until) = match current {
        Some((plan, end)) => (plan, Some(end)),
        None => (Plan::Free, previous.paid_until),
    };
    let mut access = Access {
        plan,
        paid_until,
        grace_until: None,
        scheduled: None,
    };

    if let Some(failure) = e
        .failed_at
        .filter(|_| matches!(e.status, "past_due" | "unpaid"))
    {
        let last_paid = [prior, verified]
            .into_iter()
            .flatten()
            .filter(|(plan, end)| *plan != Plan::Free && *end >= failure.saturating_sub(86_400))
            .max();
        if let Some((paid_plan, _)) = last_paid {
            let end = failure.saturating_add(7 * 86_400);
            let end = previous.grace_until.map_or(end, |old| old.min(end));
            access.grace_until = Some(end);
            if end > now && access.plan == Plan::Free {
                access.plan = paid_plan;
            }
        }
    }

    if access.plan != Plan::Free {
        let lower_price = e
            .listed
            .filter(|p| *p < access.plan)
            .zip(access.paid_until.filter(|end| *end > now));
        let canceled = if e.status == "canceled" {
            access.paid_until.map(|end| (Plan::Free, end))
        } else {
            e.cancellation.map(|end| (Plan::Free, end))
        };
        access.scheduled = [canceled, e.schedule, lower_price]
            .into_iter()
            .flatten()
            .filter(|(plan, end)| *plan < access.plan && *end >= now)
            .min_by_key(|(plan, end)| (*end, *plan));
    }
    access
}

#[cfg(test)]
mod tests {
    use super::*;
    fn access(plan: Plan, until: Option<i64>) -> Access {
        Access {
            plan,
            paid_until: until,
            grace_until: None,
            scheduled: None,
        }
    }
    #[test]
    fn requires_confirmed_payment_to_unlock() {
        let free = access(Plan::Free, None);
        let input = Evidence {
            status: "active",
            listed: Some(Plan::Max),
            ..Default::default()
        };
        assert_eq!(resolve(&free, input, 500).plan, Plan::Free);
        let input = Evidence {
            status: "active",
            listed: Some(Plan::Plus),
            payment: Some((Plan::Plus, 900)),
            ..Default::default()
        };
        assert_eq!(resolve(&free, input, 500), access(Plan::Plus, Some(900)));
    }
    #[test]
    fn paid_through_downgrade_does_not_extend_old_price() {
        let max = access(Plan::Max, Some(900));
        let evidence = || Evidence {
            status: "active",
            listed: Some(Plan::Plus),
            payment: Some((Plan::Plus, 1500)),
            ..Default::default()
        };
        let before = resolve(&max, evidence(), 700);
        assert_eq!(before.plan, Plan::Max);
        assert_eq!(before.paid_until, Some(900));
        assert_eq!(before.scheduled, Some((Plan::Plus, 900)));
        assert_eq!(
            resolve(&before, evidence(), 900),
            access(Plan::Plus, Some(1500))
        );
    }
    #[test]
    fn grace_has_a_fixed_deadline_and_recovers_with_payment() {
        let day = 86400;
        let plus = access(Plan::Plus, Some(1000));
        let failed = || Evidence {
            status: "past_due",
            listed: Some(Plan::Max),
            failed_at: Some(1000),
            ..Default::default()
        };
        let grace = resolve(&plus, failed(), 1001);
        assert_eq!(grace.plan, Plan::Plus);
        assert_eq!(grace.grace_until, Some(1000 + 7 * day));
        assert_eq!(resolve(&grace, failed(), 1000 + 7 * day).plan, Plan::Free);
        assert_eq!(
            resolve(&access(Plan::Free, None), failed(), 1001).plan,
            Plan::Free
        );
        let no_extension = resolve(
            &grace,
            Evidence {
                failed_at: Some(1000 + day),
                ..failed()
            },
            1000 + 2 * day,
        );
        assert_eq!(no_extension.grace_until, grace.grace_until);
        let recovery = Evidence {
            status: "active",
            listed: Some(Plan::Max),
            payment: Some((Plan::Max, 1000 + 30 * day)),
            ..Default::default()
        };
        assert_eq!(
            resolve(&grace, recovery, 1002),
            access(Plan::Max, Some(1000 + 30 * day))
        );
    }
    #[test]
    fn canceled_subscription_retains_only_the_paid_period() {
        let plus = access(Plan::Plus, Some(900));
        let evidence = || Evidence {
            status: "canceled",
            listed: Some(Plan::Plus),
            ..Default::default()
        };
        assert_eq!(
            resolve(&plus, evidence(), 700).scheduled,
            Some((Plan::Free, 900))
        );
        assert_eq!(resolve(&plus, evidence(), 900).plan, Plan::Free);
    }
    #[test]
    fn scheduled_future_downgrade_does_not_grant_an_unpaid_upgrade() {
        let plus = access(Plan::Plus, Some(900));
        let update = Evidence {
            status: "active",
            listed: Some(Plan::Max),
            payment: Some((Plan::Plus, 900)),
            ..Default::default()
        };
        assert_eq!(resolve(&plus, update, 700), plus);
        let future = Evidence {
            status: "active",
            listed: Some(Plan::Plus),
            payment: Some((Plan::Plus, 900)),
            schedule: Some((Plan::Free, 900)),
            ..Default::default()
        };
        assert_eq!(
            resolve(&plus, future, 700).scheduled,
            Some((Plan::Free, 900))
        );
    }
}
