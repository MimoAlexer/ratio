use crate::{AppError, AppResult};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::{Client, Method, RequestBuilder};
use serde_json::Value;
use sha2::Sha256;
use std::time::Duration;
use uuid::Uuid;

pub(super) struct Stripe {
    api: String,
    key: String,
    webhook_secret: String,
    plus: String,
    max: String,
    live: bool,
}

pub(super) struct Event {
    pub id: String,
    pub kind: String,
    pub customer: Option<String>,
    pub subscription: Option<String>,
}

pub(super) struct Subscription {
    pub id: String,
    pub customer: String,
    pub status: String,
    pub owner: Uuid,
    pub plan: Option<&'static str>,
    pub paid_plan: Option<&'static str>,
    pub paid_through: Option<DateTime<Utc>>,
    pub past_due_since: Option<DateTime<Utc>>,
    pub cancel_at_period_end: bool,
    pub cancel_at: Option<DateTime<Utc>>,
    pub scheduled_change: Option<(&'static str, DateTime<Utc>)>,
    pub current_period_end: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CheckoutStatus {
    Open,
    Complete(Option<String>),
    Expired,
}

pub(super) struct Checkout {
    pub id: String,
    pub url: String,
    pub expires_at: DateTime<Utc>,
}

// Stripe's API version is pinned because invoice lines and subscription periods changed in Basil.
const API_VERSION: &str = "2024-06-20";

fn unavailable() -> AppError {
    AppError::Unavailable("Billing is temporarily unavailable. Please try again later.".into())
}

fn configuration_error() -> AppError {
    AppError::Unavailable("Hosted billing needs a Stripe key, webhook secret, and two distinct monthly price IDs. Live billing also requires explicit enablement.".into())
}

fn bad_event() -> AppError {
    AppError::BadRequest("Invalid Stripe event.".into())
}

fn identifier(value: &str, prefix: &str) -> bool {
    value.len() <= 255
        && value.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

fn object_id(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| value.get("id")?.as_str())
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(value.as_i64()?, 0)
}

fn cursor(page: &Value, previous: Option<&str>) -> AppResult<Option<String>> {
    match page.get("has_more").and_then(Value::as_bool) {
        Some(false) => Ok(None),
        Some(true) => {
            let last = page["data"]
                .as_array()
                .and_then(|rows| rows.last())
                .and_then(object_id)
                .filter(|id| Some(*id) != previous)
                .ok_or_else(unavailable)?;
            Ok(Some(last.to_owned()))
        }
        None => Err(unavailable()),
    }
}

#[derive(Clone, Copy)]
enum CleanupResource {
    Session,
    Subscription,
}

impl CleanupResource {
    fn path(self) -> &'static str {
        match self {
            Self::Session => "/checkout/sessions",
            Self::Subscription => "/subscriptions",
        }
    }

    fn id(self, object: &Value) -> AppResult<&str> {
        let prefix = match self {
            Self::Session => "cs_",
            Self::Subscription => "sub_",
        };
        object_id(object)
            .filter(|id| identifier(id, prefix))
            .ok_or_else(unavailable)
    }

    fn owned(self, object: &Value, customer: &str, owner: &str) -> bool {
        if object_id(&object["customer"]) != Some(customer) {
            return false;
        }
        let metadata = object["metadata"]["workspace_id"].as_str();
        match self {
            Self::Subscription => metadata == Some(owner),
            Self::Session => {
                let reference = object["client_reference_id"].as_str();
                object["mode"] == "subscription"
                    && metadata.or(reference) == Some(owner)
                    && reference.is_none_or(|value| value == owner)
            }
        }
    }

    fn stopped(self, object: &Value) -> AppResult<bool> {
        match (self, object["status"].as_str()) {
            (Self::Session, Some("complete" | "expired"))
            | (Self::Subscription, Some("canceled" | "incomplete_expired")) => Ok(true),
            (Self::Session, Some("open"))
            | (
                Self::Subscription,
                Some("active" | "trialing" | "past_due" | "unpaid" | "paused" | "incomplete"),
            ) => Ok(false),
            _ => Err(unavailable()),
        }
    }
}

#[derive(Clone, Copy)]
struct PaidPeriod {
    plan: &'static str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

impl Stripe {
    #[cfg(test)]
    pub(super) fn test_endpoint(api: String) -> Self {
        Self {
            api,
            key: "sk_test_fixture".into(),
            webhook_secret: "whsec_fixture".into(),
            plus: "price_plus".into(),
            max: "price_max".into(),
            live: false,
        }
    }

    pub(super) fn from_env() -> AppResult<Self> {
        let read = |key| {
            std::env::var(key)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(configuration_error)
        };
        let key: String = read("STRIPE_SECRET_KEY")?;
        let live = key.starts_with("sk_live_") || key.starts_with("rk_live_");
        let test = key.starts_with("sk_test_") || key.starts_with("rk_test_");
        if !(test || live && std::env::var("STRIPE_ENABLE_LIVE").as_deref() == Ok("true")) {
            return Err(configuration_error());
        }
        let plus = read("STRIPE_PRICE_PLUS")?;
        let max = read("STRIPE_PRICE_MAX")?;
        let webhook_secret = read("STRIPE_WEBHOOK_SECRET")?;
        if !identifier(&plus, "price_")
            || !identifier(&max, "price_")
            || plus == max
            || !webhook_secret.starts_with("whsec_")
            || webhook_secret.len() <= 6
        {
            return Err(configuration_error());
        }
        Ok(Self {
            api: "https://api.stripe.com/v1".into(),
            key,
            webhook_secret,
            plus,
            max,
            live,
        })
    }

    pub(super) fn live_mode(&self) -> bool {
        self.live
    }

