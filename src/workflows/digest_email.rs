use std::{env, time::Duration};

use chrono::{DateTime, NaiveDate, Utc};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use url::{Host, Url};
use uuid::Uuid;

use crate::{AppResult, AppState};

const MAIL_ENDPOINT: &str = "https://api.resend.com/emails";
const BATCH_SIZE: usize = 32;

struct Transport {
    endpoint: String,
    origin: String,
    sender: String,
    api_key: String,
}

fn public_origin(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let Host::Domain(name) = url.host()? else {
        return None;
    };
    let name = name.trim_end_matches('.');
    if !name.contains('.')
        || [".localhost", ".local", ".internal", ".home.arpa", ".lan"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

fn transport(state: &AppState) -> Option<Transport> {
    let origin = public_origin(&state.config.base_url)?;
    let sender = env::var("MAIL_FROM")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let api_key = env::var("RESEND_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    Some(Transport {
        endpoint: MAIL_ENDPOINT.to_owned(),
        origin,
        sender,
        api_key,
    })
}

pub(super) fn available(state: &AppState) -> bool {
    transport(state).is_some()
}

pub async fn deliver_digest_emails(state: &AppState) -> AppResult<u64> {
    let Some(mail) = transport(state) else {
        return Ok(0);
    };
    deliver(&state.db, &state.http, &mail, Utc::now).await
}

#[derive(FromRow)]
struct Claim {
    owner_id: Uuid,
    week_start: NaiveDate,
    lease_id: Uuid,
}

async fn claim(db: &PgPool, now: DateTime<Utc>) -> AppResult<Option<Claim>> {
    // Commit the first possible send time and lease before any network action.
    // A crashed sender can then retry the same request within Resend's 24-hour
    // idempotency window, even if the crash happened after provider acceptance.
    Ok(sqlx::query_as(
        "WITH due AS (
             SELECT q.owner_id,q.week_start FROM portfolio_digest_email_outbox q
             JOIN users u ON u.id=q.owner_id
             WHERE u.identity_current AND q.status='pending' AND q.next_attempt_at <= now()
             ORDER BY q.next_attempt_at,q.owner_id,q.week_start LIMIT 1 FOR UPDATE OF q SKIP LOCKED
         )
         UPDATE portfolio_digest_email_outbox q
            SET attempts=q.attempts+1, lease_id=gen_random_uuid(),
                first_attempt_at=coalesce(q.first_attempt_at,$1),
                next_attempt_at=now()+interval '90 seconds'
           FROM due WHERE (q.owner_id,q.week_start)=(due.owner_id,due.week_start)
           RETURNING q.owner_id,q.week_start,q.lease_id",
    )
    .bind(now)
    .fetch_optional(db)
    .await?)
}

enum Outcome {
    Sent,
    Retry(u32),
    Failed,
    Canceled,
    PayloadMismatch,
}

#[derive(Deserialize)]
struct Acceptance {
    id: Uuid,
}

fn provider_key(owner: Uuid, week: NaiveDate) -> String {
    // The backup may predate this snapshot's random public UUID. The provider
    // key still identifies the same logical mail after restoring that backup.
    let mut hash = Sha256::new();
    hash.update(owner.as_bytes());
    hash.update(week.to_string().as_bytes());
    format!("portfolio-digest/{}", hex::encode(hash.finalize()))
}

async fn send(
    client: &Client,
    mail: &Transport,
    recipient: &str,
    digest: Uuid,
    key: &str,
    retry: bool,
) -> Outcome {
    let submit = |subject| send_with_subject(client, mail, recipient, digest, key, subject);
    let result = submit("Your weekly Ratio review").await;
    if retry && matches!(result, Outcome::PayloadMismatch) {
        // An older release may already have sent this message before recording
        // its receipt. Replay the original payload with the same idempotency key.
        submit("Your weekly Working Set review").await
    } else {
        result
    }
}

