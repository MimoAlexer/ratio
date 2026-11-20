use anyhow::{Context, ensure};
use ratio_server::{AppState, Config, MIGRATIONS};
use sqlx::postgres::PgPoolOptions;
use std::{
    env,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tracing_subscriber::EnvFilter;
use url::Url;

fn loopback(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    )
}
fn config(port: u16) -> anyhow::Result<(Config, IpAddr)> {
    let mode = env::var("APP_ENV").unwrap_or_else(|_| "development".into());
    ensure!(
        matches!(mode.as_str(), "development" | "production" | "test"),
        "APP_ENV must be development, test or production"
    );
    let production = mode == "production";
    let base = env::var("APP_BASE_URL").or_else(|_| {
        if production {
            Err(anyhow::anyhow!("APP_BASE_URL is required in production"))
        } else {
            Ok(format!("http://127.0.0.1:{port}"))
        }
    })?;
    let u = Url::parse(&base).context("invalid APP_BASE_URL")?;
    ensure!(
        matches!(u.scheme(), "http" | "https")
            && u.has_host()
            && u.username().is_empty()
            && u.password().is_none()
            && u.path() == "/"
            && u.query().is_none()
            && u.fragment().is_none(),
        "APP_BASE_URL must be an HTTP(S) origin without path, credentials, query or fragment"
    );
    ensure!(
        !production || u.scheme() == "https",
        "production requires an HTTPS APP_BASE_URL"
    );
    let bind = env::var("BIND_ADDR")
        .unwrap_or_else(|_| if production { "0.0.0.0" } else { "127.0.0.1" }.into())
        .parse::<IpAddr>()
        .context("invalid BIND_ADDR")?;
    if env::var("DEV_LOGIN_LINKS").ok().as_deref() == Some("1") {
        ensure!(
            !production && bind.is_loopback() && loopback(&u),
            "development sign-in links require local-only configuration"
        );
    }
    Ok((
        Config {
            base_url: u.origin().ascii_serialization(),
            production,
        },
        bind,
    ))
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("register SIGTERM");
        tokio::select! { _=tokio::signal::ctrl_c()=>{},_=sig.recv()=>{} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "ratio_server=info".into()),
        )
        .init();
    let command = env::args().nth(1).unwrap_or_else(|| "serve".into());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        println!(
            "ratio-server [serve | migrate | seed-demo | check]\nConfiguration: DATABASE_URL, APP_BASE_URL, APP_ENV, PORT, BIND_ADDR. See README.md."
        );
        return Ok(());
    }
    ensure!(
        matches!(
            command.as_str(),
            "serve" | "migrate" | "seed-demo" | "check"
        ),
        "unknown command; use --help"
    );
    let db_url = env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    if command == "seed-demo" {
        let url = Url::parse(&db_url)?;
        ensure!(
            env::var("APP_ENV").ok().as_deref() != Some("production")
                && loopback(&url)
                && url.query().is_none(),
            "demo seeding requires a local development database without connection overrides"
        );
    }
    let db = PgPoolOptions::new()
        .max_connections(16)
        .acquire_timeout(Duration::from_secs(8))
        .connect(&db_url)
        .await
        .context("connect to PostgreSQL")?;
    if command == "migrate" {
        if !ratio_server::schema_ready(&db).await {
            MIGRATIONS.run(&db).await?;
        }
        if env::var("APP_GRANT_RUNTIME").ok().as_deref() == Some("1") {
            sqlx::raw_sql(include_str!("../ops/grant-runtime.sql"))
                .execute(&db)
                .await
                .context("grant runtime permissions")?;
        }
        println!("Database migrations applied.");
        return Ok(());
    }
    if command == "check" {
        ensure!(
            ratio_server::schema_ready(&db).await,
            "database schema is not ready"
        );
        println!("Database schema ready.");
        return Ok(());
    }
    let port = env::var("PORT")
        .unwrap_or_else(|_| "3000".into())
        .parse::<u16>()
        .context("invalid PORT")?;
    let (settings, bind) = config(port)?;
    if command == "seed-demo" {
        ensure!(
            !settings.production && loopback(&Url::parse(&db_url)?),
            "demo seeding requires a local development database"
        );
        MIGRATIONS.run(&db).await?;
        ratio_server::sample::seed(&db).await?;
        println!("Local demo is ready. Sign in as demo@workspace.test.");
        return Ok(());
    }
    if settings.production {
        ensure!(
            ratio_server::schema_ready(&db).await,
            "run ratio-server migrate before serving production"
        );
    } else if !ratio_server::schema_ready(&db).await {
        MIGRATIONS.run(&db).await?;
    }
    let state = AppState::new(db, settings)?;
    let github = tokio::spawn(ratio_server::github::run(state.clone()));
    let worker_state = state.clone();
    let jobs = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tick = 0u32;
        loop {
            interval.tick().await;
            if let Err(e) = ratio_server::workflows::run_due_digests(&worker_state.db).await {
                tracing::warn!(error_kind=?std::mem::discriminant(&e),"digest worker failed");
            }
            if let Err(e) = ratio_server::workflows::deliver_digest_emails(&worker_state).await {
                tracing::warn!(error_kind=?std::mem::discriminant(&e),"digest delivery failed");
            }
            if let Err(e) = ratio_server::billing::close_deleted_accounts(&worker_state).await {
                tracing::warn!(error_kind=?std::mem::discriminant(&e),"billing deletion cleanup failed");
            }
            if tick.is_multiple_of(12) {
                if let Err(e) = ratio_server::billing::reconcile(&worker_state).await {
                    tracing::warn!(error_kind=?std::mem::discriminant(&e),"billing reconciliation failed");
                }
                if let Err(e) = ratio_server::transfer::purge_expired(&worker_state.db).await {
                    tracing::warn!(error_kind=?std::mem::discriminant(&e),"attachment trash cleanup failed");
                }
                if sqlx::query("DELETE FROM transfer_previews WHERE expires_at<=now()")
                    .execute(&worker_state.db)
                    .await
                    .is_err()
                {
                    tracing::warn!("expired import preview cleanup failed");
                }
            }
            ratio_server::operations::jobs_tick();
            tick = tick.wrapping_add(1);
        }
    });
    let listener = tokio::net::TcpListener::bind((bind, port)).await?;
    tracing::info!(address=%listener.local_addr()?,"serving workspace");
    axum::serve(
        listener,
        ratio_server::app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    github.abort();
    jobs.abort();
    Ok(())
}