    fn request(&self, http: &Client, method: Method, path: &str) -> RequestBuilder {
        http.request(method, format!("{}{}", self.api, path))
            .basic_auth(&self.key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .timeout(Duration::from_secs(12))
    }

    async fn fetch(&self, request: RequestBuilder) -> AppResult<Value> {
        let response = request.send().await.map_err(|_| unavailable())?;
        if !response.status().is_success() {
            return Err(unavailable());
        }
        response.json().await.map_err(|_| unavailable())
    }

    fn correct_mode(&self, obj: &Value) -> bool {
        obj.get("livemode").and_then(Value::as_bool) == Some(self.live)
    }

    fn plan_id(&self, id: &str) -> Option<&'static str> {
        if id == self.plus {
            Some("plus")
        } else if id == self.max {
            Some("max")
        } else {
            None
        }
    }

    fn price_plan(&self, price: &Value) -> Option<&'static str> {
        let plan = self.plan_id(price["id"].as_str()?)?;
        let cents = if plan == "plus" { 500 } else { 1500 };
        (self.correct_mode(price)
            && price["unit_amount"].as_i64() == Some(cents)
            && price["currency"] == "usd"
            && price["type"] == "recurring"
            && price["billing_scheme"] == "per_unit"
            && price["recurring"]["interval"] == "month"
            && price["recurring"]["interval_count"].as_u64() == Some(1)
            && price["recurring"]["usage_type"] == "licensed"
            && price["transform_quantity"].is_null())
        .then_some(plan)
    }

    pub(super) async fn create_customer(
        &self,
        http: &Client,
        owner: Uuid,
        email: &str,
        key: &str,
    ) -> AppResult<String> {
        let owner = owner.to_string();
        let request = self
            .request(http, Method::POST, "/customers")
            .header("Idempotency-Key", key)
            .form(&[("email", email), ("metadata[workspace_id]", &owner)]);
        let obj = self.fetch(request).await?;
        if !self.correct_mode(&obj) || obj["metadata"]["workspace_id"] != owner {
            return Err(unavailable());
        }
        obj["id"]
            .as_str()
            .filter(|id| identifier(id, "cus_"))
            .map(str::to_owned)
            .ok_or_else(unavailable)
    }

    pub(super) async fn checkout(
        &self,
        http: &Client,
        owner: Uuid,
        customer: &str,
        plan: &str,
        base_url: &str,
        key: &str,
    ) -> AppResult<Checkout> {
        if !identifier(customer, "cus_") {
            return Err(unavailable());
        }
        let price_id = match plan {
            "plus" => &self.plus,
            "max" => &self.max,
            _ => return Err(AppError::BadRequest("Choose Plus or Max.".into())),
        };
        let price = self
            .fetch(self.request(http, Method::GET, &format!("/prices/{price_id}")))
            .await?;
        if self.price_plan(&price) != Some(plan) || price["active"] != true {
            return Err(configuration_error());
        }
        let target = format!("{}/billing", base_url.trim_end_matches('/'));
        let owner = owner.to_string();
        let request = self
            .request(http, Method::POST, "/checkout/sessions")
            .header("Idempotency-Key", key)
            .form(&[
                ("mode", "subscription"),
                ("currency", "usd"),
                ("customer", customer),
                ("client_reference_id", &owner),
                ("line_items[0][price]", price_id),
                ("line_items[0][quantity]", "1"),
                ("metadata[workspace_id]", &owner),
                ("subscription_data[metadata][workspace_id]", &owner),
                ("success_url", &target),
                ("cancel_url", &target),
            ]);
        let obj = self.fetch(request).await?;
        if !self.correct_mode(&obj)
            || object_id(&obj["customer"]) != Some(customer)
            || obj["mode"] != "subscription"
        {
            return Err(unavailable());
        }
        Ok(Checkout {
            id: obj["id"]
                .as_str()
                .filter(|id| identifier(id, "cs_"))
                .ok_or_else(unavailable)?
                .to_owned(),
            url: hosted_url(&obj, "checkout.stripe.com")?,
            expires_at: timestamp(&obj["expires_at"]).ok_or_else(unavailable)?,
        })
    }

    pub(super) async fn checkout_status(
        &self,
        http: &Client,
        session: &str,
        customer: &str,
    ) -> AppResult<CheckoutStatus> {
        if !identifier(session, "cs_") || !identifier(customer, "cus_") {
            return Err(unavailable());
        }
        let obj = self
            .fetch(self.request(http, Method::GET, &format!("/checkout/sessions/{session}")))
            .await?;
        if !self.correct_mode(&obj)
            || obj["id"] != session
            || object_id(&obj["customer"]) != Some(customer)
        {
            return Err(unavailable());
        }
        match obj["status"].as_str() {
            Some("open") => Ok(CheckoutStatus::Open),
            Some("expired") => Ok(CheckoutStatus::Expired),
            Some("complete") => Ok(CheckoutStatus::Complete(
                object_id(&obj["subscription"])
                    .filter(|id| identifier(id, "sub_"))
                    .map(str::to_owned),
            )),
            _ => Err(unavailable()),
        }
    }