async fn send_with_subject(
    client: &Client,
    mail: &Transport,
    recipient: &str,
    digest: Uuid,
    key: &str,
    subject: &str,
) -> Outcome {
    let result = client
        .post(&mail.endpoint)
        .timeout(Duration::from_secs(5))
        .bearer_auth(&mail.api_key)
        .header("Idempotency-Key", key)
        .json(&serde_json::json!({
            "from": mail.sender,
            "to": [recipient],
            "subject": subject,
            "text": format!("{}/workflows/digest/{digest}", mail.origin)
        }))
        .send()
        .await;
    let Ok(response) = result else {
        return Outcome::Retry(0);
    };
    let status = response.status();
    if status.is_success() {
        return match response.json::<Acceptance>().await {
            Ok(accepted) if !accepted.id.is_nil() => Outcome::Sent,
            _ => Outcome::Retry(0),
        };
    }
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    if status == StatusCode::CONFLICT {
        // Resend distinguishes an active duplicate request from an idempotency
        // key that was reused with different request parameters.
        let error = response.json::<serde_json::Value>().await.ok();
        if error.as_ref().and_then(|e| e["name"].as_str()) == Some("invalid_idempotent_request") {
            return Outcome::PayloadMismatch;
        }
        return Outcome::Retry(retry_after);
    }
    if status.is_server_error() || matches!(status.as_u16(), 408 | 425 | 429) {
        Outcome::Retry(retry_after)
    } else {
        Outcome::Failed
    }
}

#[derive(FromRow)]
struct Delivery {
    id: Uuid,
    opted_in: bool,
    first_attempt_at: DateTime<Utc>,
    generated_at: DateTime<Utc>,
    due_at: DateTime<Utc>,
    attempts: i32,
}

impl Delivery {
    fn fresh(&self, now: DateTime<Utc>) -> bool {
        let oldest = now - chrono::Duration::hours(23);
        self.due_at <= now
            && self.due_at > oldest
            && self.generated_at > oldest
            && self.first_attempt_at > oldest
    }
}

