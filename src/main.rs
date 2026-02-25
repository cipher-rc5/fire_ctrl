/// file: src/main.rs
/// description: CLI entrypoint, runtime setup, and server/worker startup orchestration.
mod api;
mod config;
mod database;
mod llm;
mod models;
mod scraper;
mod worker;

use anyhow::Result;
use axum::http::{HeaderValue, Method, header};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tower_http::{compression::CompressionLayer, cors::CorsLayer, trace::TraceLayer};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// fire_ctrl — self-hosted Firecrawl v2 runtime in native Rust.
#[derive(Debug, Parser)]
#[command(name = "fire_ctrl", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the HTTP API server only.
    Server,
    /// Start the background worker pool only.
    Worker,
    /// Start both API server and worker pool (default for production).
    All,
    /// Health-check all dependencies and exit.
    Healthcheck,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Structured logging — respect RUST_LOG; default to "info".
    // Set LOG_FORMAT=json (e.g. in production) for machine-readable output.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,fire_ctrl=debug"));

    let json_format = std::env::var("LOG_FORMAT").as_deref() == Ok("json");
    if json_format {
        tracing_subscriber::registry()
            .with(fmt::layer().json())
            .with(filter)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(fmt::layer())
            .with(filter)
            .init();
    }

    let cfg = config::Config::from_env().unwrap_or_else(|e| {
        tracing::warn!("Config load error: {e}");
        panic!("Cannot start without a valid configuration: {e}");
    });

    let cli = Cli::parse();

    match cli.command {
        Command::Server => run_server(cfg).await,
        Command::Worker => run_worker(cfg).await,
        Command::All => {
            info!("Starting API server and worker pool");
            tokio::try_join!(run_server(cfg.clone()), run_worker(cfg))?;
            Ok(())
        }
        Command::Healthcheck => run_healthcheck(cfg).await,
    }
}

// ---------------------------------------------------------------------------
// Sub-command handlers
// ---------------------------------------------------------------------------

async fn run_server(cfg: config::Config) -> Result<()> {
    let addr = cfg.server.addr()?;
    info!(%addr, "API server listening");

    let state = build_state(&cfg).await?;

    let mut cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    if !cfg.server.cors_allow_origins.is_empty() {
        let mut origins = Vec::with_capacity(cfg.server.cors_allow_origins.len());
        for origin in &cfg.server.cors_allow_origins {
            origins.push(HeaderValue::from_str(origin).map_err(|e| {
                anyhow::anyhow!("Invalid CORS_ALLOW_ORIGINS entry `{origin}`: {e}")
            })?);
        }
        cors = cors.allow_origin(origins);
    }

    let app = api::router(state)
        .layer(CompressionLayer::new())
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn run_worker(cfg: config::Config) -> Result<()> {
    info!(
        num_workers = cfg.worker.num_workers,
        max_concurrent = cfg.worker.max_concurrent_jobs,
        "Starting worker pool"
    );

    let pool = database::build_pool(&cfg.database)?;
    let db = database::DbClient::new(pool);

    db.ping().await.map_err(|e| {
        anyhow::anyhow!(
            "Database connectivity check failed: {}\n\
             Hint: verify POSTGRES_* env vars are correct, postgresql@17 is running, \
             and shared_preload_libraries = 'timescaledb' is set in postgresql.conf \
             (run `just install` to configure this automatically).\n\
             Cause chain: {}",
            e,
            error_chain(&e)
        )
    })?;

    let scraper = Arc::new(scraper::Scraper::new(&cfg.crawler, &cfg.proxy)?);
    let llm = Arc::new(llm::LlmClient::new(&cfg.llm)?);

    let result = worker::WorkerPool::new(
        db,
        Arc::clone(&scraper),
        llm,
        cfg.worker,
        cfg.auth.allow_local_webhooks,
    )
    .run()
    .await;

    scraper.shutdown().await;
    result
}

async fn run_healthcheck(cfg: config::Config) -> Result<()> {
    info!("Running health check…");

    let pool = database::build_pool(&cfg.database)?;
    let db = database::DbClient::new(pool);
    match db.ping().await {
        Ok(()) => info!("Database: ok"),
        Err(e) => tracing::warn!("Database: {e}"),
    }

    let redis_primary = ping_redis(&cfg.redis.url).await;
    let redis_rate = ping_redis(&cfg.redis.rate_limit_url).await;
    match (redis_primary, redis_rate) {
        (Ok(()), Ok(())) => info!("Redis: ok (primary + rate-limit)"),
        (a, b) => {
            if let Err(e) = a {
                tracing::warn!("Redis primary: {e}");
            }
            if let Err(e) = b {
                tracing::warn!("Redis rate-limit: {e}");
            }
        }
    }

    match lapin::Connection::connect(&cfg.rabbitmq.url, lapin::ConnectionProperties::default())
        .await
    {
        Ok(conn) => {
            let _ = conn.close(0, "healthcheck".into()).await;
            info!("RabbitMQ: ok");
        }
        Err(e) => tracing::warn!("RabbitMQ: {e}"),
    }

    if cfg.llm.is_configured() {
        let llm = llm::LlmClient::new(&cfg.llm)?;
        match llm.health_check().await {
            Ok(()) => info!("LLM: ok"),
            Err(e) => tracing::warn!("LLM: {e}"),
        }
    } else {
        info!("LLM: not configured (skipped)");
    }

    info!("Health check complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared app state
// ---------------------------------------------------------------------------

async fn build_state(cfg: &config::Config) -> Result<api::AppState> {
    let pool = database::build_pool(&cfg.database)?;
    let db = database::DbClient::new(pool);

    // Fail fast before binding the socket — a misconfigured DB should not
    // result in a silently broken server that accepts requests but can't
    // fulfil them.
    db.ping().await.map_err(|e| {
        anyhow::anyhow!(
            "Database connectivity check failed: {}\n\
             Hint: verify POSTGRES_* env vars are correct, postgresql@17 is running, \
             and shared_preload_libraries = 'timescaledb' is set in postgresql.conf \
             (run `just install` to configure this automatically).\n\
             Cause chain: {}",
            e,
            error_chain(&e)
        )
    })?;

    let scraper = Arc::new(scraper::Scraper::new(&cfg.crawler, &cfg.proxy)?);
    let llm = Arc::new(llm::LlmClient::new(&cfg.llm)?);
    let resources = api::ResourceSnapshot::new();
    api::spawn_resource_monitor(resources.clone());

    Ok(api::AppState {
        db,
        scraper,
        llm,
        cfg: Arc::new(cfg.clone()),
        resources,
    })
}

/// Walks the `std::error::Error::source()` chain and returns each cause
/// joined with " -> " so the full context is visible in a single log line.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = Vec::new();
    let mut src = e.source();
    while let Some(cause) = src {
        parts.push(cause.to_string());
        src = cause.source();
    }
    if parts.is_empty() {
        String::from("(no further cause)")
    } else {
        parts.join(" -> ")
    }
}

async fn ping_redis(url: &str) -> Result<()> {
    let client = redis::Client::open(url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;
    let _: String = redis::cmd("PING").query_async(&mut conn).await?;
    Ok(())
}