    /// Stop future billing after a confirmed workspace deletion. Drain open sessions first:
    /// a completion racing session expiration can create another subscription to cancel.
    pub(super) async fn stop_billing(
        &self,
        http: &Client,
        customer: &str,
        owner: Uuid,
    ) -> AppResult<()> {
        if !identifier(customer, "cus_") {
            return Err(unavailable());
        }
        let owner = owner.to_string();
        let mut unresolved = false;
        let sessions = self
            .cleanup_candidates(http, customer, &owner, CleanupResource::Session)
            .await;
        let mut completed_subs = Vec::new();
        match sessions {
            Ok(ids) => {
                for id in ids {
                    match self
                        .stop_resource(http, customer, &owner, CleanupResource::Session, &id)
                        .await
                    {
                        Ok(response) if response["status"] == "complete" => {
                            match object_id(&response["subscription"])
                                .filter(|s| identifier(s, "sub_"))
                            {
                                Some(sub) => completed_subs.push(sub.to_owned()),
                                None => unresolved = true,
                            }
                        }
                        Ok(_) => (),
                        Err(_) => unresolved = true,
                    }
                }
            }
            Err(_) => unresolved = true,
        }
        let subscriptions = self
            .cleanup_candidates(http, customer, &owner, CleanupResource::Subscription)
            .await?;
        for sub in completed_subs {
            if !subscriptions.contains(&sub) {
                // A just-completed Checkout can appear before the subscriptions list catches up.
                let result = self
                    .stop_completed_subscription(http, customer, &owner, &sub)
                    .await;
                unresolved |= result.is_err();
            }
        }
        for sub in subscriptions {
            let result = self
                .stop_resource(http, customer, &owner, CleanupResource::Subscription, &sub)
                .await;
            unresolved |= result.is_err();
        }
        if unresolved {
            Err(unavailable())
        } else {
            Ok(())
        }
    }

    async fn stop_completed_subscription(
        &self,
        http: &Client,
        customer: &str,
        owner: &str,
        sub: &str,
    ) -> AppResult<()> {
        let path = format!("/subscriptions/{sub}");
        let canonical = self.fetch(self.request(http, Method::GET, &path)).await?;
        if !self.correct_mode(&canonical)
            || canonical["id"] != sub
            || !CleanupResource::Subscription.owned(&canonical, customer, owner)
        {
            return Err(unavailable());
        }
        if !CleanupResource::Subscription.stopped(&canonical)? {
            self.stop_resource(http, customer, owner, CleanupResource::Subscription, sub)
                .await?;
        }
        Ok(())
    }

    async fn cleanup_candidates(
        &self,
        http: &Client,
        customer: &str,
        owner: &str,
        resource: CleanupResource,
    ) -> AppResult<Vec<String>> {
        let status = match resource {
            CleanupResource::Session => "open",
            CleanupResource::Subscription => "all",
        };
        let mut after = None;
        let mut ids = Vec::new();
        loop {
            let mut request = self.request(http, Method::GET, resource.path()).query(&[
                ("customer", customer),
                ("status", status),
                ("limit", "100"),
            ]);
            if let Some(ref cursor) = after {
                request = request.query(&[("starting_after", cursor)]);
            }
            let page = self.fetch(request).await?;
            for object in page["data"].as_array().ok_or_else(unavailable)? {
                if !resource.owned(object, customer, owner) {
                    continue;
                }
                if !self.correct_mode(object) {
                    return Err(unavailable());
                }
                if !resource.stopped(object)? {
                    ids.push(resource.id(object)?.to_owned());
                }
            }
            after = cursor(&page, after.as_deref())?;
            if after.is_none() {
                return Ok(ids);
            }
        }
    }

    async fn stop_resource(
        &self,
        http: &Client,
        customer: &str,
        owner: &str,
        resource: CleanupResource,
        id: &str,
    ) -> AppResult<Value> {
        let path = format!("{}/{id}", resource.path());
        let request = match resource {
            CleanupResource::Session => self.request(http, Method::POST, &format!("{path}/expire")),
            CleanupResource::Subscription => self
                .request(http, Method::DELETE, &path)
                .query(&[("invoice_now", "false"), ("prorate", "false")]),
        };
        // An already-expired session, a concurrent cancellation and a lost response can
        // all produce an error after the intended state has already been reached.
        let object = match self.fetch(request).await {
            Ok(value) => value,
            Err(_) => self.fetch(self.request(http, Method::GET, &path)).await?,
        };
        if !self.correct_mode(&object)
            || object["id"] != id
            || !resource.owned(&object, customer, owner)
            || !resource.stopped(&object)?
        {
            return Err(unavailable());
        }
        Ok(object)
    }

    pub(super) async fn portal(
        &self,
        http: &Client,
        customer: &str,
        base_url: &str,
    ) -> AppResult<String> {
        if !identifier(customer, "cus_") {
            return Err(unavailable());
        }
        let target = format!("{}/billing", base_url.trim_end_matches('/'));
        let request = self
            .request(http, Method::POST, "/billing_portal/sessions")
            .form(&[("customer", customer), ("return_url", &target)]);
        let obj = self.fetch(request).await?;
        if self.correct_mode(&obj) && object_id(&obj["customer"]) == Some(customer) {
            hosted_url(&obj, "billing.stripe.com")
        } else {
            Err(unavailable())
        }
    }

