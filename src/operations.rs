//! Process-local HTTP and worker telemetry. Scrapes never select workspace content.
use std::{
    fmt::Write as _,
    sync::{
        Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use subtle::ConstantTimeEq;

use crate::AppState;

const BUCKET_MS: [u64; 10] = [5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000];

#[derive(Clone, Copy, Default)]
struct HttpStats {
    count: u64,
    errors: u64,
    elapsed: Duration,
    buckets: [u64; BUCKET_MS.len()],
}

static HTTP: std::sync::LazyLock<Mutex<HttpStats>> =
    std::sync::LazyLock::new(|| Mutex::new(HttpStats::default()));
static JOBS_LAST_TICK: AtomicI64 = AtomicI64::new(0);

impl HttpStats {
    fn record(&mut self, status: StatusCode, elapsed: Duration) {
        self.count += 1;
        self.errors += u64::from(status.is_server_error());
        self.elapsed += elapsed;
        if let Some(index) = BUCKET_MS
            .iter()
            .position(|&ms| elapsed <= Duration::from_millis(ms))
        {
            self.buckets[index] += 1;
        }
    }

    fn render(&self, out: &mut String) -> std::fmt::Result {
        writeln!(
            out,
            "# HELP workspace_http_requests_total Completed HTTP responses."
        )?;
        writeln!(out, "# TYPE workspace_http_requests_total counter")?;
        writeln!(out, "workspace_http_requests_total {}", self.count)?;
        writeln!(
            out,
            "# HELP workspace_http_5xx_total Completed HTTP responses with a server error."
        )?;
        writeln!(out, "# TYPE workspace_http_5xx_total counter")?;
        writeln!(out, "workspace_http_5xx_total {}", self.errors)?;
        writeln!(
            out,
            "# HELP workspace_http_request_duration_seconds Time to produce response headers."
        )?;
        writeln!(
            out,
            "# TYPE workspace_http_request_duration_seconds histogram"
        )?;
        let mut cumulative = 0;
        for (limit, count) in BUCKET_MS.iter().zip(self.buckets) {
            cumulative += count;
            writeln!(
                out,
                "workspace_http_request_duration_seconds_bucket{{le=\"{}\"}} {cumulative}",
                *limit as f64 / 1000.0
            )?;
        }
        writeln!(
            out,
            "workspace_http_request_duration_seconds_bucket{{le=\"+Inf\"}} {}",
            self.count
        )?;
        writeln!(
            out,
            "workspace_http_request_duration_seconds_sum {}",
            self.elapsed.as_secs_f64()
        )?;
        writeln!(
            out,
            "workspace_http_request_duration_seconds_count {}",
            self.count
        )
    }
}

pub async fn observe(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let mut response = next.run(req).await;
    let elapsed = start.elapsed();
    let status = response.status();
    HTTP.lock()
        .unwrap_or_else(|e| e.into_inner())
        .record(status, elapsed);
    let request_id = uuid::Uuid::new_v4().to_string();
    tracing::debug!(%request_id, status=status.as_u16(), elapsed_us=elapsed.as_micros() as u64, "HTTP response");
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id).expect("UUID header"),
    );
    response
}

/// Record completion of one periodic worker cycle (including handled job errors).
pub fn jobs_tick() {
    JOBS_LAST_TICK.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
}

fn token_hash(value: &str) -> Option<[u8; 32]> {
    (!value.is_empty() && value.bytes().all(|c| c.is_ascii_graphic()))
        .then(|| Sha256::digest(value).into())
}

fn authorized(headers: &HeaderMap, expected: &[u8; 32]) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some((scheme, token)) = values
        .next()
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.split_once(' '))
    else {
        return false;
    };
    values.next().is_none()
        && scheme.eq_ignore_ascii_case("Bearer")
        && token_hash(token).is_some_and(|submitted| bool::from(expected.ct_eq(&submitted)))
}

pub fn routes() -> Router<AppState> {
    let expected = std::env::var("OPS_METRICS_TOKEN")
        .ok()
        .and_then(|value| token_hash(&value));
    metrics_routes(expected)
}

fn metrics_routes(expected: Option<[u8; 32]>) -> Router<AppState> {
    let Some(expected) = expected else {
        return Router::new();
    };
    Router::new().route(
        "/ops/metrics",
        get(
            move |state: State<AppState>, headers: HeaderMap| async move {
                if !authorized(&headers, &expected) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        [(header::WWW_AUTHENTICATE, "Bearer")],
                    )
                        .into_response();
                }
                let body = scrape(&state.db).await;
                (
                    [
                        (
                            header::CONTENT_TYPE,
                            "text/plain; version=0.0.4; charset=utf-8",
                        ),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    body,
                )
                    .into_response()
            },
        ),
    )
}

// All three source timestamps have an index. A leased or paused task isn't waiting for a worker.
async fn queue_delays(db: &PgPool) -> Result<(f64, f64, f64), sqlx::Error> {
    sqlx::query_as(
        "SELECT
           COALESCE(EXTRACT(EPOCH FROM now() - (
             SELECT r.next_sync_at FROM github_repos r
             JOIN projects p ON p.id=r.project_id AND p.owner_id=r.owner_id
             WHERE NOT r.billing_paused AND NOT p.archived AND r.next_sync_at <= now()
               AND (r.lease_until IS NULL OR r.lease_until <= now())
             ORDER BY r.next_sync_at LIMIT 1)), 0)::float8,
           COALESCE(EXTRACT(EPOCH FROM now() - (
             SELECT min(next_attempt_at) FROM billing_shutdowns WHERE next_attempt_at <= now())), 0)::float8,
           COALESCE(EXTRACT(EPOCH FROM now() - (
             SELECT min(next_attempt_at) FROM portfolio_digest_email_outbox
             WHERE status='pending' AND next_attempt_at <= now())), 0)::float8"
    ).fetch_one(db).await
}