async fn deliver(
    db: &PgPool,
    client: &Client,
    mail: &Transport,
    clock: impl Fn() -> DateTime<Utc>,
) -> AppResult<u64> {
    sqlx::query(
        "UPDATE portfolio_digest_email_outbox q SET status='canceled',lease_id=NULL
         FROM users u WHERE q.owner_id=u.id AND NOT u.identity_current AND q.status='pending'",
    )
    .execute(db)
    .await?;
    let mut delivered = 0;
    for _ in 0..BATCH_SIZE {
        let Some(job) = claim(db, clock()).await? else {
            break;
        };
        let mut tx = db.begin().await?;
        // Hold the account lock across the time-bounded mail submission.
        // End-of-plan changes, recipient replacement and opt-out use this lock
        // too, so they cannot commit between the consent check and submission.
        let user: Option<(String, String, bool)> =
            sqlx::query_as("SELECT email,plan,identity_current FROM users WHERE id=$1 FOR UPDATE")
                .bind(job.owner_id)
                .fetch_optional(&mut *tx)
                .await?;
        let item: Option<Delivery> = sqlx::query_as(
            "SELECT d.id, coalesce(s.enabled AND s.email_enabled,false) AS opted_in,
                    q.first_attempt_at, d.generated_at,
                    (d.week_start + 7)::timestamp AT TIME ZONE d.timezone AS due_at, q.attempts
               FROM portfolio_digest_email_outbox q
               JOIN portfolio_digests d USING (owner_id,week_start)
               LEFT JOIN portfolio_digest_schedules s ON s.owner_id=q.owner_id
              WHERE q.owner_id=$1 AND q.week_start=$2 AND q.lease_id=$3
                AND q.status='pending' FOR UPDATE OF q",
        )
        .bind(job.owner_id)
        .bind(job.week_start)
        .bind(job.lease_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let (Some((email, plan, identity_current)), Some(item)) = (user, item) {
            let result = if !identity_current || plan != "max" || !item.opted_in {
                Outcome::Canceled
            } else if !item.fresh(clock()) {
                Outcome::Failed
            } else {
                send(
                    client,
                    mail,
                    &email,
                    item.id,
                    &provider_key(job.owner_id, job.week_start),
                    item.attempts > 1,
                )
                .await
            };
            let (status, delay) = match result {
                Outcome::Sent => ("sent", 0),
                Outcome::Canceled => ("canceled", 0),
                Outcome::Failed | Outcome::PayloadMismatch => ("failed", 0),
                Outcome::Retry(minimum) => {
                    let delay = (30_u32 * 2_u32.pow(item.attempts.clamp(0, 7) as u32))
                        .min(3600)
                        .max(minimum)
                        .min(24 * 3600);
                    ("pending", delay)
                }
            };
            sqlx::query(
                "UPDATE portfolio_digest_email_outbox SET status=$4, lease_id=NULL,
                     next_attempt_at=now()+make_interval(secs=>$5::double precision),
                     sent_at=CASE WHEN $4='sent' THEN now() ELSE NULL END
                 WHERE owner_id=$1 AND week_start=$2 AND lease_id=$3",
            )
            .bind(job.owner_id)
            .bind(job.week_start)
            .bind(job.lease_id)
            .bind(status)
            .bind(f64::from(delay))
            .execute(&mut *tx)
            .await?;
            if status == "sent" {
                delivered += 1;
            }
            if status == "failed" {
                tracing::warn!("Weekly digest email could not be delivered");
            }
        }
        tx.commit().await?;
    }
    Ok(delivered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    #[test]
    fn only_public_https_origins_form_email_links() {
        for origin in [
            "http://workspace.example.org",
            "https://127.0.0.1",
            "https://[::1]",
            "https://localhost",
            "https://host.localhost",
            "https://router.internal",
            "https://user:secret@workspace.example.org",
            "https://workspace.example.org/?redirect=bad",
            "https://workspace.example.org/path",
        ] {
            assert!(public_origin(origin).is_none(), "{origin}");
        }
        assert_eq!(
            public_origin("https://workspace.example.org/").as_deref(),
            Some("https://workspace.example.org")
        );
    }

    fn delivery_monday() -> DateTime<Utc> {
        "2026-11-16T12:00:00Z".parse().unwrap()
    }

    async fn enqueue(db: &PgPool) -> (Uuid, Uuid, String) {
        let owner = Uuid::new_v4();
        let address = format!("{owner}@example.invalid");
        sqlx::query("INSERT INTO users(id,email,plan) VALUES ($1,$2,'max')")
            .bind(owner)
            .bind(&address)
            .execute(db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO portfolio_digest_schedules(owner_id,enabled,email_enabled,next_week)
             VALUES ($1,true,true,'2026-11-16')",
        )
        .bind(owner)
        .execute(db)
        .await
        .unwrap();
        let digest = sqlx::query_scalar(
            "INSERT INTO portfolio_digests(owner_id,week_start,new_issues,issues_done,milestone_targets,releases_published,generated_at)
             VALUES ($1,'2026-11-09',2,7,6,4,'2026-11-16 00:00+00') RETURNING id"
        ).bind(owner).fetch_one(db).await.unwrap();
        sqlx::query(
            "INSERT INTO portfolio_digest_email_outbox(owner_id,week_start) VALUES ($1,'2026-11-09')"
        ).bind(owner).execute(db).await.unwrap();
        (owner, digest, address)
    }

    async fn status(db: &PgPool, owner: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM portfolio_digest_email_outbox WHERE owner_id=$1")
            .bind(owner)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn retry_sends_only_private_link_with_stable_provider_idempotency(db: PgPool) {
        let (owner, digest, address) = enqueue(&db).await;
        let calls = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
        let register = calls.clone();
        let app = Router::new().route(
            "/emails",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let calls = register.clone();
                async move {
                    let mut log = calls.lock().unwrap();
                    log.push((headers, body));
                    if log.len() == 1 {
                        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})))
                    } else {
                        (StatusCode::OK, Json(json!({"id":Uuid::new_v4()})))
                    }
                }
            }),
        );
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let mail = Transport {
            endpoint: format!("http://{addr}/emails"),
            origin: "https://workspace.example.org".into(),
            sender: "fixture@example.invalid".into(),
            api_key: "local-fixture-key".into(),
        };
        let client = Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            deliver(&db, &client, &mail, delivery_monday).await.unwrap(),
            0
        );
        assert_eq!(status(&db, owner).await, "pending");
        assert_eq!(
            deliver(&db, &client, &mail, delivery_monday).await.unwrap(),
            0
        );
        sqlx::query(
            "UPDATE portfolio_digest_email_outbox SET next_attempt_at=now() WHERE owner_id=$1",
        )
        .bind(owner)
        .execute(&db)
        .await
        .unwrap();
        let (a, b) = tokio::join!(
            deliver(&db, &client, &mail, delivery_monday),
            deliver(&db, &client, &mail, delivery_monday)
        );
        assert_eq!(a.unwrap() + b.unwrap(), 1);
        assert_eq!(status(&db, owner).await, "sent");
        assert_eq!(
            deliver(&db, &client, &mail, delivery_monday).await.unwrap(),
            0
        );
        let requests = calls.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (headers, body) in requests.iter() {
            assert_eq!(
                headers["Idempotency-Key"].to_str().unwrap(),
                provider_key(owner, "2026-11-09".parse().unwrap())
            );
            assert_eq!(headers["Authorization"], "Bearer local-fixture-key");
            assert_eq!(
                body,
                &json!({
                    "from":"fixture@example.invalid","to":[address],"subject":"Your weekly Ratio review",
                    "text":format!("https://workspace.example.org/workflows/digest/{digest}")
                })
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn existing_digest_retry_reuses_the_pre_upgrade_request() {
        let subjects = Arc::new(Mutex::new(Vec::new()));
        let observed = subjects.clone();
        let app = Router::new().route(
            "/emails",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let observed = observed.clone();
                async move {
                    assert_eq!(headers["Idempotency-Key"], "fixture-key");
                    let subject = body["subject"].as_str().unwrap();
                    observed.lock().unwrap().push(subject.to_owned());
                    if subject == "Your weekly Working Set review" {
                        (StatusCode::OK, Json(json!({"id":Uuid::new_v4()})))
                    } else {
                        (
                            StatusCode::CONFLICT,
                            Json(json!({"name":"invalid_idempotent_request"})),
                        )
                    }
                }
            }),
        );
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let mail = Transport {
            endpoint: format!("http://{addr}/emails"),
            origin: "https://workspace.example.org".into(),
            sender: "fixture@example.invalid".into(),
            api_key: "fixture".into(),
        };
        let client = Client::builder().no_proxy().build().unwrap();
        let digest = Uuid::new_v4();
        for retry in [false, true] {
            let result = send(
                &client,
                &mail,
                "reader@example.invalid",
                digest,
                "fixture-key",
                retry,
            )
            .await;
            assert!(matches!(
                (retry, result),
                (false, Outcome::PayloadMismatch) | (true, Outcome::Sent)
            ));
        }
        assert_eq!(
            *subjects.lock().unwrap(),
            [
                "Your weekly Ratio review",
                "Your weekly Ratio review",
                "Your weekly Working Set review"
            ]
        );
        server.abort();
    }

    #[sqlx::test]
    async fn revoked_opt_in_downgrade_and_expired_retries_never_contact_provider(db: PgPool) {
        let (optout, _, _) = enqueue(&db).await;
        let (downgrade, _, _) = enqueue(&db).await;
        let (changed, _, _) = enqueue(&db).await;
        let (old, _, _) = enqueue(&db).await;
        let (unverified, _, _) = enqueue(&db).await;
        let (stale_snapshot, _, _) = enqueue(&db).await;
        let (expired_local_day, _, _) = enqueue(&db).await;
        sqlx::query("UPDATE portfolio_digest_schedules SET email_enabled=false WHERE owner_id=$1")
            .bind(optout)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET plan='free' WHERE id=$1")
            .bind(downgrade)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET email='changed@example.invalid' WHERE id=$1")
            .bind(changed)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET identity_current=false WHERE id=$1")
            .bind(unverified)
            .execute(&db)
            .await
            .unwrap();
        for id in [optout, downgrade, changed, unverified] {
            assert_eq!(status(&db, id).await, "canceled");
        }
        sqlx::query("UPDATE portfolio_digest_email_outbox SET first_attempt_at='2026-11-15 10:00+00' WHERE owner_id=$1")
            .bind(old).execute(&db).await.unwrap();
        sqlx::query(
            "UPDATE portfolio_digests SET generated_at='2026-11-14 00:00+00' WHERE owner_id=$1",
        )
        .bind(stale_snapshot)
        .execute(&db)
        .await
        .unwrap();
        // Monday 00:00 in Kiritimati was 26 hours ago at the fixture clock.
        sqlx::query("UPDATE portfolio_digests SET timezone='Pacific/Kiritimati' WHERE owner_id=$1")
            .bind(expired_local_day)
            .execute(&db)
            .await
            .unwrap();
        let mail = Transport {
            endpoint: "http://127.0.0.1:1/unreachable".into(),
            origin: "https://workspace.example.org".into(),
            sender: "fixture@example.invalid".into(),
            api_key: "fixture".into(),
        };
        assert_eq!(
            deliver(&db, &Client::new(), &mail, delivery_monday)
                .await
                .unwrap(),
            0
        );
        // A request against the closed fixture port would instead reschedule a network retry.
        for id in [old, stale_snapshot, expired_local_day] {
            assert_eq!(status(&db, id).await, "failed");
        }
        assert_eq!(status(&db, unverified).await, "canceled");
    }
}
