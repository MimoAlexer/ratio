//! Run with a local DATABASE_URL and cargo test --test read_load -- --ignored --nocapture.
//! SQLx creates a separate migrated database and removes it after a successful run.
//! Measures complete, authenticated, uncompressed HTTP responses over loopback.
//! Use --release before ' -- ' for an optimized build. No external providers participate.
use anyhow::{Context, Result, ensure};
use reqwest::{Client, StatusCode, header::COOKIE};
use sha2::{Digest, Sha256};
use solo_tracker::{AppState, app};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::Barrier, task::JoinSet};
use tracing_subscriber::{Layer, layer::SubscriberExt};
use uuid::Uuid;

const READERS: usize = 25;
const MEASURED_PER_READER: usize = 8;
const WARM_PER_READER: usize = 2;

struct QueryCounter(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for QueryCounter {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if e.metadata().target() == "sqlx::query" {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn seed(pool: &PgPool) -> Result<Vec<String>> {
    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,plan) VALUES ($1,'reader@example.invalid','max')")
        .bind(owner)
        .execute(pool)
        .await?;
    let mut cookies = Vec::with_capacity(READERS);
    for _ in 0..READERS {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        sqlx::query("INSERT INTO user_sessions(user_id,token_hash) VALUES($1,$2)")
            .bind(owner)
            .bind(Sha256::digest(token.as_bytes()).to_vec())
            .execute(pool)
            .await?;
        cookies.push(format!("workspace_session={token}"));
    }
    sqlx::query(
        "INSERT INTO projects(owner_id,id,key,name)
        SELECT $1,md5('project'||p)::uuid,'P'||p,'Project '||p FROM generate_series(1,5) p",
    )
    .bind(owner)
    .execute(pool)
    .await?;
    sqlx::query(r#"INSERT INTO issues(owner_id,project_id,id,number,title,body,status,kind,priority,labels,
              next_action,checkpoint_at,started_at,completed_at,security_critical,manual_severity,updated_at)
        SELECT $1,md5('project'||p)::uuid,md5('issue'||n)::uuid,((n-1)/5)+1,
               (CASE WHEN n%97=0 THEN 'Inspect marked queue ' ELSE 'Verify component ' END)||n,
               (CASE WHEN n%97=0 THEN 'calibrationprobe ' ELSE '' END) ||
                  repeat('Reproduce the integration result with the batch runner and record the observed changes. ',3),
               status, (ARRAY['feature','maintenance','bug'])[1+n%3],
               (ARRAY['low','normal','high'])[1+n%3], ARRAY['release','component'||n%20],
               CASE WHEN n=5001 THEN 'Confirm saved capacity' ELSE 'Review the previous run' END,
               CASE WHEN n=5001 THEN now() END,
               CASE WHEN status IN ('done','in_progress') THEN now()-interval '5 days' END,
               CASE WHEN status='done' THEN now()-interval '1 day' END,
               n%607=0, CASE WHEN n%607=0 THEN 'critical' ELSE 'none' END,
               now()-(10000-n)*interval '1 minute'
        FROM (SELECT n,1+(n-1)%5 AS p, CASE WHEN n=5001 THEN 'in_progress'
               ELSE (ARRAY['backlog','ready','in_progress','done','canceled'])[1+(n/5)%5] END AS status
              FROM generate_series(1,10000) n) s"#)
        .bind(owner).execute(pool).await?;
    sqlx::query(
        "INSERT INTO issue_events(owner_id,project_id,issue_id,new_status,action,revision_after)
              SELECT owner_id,project_id,id,status,'create',1 FROM issues WHERE owner_id=$1",
    )
    .bind(owner)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO notes(owner_id,project_id,id,title,path)
              SELECT $1,md5('project'||(1+(n-1)%5))::uuid,md5('note'||n)::uuid,
                       'Decision '||n,'decisions/'||n||'.md' FROM generate_series(1,2000) n",
    )
    .bind(owner)
    .execute(pool)
    .await?;
    sqlx::query(r#"INSERT INTO note_links(owner_id, project_id,source_id,ordinal,raw,target,label,kind,dest_note,dest_issue)
        SELECT $1, md5('project'||p)::uuid,md5('note'||n)::uuid,ord,
               '[['||target||']]',target,target,'wiki',
               CASE WHEN ord<10 THEN md5('note'||dest)::uuid END,
               CASE WHEN ord>=10 THEN md5('issue'||dest)::uuid END
        FROM (SELECT *, CASE WHEN ord<10 THEN 'decisions/'||dest||'.md'
                    ELSE 'P'||p||'-'||(1+(dest-1)/5) END AS target
              FROM (SELECT n,ord,p,CASE WHEN ord<10 THEN p+5*((g+ord+1)%400)
                                ELSE p+5*((g*13+ord*37)%2000) END AS dest
                    FROM (SELECT n,ord,1+(n-1)%5 AS p,(n-1)/5 AS g
                          FROM generate_series(1,2000) n CROSS JOIN generate_series(0,24) ord) b) d) s"#)
        .bind(owner).execute(pool).await?;
    sqlx::query(
        "UPDATE notes n SET body='Implementation notes'||chr(10)||r.authored
            FROM (SELECT source_id,string_agg(raw,chr(10) ORDER BY ordinal) authored
                  FROM note_links WHERE owner_id=$1 GROUP BY source_id) r
            WHERE n.id=r.source_id",
    )
    .bind(owner)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO note_issue_sources(owner_id,project_id,note_id,issue_id,source_anchor,excerpt,note_revision)
            SELECT owner_id,project_id,md5('note1')::uuid,id,'decision','Implementation notes',1
            FROM issues WHERE id=md5('issue5001')::uuid").execute(pool).await?;
    let counts: (i64,i64,i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM issues),(SELECT count(*) FROM notes),(SELECT count(*) FROM note_links)")
        .fetch_one(pool).await?;
    ensure!(
        counts == (10_000, 2_000, 50_000),
        "fixture counts: {counts:?}"
    );
    sqlx::query("ANALYZE").execute(pool).await?;
    Ok(cookies)
}

#[derive(Clone)]
struct Reader {
    client: Client,
    cookie: String,
    detail_url: String,
    detail_title: String,
}

#[derive(Clone, Copy)]
enum Page {
    Home,
    List,
    Detail,
    Search,
}

impl Page {
    fn request(self, reader: &Reader, base: &str) -> (String, String) {
        match self {
            Page::Home => (format!("{base}/"), "Confirm saved capacity".into()),
            // The work list begins with active issues, newest update first within that group.
            Page::List => (format!("{base}/issues"), "Verify component 9989".into()),
            Page::Detail => (
                reader.detail_url.clone(),
                format!("<h1>{}</h1>", reader.detail_title),
            ),
            Page::Search => (
                format!("{base}/issues?q=calibrationprobe"),
                "Inspect marked queue".into(),
            ),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::List => "List",
            Self::Detail => "Detail",
            Self::Search => "Search",
        }
    }
}

async fn phase(
    readers: &[Reader],
    base: &str,
    page: Page,
    repeats: usize,
) -> Result<(Vec<Duration>, usize)> {
    let gate = Arc::new(Barrier::new(readers.len()));
    let mut tasks = JoinSet::new();
    for reader in readers {
        let (url, marker) = page.request(reader, base);
        let reader = reader.clone();
        let gate = gate.clone();
        tasks.spawn(async move {
            let mut samples = Vec::with_capacity(repeats);
            let mut errors = 0;
            gate.wait().await;
            for _ in 0..repeats {
                let start = Instant::now();
                let response = reader
                    .client
                    .get(&url)
                    .header(COOKIE, &reader.cookie)
                    .send()
                    .await;
                let valid = match response {
                    Ok(r) if r.status() == StatusCode::OK => {
                        r.text().await.is_ok_and(|body| body.contains(&marker))
                    }
                    _ => false,
                };
                samples.push(start.elapsed());
                if !valid {
                    errors += 1;
                }
            }
            (samples, errors)
        });
    }
    let mut measurements = Vec::with_capacity(readers.len() * repeats);
    let mut errors = 0;
    while let Some(result) = tasks.join_next().await {
        let (mut samples, failed) = result?;
        measurements.append(&mut samples);
        errors += failed;
    }
    measurements.sort_unstable();
    Ok((measurements, errors))
}

async fn benchmark(opts: PgPoolOptions, connect: PgConnectOptions) -> Result<()> {
    let pool = opts
        .max_connections(16)
        .min_connections(16)
        .idle_timeout(None)
        .acquire_timeout(Duration::from_secs(8))
        .connect_with(connect)
        .await?;
    let cookies = seed(&pool).await?;
    let details: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id,title FROM issues WHERE number BETWEEN 101 AND 105 ORDER BY project_id,number",
    )
    .fetch_all(&pool)
    .await?;
    ensure!(
        details.len() == READERS,
        "one distinct detail issue per reader"
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = app(AppState::for_test(pool.clone()));
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    });
    let mut readers = Vec::new();
    for (cookie, (id, title)) in cookies.into_iter().zip(details) {
        readers.push(Reader {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()?,
            cookie,
            detail_url: format!("{base}/issues/{id}"),
            detail_title: title,
        });
    }
    let queries = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(
        QueryCounter(queries.clone()).with_filter(tracing_subscriber::filter::filter_fn(|m| {
            m.target() == "sqlx::query"
        })),
    ))?;
    println!(
        "loopback · one workspace, five projects, 10,000 issues, 2,000 notes, 50,000 note links · 25 independent signed-in sessions · 16 DB connections · four Tokio workers"
    );
    println!("page     samples     p50 ms     p95 ms    SQLx query events/request    errors");
    let mut failures = 0;
    for page in [Page::Home, Page::List, Page::Detail, Page::Search] {
        let (_, warm_errors) = phase(&readers, &base, page, WARM_PER_READER).await?;
        ensure!(
            warm_errors == 0,
            "{} warm responses failed ({warm_errors})",
            page.name()
        );
        queries.store(0, Ordering::Relaxed);
        let (samples, errors) = phase(&readers, &base, page, MEASURED_PER_READER).await?;
        let n = samples.len();
        let percentile = |p: usize| samples[(n * p).div_ceil(100) - 1].as_secs_f64() * 1000.0;
        let per_request = queries.load(Ordering::Relaxed) as f64 / n as f64;
        ensure!(per_request >= 1.0, "missing SQLx query events");
        println!(
            "{:<8} {:>7} {:>10.2} {:>10.2} {:>27.2} {:>9}",
            page.name(),
            n,
            percentile(50),
            percentile(95),
            per_request,
            errors
        );
        failures += errors;
    }
    server.abort();
    let _ = server.await;
    pool.close().await;
    ensure!(
        failures == 0,
        "{failures} HTTP status or response-content failures"
    );
    Ok(())
}

#[sqlx::test]
#[ignore = "opt-in local load measurement with its own migrated PostgreSQL database"]
async fn authenticated_reads(opts: PgPoolOptions, connect: PgConnectOptions) -> Result<()> {
    // SQLx provisions the database on a single-thread runtime. Measure the HTTP server
    // and its readers on a bounded multithread runtime for reproducible concurrency.
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?
            .block_on(benchmark(opts, connect))
    })
    .await
    .context("benchmark runtime")?
}