async fn scrape(db: &PgPool) -> String {
    let snapshot = *HTTP.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = String::with_capacity(2400);
    snapshot.render(&mut out).expect("writing to String");
    let tick = JOBS_LAST_TICK.load(Ordering::Relaxed);
    writeln!(out, "# HELP workspace_jobs_last_tick_timestamp_seconds Last completed periodic worker cycle, or zero before the first cycle.").unwrap();
    writeln!(out, "# TYPE workspace_jobs_last_tick_timestamp_seconds gauge\nworkspace_jobs_last_tick_timestamp_seconds {tick}").unwrap();
    writeln!(
        out,
        "# TYPE workspace_db_pool_connections gauge\nworkspace_db_pool_connections {}",
        db.size()
    )
    .unwrap();
    writeln!(
        out,
        "# TYPE workspace_db_pool_idle_connections gauge\nworkspace_db_pool_idle_connections {}",
        db.num_idle()
    )
    .unwrap();
    let delays = tokio::time::timeout(Duration::from_secs(1), queue_delays(db)).await;
    writeln!(out, "# TYPE workspace_queue_scrape_success gauge").unwrap();
    if let Ok(Ok((github, billing, digest))) = delays {
        writeln!(out, "workspace_queue_scrape_success 1").unwrap();
        writeln!(out, "# HELP workspace_queue_oldest_due_seconds Seconds since the oldest currently due task was scheduled, zero for an empty queue.").unwrap();
        writeln!(out, "# TYPE workspace_queue_oldest_due_seconds gauge").unwrap();
        for (queue, age) in [
            ("github_sweep", github),
            ("billing_shutdown", billing),
            ("digest_email", digest),
        ] {
            writeln!(
                out,
                "workspace_queue_oldest_due_seconds{{queue=\"{queue}\"}} {age}"
            )
            .unwrap();
        }
    } else {
        writeln!(out, "workspace_queue_scrape_success 0").unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn response_ids_are_generated_independently_of_caller_input() {
        let app = Router::new()
            .fallback(|| async { StatusCode::BAD_GATEWAY })
            .layer(axum::middleware::from_fn(observe));
        let mut ids = Vec::new();
        for _ in 0..2 {
            let request = Request::get("/private-title?token=private-query")
                .header("x-request-id", "private-input")
                .body(Body::empty())
                .unwrap();
            let res = app.clone().oneshot(request).await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
            ids.push(
                uuid::Uuid::parse_str(res.headers()["x-request-id"].to_str().unwrap()).unwrap(),
            );
        }
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn histogram_preserves_boundaries_and_counts_overflow_and_server_errors() {
        let mut stats = HttpStats::default();
        stats.record(StatusCode::OK, Duration::from_millis(5));
        stats.record(StatusCode::NOT_FOUND, Duration::from_micros(5001));
        stats.record(StatusCode::BAD_GATEWAY, Duration::from_secs(6));
        let mut text = String::new();
        stats.render(&mut text).unwrap();
        for line in [
            "workspace_http_requests_total 3",
            "workspace_http_5xx_total 1",
            "workspace_http_request_duration_seconds_bucket{le=\"0.005\"} 1",
            "workspace_http_request_duration_seconds_bucket{le=\"0.01\"} 2",
            "workspace_http_request_duration_seconds_bucket{le=\"5\"} 2",
            "workspace_http_request_duration_seconds_bucket{le=\"+Inf\"} 3",
            "workspace_http_request_duration_seconds_count 3",
        ] {
            assert!(text.lines().any(|v| v == line), "{line}");
        }
    }

    #[tokio::test]
    async fn metrics_require_a_configured_bearer_and_survive_a_closed_pool() {
        let db = PgPool::connect_lazy("postgres://localhost/workspace").unwrap();
        db.close().await;
        let state = AppState::for_test(db);
        let uri = "/ops/metrics";
        let get = || Request::get(uri).body(Body::empty()).unwrap();
        let disabled = metrics_routes(None).with_state(state.clone());
        assert_eq!(
            disabled.oneshot(get()).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        let app = metrics_routes(token_hash("only-for-tests")).with_state(state);
        for values in [
            vec![],
            vec!["Basic only-for-tests"],
            vec!["Bearer wrong"],
            vec!["Bearer only-for-tests", "Bearer wrong"],
        ] {
            let mut req = get();
            for value in values {
                req.headers_mut()
                    .append(header::AUTHORIZATION, HeaderValue::from_static(value));
            }
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let mut req = get();
        req.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer only-for-tests"),
        );
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = axum::body::to_bytes(res.into_body(), 8192).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("workspace_queue_scrape_success 0\n"));
        assert!(
            !body.contains("workspace_queue_oldest_due_seconds")
                && !body.contains("only-for-tests")
        );
    }

    #[sqlx::test]
    async fn queue_age_uses_due_times_and_ignores_future_retries(db: PgPool) {
        assert_eq!(queue_delays(&db).await.unwrap(), (0.0, 0.0, 0.0));
        sqlx::query(
            "INSERT INTO billing_shutdowns(owner_id,customer_id,provider_live,next_attempt_at)
            VALUES (gen_random_uuid(),'a',false,now()-interval '5 minutes'),
                   (gen_random_uuid(),'b',false,now()+interval '1 hour')",
        )
        .execute(&db)
        .await
        .unwrap();
        let (github, billing, digest) = queue_delays(&db).await.unwrap();
        assert_eq!((github, digest), (0.0, 0.0));
        assert!((300.0..360.0).contains(&billing));
    }
}