    pub(super) fn event(&self, header: &str, body: &[u8], now: DateTime<Utc>) -> AppResult<Event> {
        let parts = header
            .split(',')
            .filter_map(|part| part.trim().split_once('='));
        let mut timestamps = parts
            .clone()
            .filter_map(|(kind, v)| (kind == "t").then_some(v));
        let t = timestamps.next().ok_or_else(bad_event)?;
        let epoch = t.parse::<i64>().map_err(|_| bad_event())?;
        if timestamps.next().is_some() || epoch.abs_diff(now.timestamp()) > 300 {
            return Err(bad_event());
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(self.webhook_secret.as_bytes())
            .map_err(|_| configuration_error())?;
        mac.update(t.as_bytes());
        mac.update(b".");
        mac.update(body);
        if !parts
            .filter_map(|(kind, v)| (kind == "v1").then_some(v))
            .any(|h| hex::decode(h).is_ok_and(|sig| mac.clone().verify_slice(&sig).is_ok()))
        {
            return Err(bad_event());
        }
        let event: Value = serde_json::from_slice(body).map_err(|_| bad_event())?;
        if !self.correct_mode(&event) {
            return Err(bad_event());
        }
        let id = event["id"]
            .as_str()
            .filter(|id| identifier(id, "evt_"))
            .ok_or_else(bad_event)?
            .to_owned();
        let kind = event["type"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() < 200)
            .ok_or_else(bad_event)?
            .to_owned();
        let obj = event
            .pointer("/data/object")
            .filter(|v| v.is_object())
            .ok_or_else(bad_event)?;
        let sub = if kind.starts_with("customer.subscription.") {
            object_id(obj)
        } else {
            object_id(&obj["subscription"])
                .or_else(|| object_id(obj.pointer("/parent/subscription_details/subscription")?))
        };
        Ok(Event {
            id,
            kind,
            customer: object_id(&obj["customer"])
                .filter(|s| identifier(s, "cus_"))
                .map(str::to_owned),
            subscription: sub.filter(|s| identifier(s, "sub_")).map(str::to_owned),
        })
    }

    /// List canonical subscriptions for a previously-bound customer, including canceled subscriptions.
    /// The signed event is only a wakeup; an invoice payload or Checkout return URL cannot grant a plan.
    pub(super) async fn subscriptions(
        &self,
        http: &Client,
        customer: &str,
        owner: Uuid,
    ) -> AppResult<Vec<Subscription>> {
        if !identifier(customer, "cus_") {
            return Err(unavailable());
        }
        let mut result = Vec::new();
        let mut after = None;
        let owner_text = owner.to_string();
        loop {
            let mut request = self.request(http, Method::GET, "/subscriptions").query(&[
                ("customer", customer),
                ("status", "all"),
                ("limit", "100"),
                ("expand[]", "data.latest_invoice"),
                ("expand[]", "data.schedule"),
            ]);
            if let Some(ref id) = after {
                request = request.query(&[("starting_after", id)]);
            }
            let page = self.fetch(request).await?;
            for obj in page["data"].as_array().ok_or_else(unavailable)? {
                if obj["metadata"]["workspace_id"] != owner_text {
                    continue;
                }
                if object_id(&obj["customer"]) != Some(customer) {
                    return Err(unavailable());
                }
                result.push(self.parse_subscription(http, obj).await?);
            }
            after = cursor(&page, after.as_deref())?;
            if after.is_none() {
                break;
            }
        }
        Ok(result)
    }

    async fn parse_subscription(&self, http: &Client, obj: &Value) -> AppResult<Subscription> {
        if !self.correct_mode(obj) {
            return Err(unavailable());
        }
        let id = obj["id"]
            .as_str()
            .filter(|s| identifier(s, "sub_"))
            .ok_or_else(unavailable)?;
        let customer = object_id(&obj["customer"])
            .filter(|s| identifier(s, "cus_"))
            .ok_or_else(unavailable)?;
        let owner = Uuid::parse_str(
            obj["metadata"]["workspace_id"]
                .as_str()
                .ok_or_else(unavailable)?,
        )
        .map_err(|_| unavailable())?;
        let status = obj["status"].as_str().ok_or_else(unavailable)?.to_owned();
        let items = obj["items"]["data"].as_array().ok_or_else(unavailable)?;
        let single = match items.as_slice() {
            [item] if obj["items"]["has_more"] == false && item["quantity"].as_u64() == Some(1) => {
                Some(item)
            }
            _ => None,
        };
        let plan = single.and_then(|item| self.price_plan(&item["price"]));
        let item = single.and_then(object_id).filter(|s| identifier(s, "si_"));
        let latest = &obj["latest_invoice"];
        let mut evidence = None;
        if let Some(item) = item {
            evidence = self.paid_period(http, latest, customer, id, item).await?;
            if evidence.is_none() && !latest.is_null() {
                evidence = self.previous_payment(http, customer, id, item).await?;
            }
        }
        let past_due_since = if matches!(status.as_str(), "past_due" | "unpaid")
            && latest["attempted"] == true
            && object_id(&latest["customer"]) == Some(customer)
            && object_id(&latest["subscription"]) == Some(id)
            && self.correct_mode(latest)
            && matches!(latest["status"].as_str(), Some("open" | "uncollectible"))
        {
            timestamp(&latest["status_transitions"]["finalized_at"])
                .or_else(|| timestamp(&latest["created"]))
        } else {
            None
        };
        let scheduled_change = self.next_plan(&obj["schedule"], plan, customer, id);
        Ok(Subscription {
            id: id.to_owned(),
            customer: customer.to_owned(),
            status,
            owner,
            plan,
            paid_plan: evidence.map(|p| p.plan),
            paid_through: evidence.map(|p| p.end),
            past_due_since,
            cancel_at_period_end: obj["cancel_at_period_end"] == true,
            cancel_at: timestamp(&obj["cancel_at"]),
            scheduled_change,
            current_period_end: timestamp(&obj["current_period_end"]),
        })
    }

    fn next_plan(
        &self,
        schedule: &Value,
        plan: Option<&str>,
        customer: &str,
        subscription: &str,
    ) -> Option<(&'static str, DateTime<Utc>)> {
        if !self.correct_mode(schedule)
            || schedule["status"] != "active"
            || object_id(&schedule["customer"]) != Some(customer)
            || object_id(&schedule["subscription"]) != Some(subscription)
        {
            return None;
        }
        let end = timestamp(&schedule["current_phase"]["end_date"])?;
        let phase = schedule["phases"]
            .as_array()?
            .iter()
            .find(|phase| timestamp(&phase["start_date"]) == Some(end))?;
        let [item] = phase["items"].as_array()?.as_slice() else {
            return None;
        };
        if item["quantity"].as_u64() != Some(1) {
            return None;
        }
        let next = self.plan_id(object_id(&item["price"])?)?;
        (Some(next) != plan).then_some((next, end))
    }

    async fn previous_payment(
        &self,
        http: &Client,
        customer: &str,
        id: &str,
        item: &str,
    ) -> AppResult<Option<PaidPeriod>> {
        let mut after = None;
        loop {
            let mut request = self.request(http, Method::GET, "/invoices").query(&[
                ("customer", customer),
                ("subscription", id),
                ("status", "paid"),
                ("limit", "100"),
            ]);
            if let Some(ref key) = after {
                request = request.query(&[("starting_after", key)]);
            }
            let page = self.fetch(request).await?;
            for invoice in page["data"].as_array().ok_or_else(unavailable)? {
                if let Some(paid) = self.paid_period(http, invoice, customer, id, item).await? {
                    return Ok(Some(paid));
                }
            }
            after = cursor(&page, after.as_deref())?;
            if after.is_none() {
                return Ok(None);
            }
        }
    }

    async fn paid_period(
        &self,
        http: &Client,
        invoice: &Value,
        customer: &str,
        subscription: &str,
        item: &str,
    ) -> AppResult<Option<PaidPeriod>> {
        if invoice["status"] != "paid"
            || invoice["paid"] != true
            || !self.correct_mode(invoice)
            || object_id(&invoice["customer"]) != Some(customer)
            || object_id(&invoice["subscription"]) != Some(subscription)
        {
            return Ok(None);
        }
        let id = invoice["id"]
            .as_str()
            .filter(|s| identifier(s, "in_"))
            .ok_or_else(unavailable)?;
        let mut lines = invoice["lines"].clone();
        let mut candidate: Option<PaidPeriod> = None;
        let mut after = None;
        let now = Utc::now();
        loop {
            for line in lines["data"].as_array().ok_or_else(unavailable)? {
                if let Some(p) = self.line_period(line, subscription, item, now)
                    && candidate.is_none_or(|c| (p.start, p.end) > (c.start, c.end))
                {
                    candidate = Some(p);
                }
            }
            after = cursor(&lines, after.as_deref())?;
            let Some(ref key) = after else { break };
            lines = self
                .fetch(
                    self.request(http, Method::GET, &format!("/invoices/{id}/lines"))
                        .query(&[("limit", "100"), ("starting_after", key.as_str())]),
                )
                .await?;
        }
        Ok(candidate)
    }

    fn line_period(
        &self,
        line: &Value,
        subscription: &str,
        item: &str,
        now: DateTime<Utc>,
    ) -> Option<PaidPeriod> {
        if !(line["type"] == "subscription"
            || line["type"] == "invoiceitem" && line["proration"] == true)
            || object_id(&line["subscription"]) != Some(subscription)
            || object_id(&line["subscription_item"]) != Some(item)
            || line["currency"] != "usd"
            || line["quantity"].as_u64() != Some(1)
            || line["amount"].as_i64()? <= 0
        {
            return None;
        }
        let plan = self.price_plan(&line["price"])?;
        let start = timestamp(&line["period"]["start"])?;
        let end = timestamp(&line["period"]["end"])?;
        (start <= now && end > start).then_some(PaidPeriod { plan, start, end })
    }
}

fn hosted_url(obj: &Value, host: &str) -> AppResult<String> {
    let raw = obj["url"].as_str().ok_or_else(unavailable)?;
    let url = reqwest::Url::parse(raw).map_err(|_| unavailable())?;
    if url.scheme() != "https"
        || url.host_str() != Some(host)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(unavailable());
    }
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::Form,
        http::HeaderMap,
        routing::{get, post},
    };
    use hmac::{Hmac, Mac};
    use serde_json::{Value, json};
    use sha2::Sha256;
    use std::collections::HashMap;

    fn signature(body: &[u8], t: i64) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_fixture").unwrap();
        mac.update(format!("{t}.").as_bytes());
        mac.update(body);
        format!(
            "t={t},v1=badhex,v1={}",
            hex::encode(mac.finalize().into_bytes())
        )
    }

    #[test]
    fn signed_events_use_original_bytes_a_single_fresh_timestamp_and_correct_mode() {
        let stripe = Stripe::test_endpoint(String::new());
        let t = 1720000000;
        let now = DateTime::from_timestamp(t, 0).unwrap();
        let body=br#"{ "id":"evt_123","type":"invoice.paid","livemode":false,"data":{"object":{"customer":"cus_123","parent":{"subscription_details":{"subscription":"sub_123"}}}}}"#;
        let signed = signature(body, t);
        let event = stripe
            .event(&signed, body, now)
            .expect("current signed event");
        assert_eq!(event.id, "evt_123");
        assert_eq!(event.kind, "invoice.paid");
        assert_eq!(event.subscription.as_deref(), Some("sub_123"));
        assert_eq!(event.customer.as_deref(), Some("cus_123"));
        assert!(stripe.event(&signed, &body[1..], now).is_err());
        for ts in [t - 301, t + 301, i64::MIN] {
            assert!(stripe.event(&signature(body, ts), body, now).is_err());
        }
        assert!(stripe.event(&format!("{signed},t={t}"), body, now).is_err());
        let live = String::from_utf8(body.to_vec())
            .unwrap()
            .replace("false", "true");
        assert!(
            stripe
                .event(&signature(live.as_bytes(), t), live.as_bytes(), now)
                .is_err()
        );
    }

    async fn start(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn price(id: &str, amount: i64) -> Value {
        json!({"id":id,"object":"price","active":true,"livemode":false,"currency":"usd","type":"recurring","billing_scheme":"per_unit","unit_amount":amount,"recurring":{"interval":"month","interval_count":1,"usage_type":"licensed"}})
    }

    #[tokio::test]
    async fn unpaid_upgrade_keeps_only_the_plan_in_the_paid_invoice() {
        let owner = Uuid::new_v4();
        let current = 1711000000;
        let previous = 1710000000;
        let sub = json!({"id":"sub_1","object":"subscription","customer":"cus_1","metadata":{"workspace_id":owner},"livemode":false,"status":"past_due","current_period_end":current,"cancel_at_period_end":false,"items":{"has_more":false,"data":[{"id":"si_1","price":price("price_max",1500),"quantity":1}]},"latest_invoice":{"id":"in_failure","customer":"cus_1","subscription":"sub_1","livemode":false,"status":"open","created":previous-3600,"status_transitions":{"finalized_at":previous},"attempted":true}});
        let paid = json!({"id":"in_paid","customer":"cus_1","subscription":"sub_1","livemode":false,"status":"paid","paid":true,"lines":{"has_more":false,"data":[
            {"type":"subscription","subscription":"sub_1","subscription_item":"si_1","quantity":1,"amount":500,"currency":"usd","price":price("price_plus",500),"period":{"start":previous,"end":current}},
            {"type":"invoiceitem","proration":true,"subscription":"sub_1","subscription_item":"si_1","quantity":1,"amount":-15,"price":price("price_max",1500),"period":{"start":previous,"end":current}}
        ]}});
        let app = Router::new()
            .route(
                "/subscriptions",
                get(move || async move { Json(json!({"has_more":false,"data":[sub]})) }),
            )
            .route(
                "/invoices",
                get(move || async move { Json(json!({"has_more":false,"data":[paid]})) }),
            );
        let stripe = Stripe::test_endpoint(start(app).await);
        let all = stripe
            .subscriptions(&reqwest::Client::new(), "cus_1", owner)
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
        let observed = &all[0];
        assert_eq!(observed.plan, Some("max"));
        assert_eq!(observed.paid_plan, Some("plus"));
        assert_eq!(observed.paid_through.unwrap().timestamp(), current);
        assert_eq!(observed.past_due_since.unwrap().timestamp(), previous);
    }

    #[tokio::test]
    async fn checkout_validates_monthly_usd_price_and_sends_only_server_selected_identifiers() {
        let owner = Uuid::new_v4();
        let app=Router::new()
            .route("/prices/price_plus",get(||async {Json(price("price_plus",500))}))
            .route("/prices/price_max",get(||async {Json(price("price_max",1499))}))
            .route("/checkout/sessions",post(move |headers:HeaderMap,Form(form):Form<HashMap<String,String>>|async move {
                assert_eq!(headers["stripe-version"],"2024-06-20");
                assert_eq!(headers["idempotency-key"],"attempt-1");
                assert_eq!(form["mode"],"subscription");
                assert_eq!(form["currency"], "usd");
                assert_eq!(form["customer"],"cus_1");
                assert_eq!(form["line_items[0][price]"],"price_plus");
                assert_eq!(form["line_items[0][quantity]"],"1");
                assert_eq!(form["subscription_data[metadata][workspace_id]"],owner.to_string());
                assert_eq!(form["success_url"],"https://app.example/billing");
                Json(json!({"id":"cs_test_1","url":"https://checkout.stripe.com/c/pay/session","customer":"cus_1","mode":"subscription","livemode":false,"expires_at":1810000000}))
            }));
        let stripe = Stripe::test_endpoint(start(app).await);
        let http = reqwest::Client::new();
        assert!(
            stripe
                .checkout(
                    &http,
                    owner,
                    "cus_1",
                    "price_override",
                    "https://app.example",
                    "attempt-0"
                )
                .await
                .is_err()
        );
        assert!(
            stripe
                .checkout(
                    &http,
                    owner,
                    "cus_1",
                    "max",
                    "https://app.example",
                    "attempt-0"
                )
                .await
                .is_err()
        );
        let checkout = stripe
            .checkout(
                &http,
                owner,
                "cus_1",
                "plus",
                "https://app.example",
                "attempt-1",
            )
            .await
            .expect("configured monthly plan");
        assert_eq!(checkout.id, "cs_test_1");
        assert_eq!(checkout.expires_at.timestamp(), 1810000000);
    }

    #[test]
    fn invoice_evidence_requires_a_settled_usd_line_for_the_same_item_and_current_service() {
        let stripe = Stripe::test_endpoint(String::new());
        let now = DateTime::from_timestamp(1720000000, 0).unwrap();
        let line = json!({"type":"invoiceitem","proration":true,"subscription":"sub_1","subscription_item":"si_1",
            "currency":"usd","amount":90,"quantity":1,"price":price("price_max",1500),"period":{"start":1719000000,"end":1721000000}});
        let selected = stripe
            .line_period(&line, "sub_1", "si_1", now)
            .expect("paid upgrade proration");
        assert_eq!(selected.plan, "max");
        for (field, bad) in [
            ("subscription", json!("sub_other")),
            ("subscription_item", json!("si_other")),
            ("currency", json!("eur")),
            ("quantity", json!(2)),
            ("amount", json!(-90)),
            ("proration", json!(false)),
            ("period", json!({"start":1720000001,"end":1721000000})),
            ("price", price("price_other", 1500)),
        ] {
            let mut altered = line.clone();
            altered[field] = bad;
            assert!(
                stripe.line_period(&altered, "sub_1", "si_1", now).is_none(),
                "{field}"
            );
        }
    }

    #[tokio::test]
    async fn canonical_subscription_reads_queued_phase_and_next_page_of_invoice_lines() {
        let owner = Uuid::new_v4();
        let renewal = Utc::now().timestamp() + 3600;
        let line = json!({"id":"il_valid","type":"subscription","subscription":"sub_1","subscription_item":"si_1","currency":"usd",
         "quantity":1,"amount":1500,"price":price("price_max",1500),"period":{"start":renewal-7200,"end":renewal}});
        let invoice = json!({"id":"in_1","subscription":"sub_1","customer":"cus_1","livemode":false,"currency":"usd","paid":true,"status":"paid",
                    "lines":{"has_more":true,"data":[{"id":"il_credit","amount":-100}]}});
        let schedule = json!({"livemode":false,"customer":"cus_1","subscription":"sub_1","status":"active",
            "current_phase":{"end_date":renewal},"phases":[{"start_date":renewal,"items":[{"price":"price_plus","quantity":1}]}]});
        let sub = json!({"id":"sub_1","customer":"cus_1","metadata":{"workspace_id":owner},"livemode":false,
            "status":"active","current_period_end":renewal,"cancel_at_period_end":false,"schedule":schedule,
            "items":{"has_more":false,"data":[{"id":"si_1","quantity":1,"price":price("price_max",1500)}]},"latest_invoice":invoice});
        let app = Router::new()
            .route(
                "/subscriptions",
                get(move || async move { Json(json!({"has_more":false,"data":[sub]})) }),
            )
            .route(
                "/invoices/in_1/lines",
                get(
                    move |axum::extract::Query(query): axum::extract::Query<
                        HashMap<String, String>,
                    >| async move {
                        assert_eq!(query["starting_after"], "il_credit");
                        Json(json!({"has_more":false,"data":[line]}))
                    },
                ),
            );
        let stripe = Stripe::test_endpoint(start(app).await);
        let subs = stripe
            .subscriptions(&Client::new(), "cus_1", owner)
            .await
            .unwrap();
        let [result] = subs.as_slice() else {
            panic!("expected one subscription")
        };
        assert_eq!(result.paid_plan, Some("max"));
        assert_eq!(result.paid_through.unwrap().timestamp(), renewal);
        let (next, effective) = result.scheduled_change.unwrap();
        assert_eq!((next, effective.timestamp()), ("plus", renewal));
    }

    #[tokio::test]
    async fn portal_status_and_customer_creation_check_bound_customer_and_hosted_origin() {
        let owner = Uuid::new_v4();
        let app=Router::new()
            .route("/customers",post(move|headers:HeaderMap,Form(form):Form<HashMap<String,String>>|async move {
                assert_eq!(headers["idempotency-key"],"create-once");
                assert_eq!(form["email"],"test@example.com");
                assert_eq!(form["metadata[workspace_id]"],owner.to_string());
                Json(json!({"id":"cus_1","livemode":false,"metadata":{"workspace_id":owner}}))
            }))
            .route("/billing_portal/sessions",post(|Form(form):Form<HashMap<String,String>>|async move {
                assert_eq!(form["return_url"],"https://app.example/billing");
                let url=if form["customer"]=="cus_1" {"https://billing.stripe.com/p/session"} else {"https://other.example/session"};
                Json(json!({"id":"bps_1","livemode":false,"customer":form["customer"],"url":url}))
            }))
            .route("/checkout/sessions/cs_1",get(||async{Json(json!({"id":"cs_1","customer":"cus_1","livemode":false,"status":"expired"}))}));
        let stripe = Stripe::test_endpoint(start(app).await);
        let http = Client::new();
        assert_eq!(
            stripe
                .create_customer(&http, owner, "test@example.com", "create-once")
                .await
                .unwrap(),
            "cus_1"
        );
        assert_eq!(
            stripe
                .portal(&http, "cus_1", "https://app.example/")
                .await
                .unwrap(),
            "https://billing.stripe.com/p/session"
        );
        assert!(
            stripe
                .portal(&http, "cus_2", "https://app.example")
                .await
                .is_err()
        );
        assert_eq!(
            stripe
                .checkout_status(&http, "cs_1", "cus_1")
                .await
                .unwrap(),
            CheckoutStatus::Expired
        );
        assert!(
            stripe
                .checkout_status(&http, "cs_1", "cus_2")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deletion_expires_all_owned_sessions_then_cancels_newly_visible_subscriptions() {
        use axum::{
            extract::{Path, Query, State},
            http::StatusCode,
            routing::delete,
        };
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct DeletionFixture {
            owner: Uuid,
            log: Arc<Mutex<Vec<String>>>,
        }
        impl DeletionFixture {
            fn session(&self, id: &str, status: &str, owner: Option<Uuid>) -> Value {
                json!({"id":id,"mode":"subscription","livemode":false,"customer":"cus_1","status":status,"metadata":{"workspace_id":owner},
                        "subscription":if status=="complete"{Some("sub_raced")}else{None}})
            }
            fn subscription(&self, id: &str, status: &str, owner: Uuid) -> Value {
                json!({"id":id,"customer":"cus_1","livemode":false,"status":status,"metadata":{"workspace_id":owner}})
            }
        }

        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let state = DeletionFixture {
            owner,
            log: Default::default(),
        };
        let app=Router::new()
            .route("/checkout/sessions",get(move |State(f):State<DeletionFixture>, Query(q):Query<HashMap<String,String>>|async move{
                assert_eq!(q["customer"],"cus_1");
                assert_eq!(q["status"],"open");
                let mut log=f.log.lock().unwrap();
                if log.iter().any(|s|s=="cancel sub_raced"){
                    return Json(json!({"data":[],"has_more":false}));
                }
                let mut legacy=f.session("cs_2","open",None);
                legacy["client_reference_id"]=json!(f.owner);
                if q.contains_key("starting_after") {
                    assert_eq!(q["starting_after"],"cs_1");
                    log.push("session page 2".into());
                    return Json(json!({"data":[legacy,f.session("cs_9","open",Some(other))],"has_more":false}));
                }
                Json(json!({"data":[f.session("cs_1","open",Some(f.owner))],"has_more":true}))
            }))
            .route("/checkout/sessions/{id}/expire",post(|State(f):State<DeletionFixture>,Path(id):Path<String>|async move{
                assert!(matches!(id.as_str(),"cs_1"|"cs_2"));
                let mut log=f.log.lock().unwrap();
                assert!(log.iter().any(|s|s=="session page 2"));
                log.push(format!("expire {id}"));
                if id=="cs_2" {return (StatusCode::BAD_REQUEST,Json(json!({})));}
                (StatusCode::OK,Json(f.session(&id,"expired",Some(f.owner))))
            }))
            .route("/checkout/sessions/cs_2",get(|State(f):State<DeletionFixture>|async move {
                Json(f.session("cs_2","complete",Some(f.owner)))
            }))
            .route("/subscriptions",get(move|State(f):State<DeletionFixture>,Query(q):Query<HashMap<String,String>>|async move{
                assert_eq!(q["customer"],"cus_1");
                assert_eq!(q["status"],"all");
                let log=f.log.lock().unwrap();
                assert!(log.iter().any(|s|s=="expire cs_2"));
                let status=if log.iter().any(|s|s=="cancel sub_raced") {"canceled"}else{"past_due"};
                Json(json!({"data":[f.subscription("sub_raced",status,f.owner),f.subscription("sub_other","active",other),
                    f.subscription("sub_finished","incomplete_expired",f.owner)],"has_more":false}))
            }))
            .route("/subscriptions/{id}",delete(|State(f):State<DeletionFixture>,Path(id):Path<String>|async move {
                assert_eq!(id,"sub_raced");
                f.log.lock().unwrap().push(format!("cancel {id}"));
                // The provider canceled it, but the response was lost.
                (StatusCode::BAD_GATEWAY,Json(json!({})))
            }).get(|State(f):State<DeletionFixture>,Path(id):Path<String>|async move {
                assert_eq!(id,"sub_raced");
                f.log.lock().unwrap().push(format!("confirm {id}"));
                Json(f.subscription(&id,"canceled",f.owner))
            }))
            .with_state(state.clone());
        let stripe = Stripe::test_endpoint(start(app).await);
        let http = Client::new();
        stripe.stop_billing(&http, "cus_1", owner).await.unwrap();
        stripe.stop_billing(&http, "cus_1", owner).await.unwrap();
        assert_eq!(
            *state.log.lock().unwrap(),
            [
                "session page 2",
                "expire cs_1",
                "expire cs_2",
                "cancel sub_raced",
                "confirm sub_raced"
            ]
        );
    }

    #[tokio::test]
    async fn deletion_remains_retryable_when_cancellation_did_not_take_effect() {
        let owner = Uuid::new_v4();
        let sub = json!({"id":"sub_retry","livemode":false,"customer":"cus_1","metadata":{"workspace_id":owner},"status":"active"});
        let listed = sub.clone();
        let mut independent = sub.clone();
        independent["id"] = json!("sub_independent");
        let mut stopped = independent.clone();
        stopped["status"] = json!("canceled");
        let (sent, received) = tokio::sync::oneshot::channel();
        let signal = std::sync::Arc::new(std::sync::Mutex::new(Some(sent)));
        let app =
            Router::new()
                .route(
                    "/checkout/sessions",
                    get(|| async { Json(json!({"data":[],"has_more":false})) }),
                )
                .route(
                    "/subscriptions",
                    get(move || async move {
                        Json(json!({"data":[listed,independent],"has_more":false}))
                    }),
                )
                .route(
                    "/subscriptions/sub_independent",
                    axum::routing::delete(move || async move {
                        signal.lock().unwrap().take().unwrap().send(()).unwrap();
                        Json(stopped)
                    }),
                )
                .route(
                    "/subscriptions/sub_retry",
                    get(move || async move { Json(sub) })
                        .delete(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
                );
        let stripe = Stripe::test_endpoint(start(app).await);
        assert!(
            stripe
                .stop_billing(&Client::new(), "cus_1", owner)
                .await
                .is_err()
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(300), received).await,
            Ok(Ok(()))
        ));
    }

    #[tokio::test]
    async fn cleanup_accepts_concurrent_expiry_and_an_immediate_confirmed_cancellation() {
        use axum::extract::Query;
        let owner = Uuid::new_v4();
        let session = json!({"id":"cs_older","customer":"cus_1","metadata":{"workspace_id":owner},"mode":"subscription","livemode":false,"status":"open"});
        let mut expired = session.clone();
        expired["status"] = json!("expired");
        let sub = json!({"id":"sub_due","customer":"cus_1","metadata":{"workspace_id":owner},"livemode":false,"status":"unpaid"});
        let mut canceled = sub.clone();
        canceled["status"] = json!("canceled");
        let app = Router::new()
            .route(
                "/checkout/sessions",
                get(move || async move { Json(json!({"data":[session],"has_more":false})) }),
            )
            .route(
                "/checkout/sessions/cs_older/expire",
                post(|| async { axum::http::StatusCode::BAD_REQUEST }),
            )
            .route(
                "/checkout/sessions/cs_older",
                get(move || async move { Json(expired) }),
            )
            .route(
                "/subscriptions",
                get(move || async move { Json(json!({"data":[sub],"has_more":false})) }),
            )
            .route(
                "/subscriptions/sub_due",
                axum::routing::delete(move |Query(q): Query<HashMap<String, String>>| async move {
                    assert_eq!(q["invoice_now"], "false");
                    assert_eq!(q["prorate"], "false");
                    Json(canceled)
                }),
            );
        let stripe = Stripe::test_endpoint(start(app).await);
        stripe
            .stop_billing(&Client::new(), "cus_1", owner)
            .await
            .unwrap();
    }
}
