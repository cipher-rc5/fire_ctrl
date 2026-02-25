/// file: src/api.rs
/// description: HTTP router, auth/resource checks, and Firecrawl v2 handlers.
/// HTTP API — all Firecrawl v2 endpoints.
///
/// Route map (matches official spec):
///   POST  /v2/scrape
///   POST  /v2/crawl
///   GET   /v2/crawl/:id          — status + paginated results
///   DELETE /v2/crawl/:id         — cancel
///   GET   /v2/crawl/:id/errors
///   POST  /v2/batch/scrape
///   GET   /v2/batch/scrape/:id
///   DELETE /v2/batch/scrape/:id
///   POST  /v2/map
///   POST  /v2/extract
///   GET   /v2/extract/:id
///   POST  /v2/search
///   GET   /health
use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::{delete, get, post},
};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use sysinfo::{CpuRefreshKind, RefreshKind, System};
use tracing::warn;
use uuid::Uuid;

use crate::config::Config;
use crate::database::DbClient;
use crate::llm::LlmClient;
use crate::models::*;
use crate::scraper::Scraper;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Cached resource snapshot refreshed every 5 s in the background.
/// Stored as packed u64: high 32 bits = cpu_millipct, low 32 bits = ram_millipct.
#[derive(Clone)]
pub struct ResourceSnapshot(Arc<AtomicU64>);

impl ResourceSnapshot {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    fn store(&self, cpu_frac: f64, ram_frac: f64) {
        let cpu = (cpu_frac * 1_000.0) as u64;
        let ram = (ram_frac * 1_000.0) as u64;
        self.0.store((cpu << 32) | ram, Ordering::Relaxed);
    }

    fn load(&self) -> (f64, f64) {
        let packed = self.0.load(Ordering::Relaxed);
        let cpu = (packed >> 32) as f64 / 1_000.0;
        let ram = (packed & 0xFFFF_FFFF) as f64 / 1_000.0;
        (cpu, ram)
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: DbClient,
    pub scraper: Arc<Scraper>,
    pub llm: Arc<LlmClient>,
    pub cfg: Arc<Config>,
    pub resources: ResourceSnapshot,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router {
    Router::new()
        // health
        .route("/health", get(health_handler))
        // scrape
        .route("/v2/scrape", post(scrape_handler))
        // crawl
        .route("/v2/crawl", post(crawl_handler))
        .route("/v2/crawl/{id}", get(crawl_status_handler))
        .route("/v2/crawl/{id}", delete(crawl_cancel_handler))
        .route("/v2/crawl/{id}/errors", get(crawl_errors_handler))
        .route("/v2/crawl/ongoing", get(crawl_ongoing_handler))
        // batch_scrape
        .route("/v2/batch/scrape", post(batch_scrape_handler))
        .route("/v2/batch/scrape/{id}", get(batch_status_handler))
        .route("/v2/batch/scrape/{id}", delete(batch_cancel_handler))
        // map
        .route("/v2/map", post(map_handler))
        // extract
        .route("/v2/extract", post(extract_handler))
        .route("/v2/extract/{id}", get(extract_status_handler))
        // search
        .route("/v2/search", post(search_handler))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// auth_middleware_helper
// ---------------------------------------------------------------------------

fn check_auth(headers: &HeaderMap, cfg: &Config) -> Result<(), AppError> {
    if !cfg.auth.use_db_authentication {
        return Ok(()); // self-hosted, auth bypassed
    }

    let key = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_owned());

    match (key, &cfg.auth.test_api_key) {
        (Some(k), _) if k == cfg.auth.bull_auth_key => Ok(()),
        (Some(k), Some(expected)) if &k == expected => Ok(()),
        // No key sent, or no configured key — always reject when auth is enabled.
        _ => Err(AppError::Unauthorized),
    }
}

// ---------------------------------------------------------------------------
// resource gate (cpu / ram) — reads from pre-computed background snapshot
// ---------------------------------------------------------------------------

fn check_resources(state: &AppState) -> Result<(), AppError> {
    let (cpu, ram) = state.resources.load();
    let cfg = &state.cfg;

    if cpu > cfg.resource.max_cpu {
        return Err(AppError::ResourceLimit(format!(
            "CPU usage {:.0}% exceeds limit {:.0}%",
            cpu * 100.0,
            cfg.resource.max_cpu * 100.0
        )));
    }
    if ram > cfg.resource.max_ram {
        return Err(AppError::ResourceLimit(format!(
            "RAM usage {:.0}% exceeds limit {:.0}%",
            ram * 100.0,
            cfg.resource.max_ram * 100.0
        )));
    }
    Ok(())
}

/// Spawns a background task that refreshes CPU/RAM metrics every 5 seconds.
pub fn spawn_resource_monitor(snapshot: ResourceSnapshot) {
    tokio::spawn(async move {
        let mut sys = System::new_with_specifics(
            RefreshKind::everything().with_cpu(CpuRefreshKind::everything()),
        );
        loop {
            sys.refresh_cpu_all();
            sys.refresh_memory();

            let cpu = sys.global_cpu_usage() as f64 / 100.0;
            let total_mem = sys.total_memory() as f64;
            let used_mem = sys.used_memory() as f64;
            let ram = if total_mem > 0.0 {
                used_mem / total_mem
            } else {
                0.0
            };
            snapshot.store(cpu, ram);

            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = state.db.ping().await;
    let redis_primary_ok = ping_redis(&state.cfg.redis.url).await;
    let redis_rate_ok = ping_redis(&state.cfg.redis.rate_limit_url).await;
    let redis_ok = match (&redis_primary_ok, &redis_rate_ok) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) => Err(format!("primary redis: {e}")),
        (Ok(()), Err(e)) => Err(format!("rate-limit redis: {e}")),
        (Err(a), Err(b)) => Err(format!("primary redis: {a}; rate-limit redis: {b}")),
    };
    let rabbit_ok = ping_rabbitmq(&state.cfg.rabbitmq.url).await;
    let llm_ok = if state.cfg.llm.is_configured() {
        state.llm.health_check().await.is_ok()
    } else {
        true // not configured → not checked
    };

    let overall = if db_ok.is_ok() && redis_ok.is_ok() && rabbit_ok.is_ok() && llm_ok {
        "ok"
    } else {
        "degraded"
    };

    let body = HealthResponse {
        status: overall.to_string(),
        services: ServiceHealth {
            database: ComponentStatus {
                healthy: db_ok.is_ok(),
                error: db_ok.err().map(|e| e.to_string()),
            },
            redis: ComponentStatus {
                healthy: redis_ok.is_ok(),
                error: redis_ok.err(),
            },
            rabbitmq: ComponentStatus {
                healthy: rabbit_ok.is_ok(),
                error: rabbit_ok.err(),
            },
            llm: ComponentStatus {
                healthy: llm_ok,
                error: None,
            },
        },
    };

    let status = if overall == "ok" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body))
}

async fn ping_redis(url: &str) -> Result<(), String> {
    let client = redis::Client::open(url).map_err(|e| e.to_string())?;
    let mut conn = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.get_multiplexed_async_connection(),
    )
    .await
    .map_err(|_| "connection timed out".to_string())
    .and_then(|r| r.map_err(|e| e.to_string()))?;

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        redis::cmd("PING").query_async::<String>(&mut conn),
    )
    .await
    .map_err(|_| "PING timed out".to_string())
    .and_then(|r| r.map_err(|e| e.to_string()))?;
    Ok(())
}

async fn ping_rabbitmq(url: &str) -> Result<(), String> {
    let conn = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        lapin::Connection::connect(url, lapin::ConnectionProperties::default()),
    )
    .await
    .map_err(|_| "connection timed out".to_string())
    .and_then(|r| r.map_err(|e| e.to_string()))?;
    conn.close(0, "healthcheck".into())
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// POST /v2/scrape  — synchronous
// ---------------------------------------------------------------------------

async fn scrape_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ScrapeRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;
    check_resources(&state)?;

    let requested_formats = if req.formats.is_empty() {
        vec![OutputFormat::Markdown]
    } else {
        req.formats.clone()
    };

    let mut effective_formats = requested_formats.clone();
    if req.extract.is_some()
        && !effective_formats.contains(&OutputFormat::Markdown)
        && !effective_formats.contains(&OutputFormat::Html)
        && !effective_formats.contains(&OutputFormat::RawHtml)
    {
        // extraction needs page content; ensure one text-bearing format is fetched.
        effective_formats.push(OutputFormat::Markdown);
    }

    let job = ScrapeJobData {
        url: req.url.clone(),
        formats: effective_formats,
        only_main_content: req.only_main_content,
        include_tags: req.include_tags,
        exclude_tags: req.exclude_tags,
        headers: req.headers,
        wait_for: req.wait_for,
        mobile: req.mobile,
        timeout: req.timeout,
        extract: req.extract.clone(),
        actions: req.actions,
        location: req.location,
        webhook: req.webhook,
    };

    let mut result = state.scraper.scrape(&job).await?;

    // llm_extraction (inline, synchronous).
    if let Some(ref extract) = req.extract
        && state.cfg.llm.is_configured()
    {
        let mut content = result
            .markdown
            .clone()
            .or_else(|| result.html.clone())
            .or_else(|| result.raw_html.clone())
            .unwrap_or_default();

        let total_chars = content.chars().count();
        let max_chars = state.cfg.llm.max_input_chars;
        if total_chars > max_chars {
            content = content.chars().take(max_chars).collect();
            result.warning = Some(format!(
                "Extraction input was truncated from {} to {} characters. Refine the prompt or request fewer fields for best results.",
                total_chars, max_chars
            ));
        }

        match state
            .llm
            .extract(&crate::llm::ExtractionRequest {
                content,
                prompt: extract.prompt.clone().unwrap_or_default(),
                schema: extract.schema.clone(),
                system_prompt: extract.system_prompt.clone(),
            })
            .await
        {
            Ok(ex) => {
                result.extract = Some(ex.data);
                if let Some(w) = ex.warning {
                    result.warning = Some(match result.warning.take() {
                        Some(existing) => format!("{} {}", existing, w),
                        None => w,
                    });
                }
            }
            Err(e) => {
                warn!(error = %e, "LLM extraction failed");
            }
        }
    }

    // if markdown/html/raw_html were only fetched to power extraction, hide them
    // unless explicitly requested by the caller.
    if !requested_formats.contains(&OutputFormat::Markdown) {
        result.markdown = None;
    }
    if !requested_formats.contains(&OutputFormat::Html) {
        result.html = None;
    }
    if !requested_formats.contains(&OutputFormat::RawHtml) {
        result.raw_html = None;
    }

    Ok(Json(ScrapeResponse {
        success: true,
        data: result,
    }))
}

// ---------------------------------------------------------------------------
// POST /v2/crawl  — async, returns job ID
// ---------------------------------------------------------------------------

async fn crawl_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CrawlRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;
    check_resources(&state)?;

    let max_depth = req.max_depth.min(state.cfg.crawler.max_depth);
    let limit = req.limit.min(state.cfg.crawler.max_pages);

    // Create the crawl group first; its ID is what we expose to the caller.
    // We embed group_id in the job data so the worker reuses this group
    // rather than creating a duplicate.
    let group = state.db.create_crawl_group(Uuid::nil(), 86_400_000).await?;

    let job_data = JobEnvelope::Crawl(CrawlJobData {
        url: req.url,
        max_depth,
        limit,
        scrape_options: req.scrape_options,
        allow_backward_links: req.allow_backward_links,
        allow_external_links: req.allow_external_links,
        ignore_sitemap: req.ignore_sitemap,
        include_paths: req.include_paths,
        exclude_paths: req.exclude_paths,
        webhook: req.webhook,
        priority: req.priority,
        ttl_ms: 86_400_000, // 24h default TTL
        group_id: Some(group.id),
    });

    let data = serde_json::to_value(&job_data)?;
    state
        .db
        .enqueue(&data, req.priority, None, Some(group.id), None)
        .await?;

    Ok(Json(CrawlResponse {
        success: true,
        id: group.id,
        url: format!("/v2/crawl/{}", group.id),
    }))
}

// ---------------------------------------------------------------------------
// GET /v2/crawl/:id
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PaginationParams {
    limit: Option<i64>,
    #[serde(rename = "after")]
    after_id: Option<Uuid>,
    #[serde(rename = "after_created_at")]
    after_created_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn crawl_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(params): Query<PaginationParams>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;

    // `id` is the crawl group ID returned by POST /v2/crawl.
    let group = state.db.get_crawl_group(id).await?;
    let counts = state.db.get_group_counts(id).await?;

    let page_limit = params.limit.unwrap_or(25).min(100);
    let rows = state
        .db
        .get_group_results(id, page_limit, params.after_id, params.after_created_at)
        .await?;

    let data: Vec<ScrapeResult> = rows
        .iter()
        .filter_map(|r| {
            r.returnvalue
                .as_ref()
                .and_then(|v| serde_json::from_value(v.clone()).ok())
        })
        .collect();

    let next_url = if data.len() == page_limit as usize {
        rows.last().map(|r| {
            format!(
                "/v2/crawl/{}?after={}&after_created_at={}&limit={}",
                id,
                r.id,
                r.created_at.to_rfc3339(),
                page_limit
            )
        })
    } else {
        None
    };

    // derive overall job status from the group status and pending counts.
    let overall_status = match group.status {
        GroupStatus::Completed => JobStatus::Completed,
        GroupStatus::Cancelled => JobStatus::Cancelled,
        GroupStatus::Active if counts.pending == 0 && counts.total > 0 => JobStatus::Completed,
        _ => JobStatus::Scraping,
    };

    Ok(Json(CrawlStatusResponse {
        status: overall_status,
        total: counts.total,
        completed: counts.completed,
        failed: counts.failed,
        credits_used: counts.completed,
        expires_at: group.expires_at,
        next: next_url,
        data,
    }))
}

// ---------------------------------------------------------------------------
// DELETE /v2/crawl/:id  — cancel
// ---------------------------------------------------------------------------

async fn crawl_cancel_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;

    if state.db.get_crawl_group(id).await.is_ok() {
        state.db.cancel_group(id).await?;
    } else {
        let job = state.db.get_job(id).await?;
        if let Some(group_id) = job.group_id {
            state.db.cancel_group(group_id).await?;
        }
        state.db.cancel_job(id).await?;
    }

    Ok(Json(serde_json::json!({ "status": "cancelled" })))
}

// ---------------------------------------------------------------------------
// GET /v2/crawl/:id/errors
// ---------------------------------------------------------------------------

async fn crawl_errors_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;

    let group_id = if state.db.get_crawl_group(id).await.is_ok() {
        id
    } else {
        let job = state.db.get_job(id).await?;
        job.group_id.ok_or(AppError::NotFound(id.to_string()))?
    };

    // Return all failed sub-jobs with their reasons.
    let rows = state
        .db
        .get_group_results(group_id, 1000, None, None)
        .await?;
    let errors: Vec<_> = rows
        .iter()
        .filter(|r| matches!(r.status, JobStatus::Failed))
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "timestamp": r.created_at,
                "url": r.data.as_ref().and_then(|d| d["payload"]["url"].as_str()).unwrap_or(""),
                "error": r.failedreason.as_deref().unwrap_or("unknown"),
            })
        })
        .collect();

    Ok(Json(
        serde_json::json!({ "errors": errors, "robotsBlocked": [] }),
    ))
}

// ---------------------------------------------------------------------------
// GET /v2/crawl/ongoing
// ---------------------------------------------------------------------------

async fn crawl_ongoing_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;
    let groups = state.db.get_active_groups().await?;
    let items: Vec<_> = groups
        .iter()
        .map(|g| {
            serde_json::json!({
                "id": g.id,
                "status": g.status,
                "created_at": g.created_at,
                "owner_id": g.owner_id,
                "ttl": g.ttl
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "success": true, "crawls": items }),
    ))
}

// ---------------------------------------------------------------------------
// POST /v2/batch/scrape
// ---------------------------------------------------------------------------

async fn batch_scrape_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<BatchScrapeRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;
    check_resources(&state)?;

    let invalid_urls: Vec<String> = req
        .urls
        .iter()
        .filter(|u| url::Url::parse(u).is_err())
        .cloned()
        .collect();

    let valid_urls: Vec<String> = req
        .urls
        .into_iter()
        .filter(|u| url::Url::parse(u).is_ok())
        .collect();

    let job_data = JobEnvelope::BatchScrape(BatchScrapeJobData {
        urls: valid_urls,
        scrape_options: req.scrape_options,
        webhook: req.webhook,
        priority: req.priority,
    });

    let data = serde_json::to_value(&job_data)?;
    let row = state
        .db
        .enqueue(&data, req.priority, None, None, None)
        .await?;

    Ok(Json(BatchScrapeResponse {
        success: true,
        id: row.id,
        url: format!("/v2/batch/scrape/{}", row.id),
        invalid_urls,
    }))
}

// ---------------------------------------------------------------------------
// GET /v2/batch/scrape/:id  (reuses crawl status logic)
// DELETE /v2/batch/scrape/:id
// ---------------------------------------------------------------------------

async fn batch_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(params): Query<PaginationParams>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;

    let job = state.db.get_job(id).await?;
    let Some(group_id) = job.group_id else {
        return Ok(Json(CrawlStatusResponse {
            status: job.status,
            total: 0,
            completed: 0,
            failed: 0,
            credits_used: 0,
            expires_at: None,
            next: None,
            data: vec![],
        }));
    };

    let group = state.db.get_crawl_group(group_id).await?;
    let counts = state.db.get_group_counts(group_id).await?;
    let page_limit = params.limit.unwrap_or(25).min(100);
    let rows = state
        .db
        .get_group_results(
            group_id,
            page_limit,
            params.after_id,
            params.after_created_at,
        )
        .await?;

    let data: Vec<ScrapeResult> = rows
        .iter()
        .filter_map(|r| {
            r.returnvalue
                .as_ref()
                .and_then(|v| serde_json::from_value(v.clone()).ok())
        })
        .collect();

    let next_url = if data.len() == page_limit as usize {
        rows.last().map(|r| {
            format!(
                "/v2/batch/scrape/{}?after={}&after_created_at={}&limit={}",
                id,
                r.id,
                r.created_at.to_rfc3339(),
                page_limit
            )
        })
    } else {
        None
    };

    let overall_status = match group.status {
        GroupStatus::Completed => JobStatus::Completed,
        GroupStatus::Cancelled => JobStatus::Cancelled,
        GroupStatus::Active if counts.pending == 0 && counts.total > 0 => JobStatus::Completed,
        _ => JobStatus::Scraping,
    };

    Ok(Json(CrawlStatusResponse {
        status: overall_status,
        total: counts.total,
        completed: counts.completed,
        failed: counts.failed,
        credits_used: counts.completed,
        expires_at: group.expires_at,
        next: next_url,
        data,
    }))
}

async fn batch_cancel_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    crawl_cancel_handler(State(state), headers, path).await
}

// ---------------------------------------------------------------------------
// POST /v2/map  — synchronous URL discovery
// ---------------------------------------------------------------------------

async fn map_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MapRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;

    let links = state
        .scraper
        .map_urls(
            &req.url,
            req.search.as_deref(),
            req.ignore_sitemap,
            req.include_subdomains,
            req.limit,
        )
        .await?;

    Ok(Json(MapResponse {
        success: true,
        links: links
            .into_iter()
            .map(|url| SearchWebResult {
                url,
                title: None,
                description: None,
                category: None,
            })
            .collect(),
    }))
}

// ---------------------------------------------------------------------------
// POST /v2/extract  — async LLM extraction across multiple URLs
// ---------------------------------------------------------------------------

async fn extract_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ExtractRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;

    if !state.cfg.llm.is_configured() {
        return Err(AppError::BadRequest(
            "LLM not configured. Set OPENAI_API_KEY or OLLAMA_BASE_URL.".to_string(),
        ));
    }

    let job_data = JobEnvelope::Extract(ExtractJobData {
        urls: req.urls,
        prompt: req.prompt,
        schema: req.schema,
        system_prompt: req.system_prompt,
        allow_external_links: req.allow_external_links,
        enable_web_search: req.enable_web_search,
        ignore_invalid_urls: req.ignore_invalid_urls,
    });

    let data = serde_json::to_value(&job_data)?;
    let row = state.db.enqueue(&data, 0, None, None, None).await?;

    Ok(Json(ExtractResponse {
        success: true,
        id: row.id,
        url: format!("/v2/extract/{}", row.id),
    }))
}

// ---------------------------------------------------------------------------
// GET /v2/extract/:id
// ---------------------------------------------------------------------------

async fn extract_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;

    let row = state.db.get_job(id).await?;
    let data = row.returnvalue.and_then(|v| v.get("data").cloned());

    Ok(Json(ExtractStatusResponse {
        status: row.status,
        data,
        error: row.failedreason,
    }))
}

// ---------------------------------------------------------------------------
// POST /v2/search
// ---------------------------------------------------------------------------

async fn search_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SearchRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_auth(&headers, &state.cfg)?;
    check_resources(&state)?;

    let urls = search_urls(
        &req.query,
        req.limit,
        req.lang.as_deref(),
        req.country.as_deref(),
        &state.cfg,
        &state.scraper,
    )
    .await?;

    let mut results: Vec<SearchWebResult> = Vec::new();
    let scrape_opts = req.scrape_options.as_ref();

    for url in urls {
        let job = ScrapeJobData {
            url: url.clone(),
            formats: scrape_opts
                .map(|o| o.formats.clone())
                .unwrap_or_else(|| vec![OutputFormat::Markdown]),
            only_main_content: scrape_opts.map(|o| o.only_main_content).unwrap_or(true),
            include_tags: scrape_opts
                .map(|o| o.include_tags.clone())
                .unwrap_or_default(),
            exclude_tags: scrape_opts
                .map(|o| o.exclude_tags.clone())
                .unwrap_or_default(),
            headers: None,
            wait_for: scrape_opts.map(|o| o.wait_for).unwrap_or(0),
            mobile: false,
            timeout: req.timeout,
            extract: None,
            actions: vec![],
            location: None,
            webhook: None,
        };
        if let Ok(result) = state.scraper.scrape(&job).await {
            let description = result
                .markdown
                .as_ref()
                .and_then(|m| m.lines().find(|line| !line.trim().is_empty()))
                .map(|s| s.chars().take(180).collect::<String>());
            results.push(SearchWebResult {
                url: result.url,
                title: result.title,
                description,
                category: None,
            });
        }
    }

    Ok(Json(SearchResponse {
        success: true,
        data: SearchResponseData {
            web: Some(results),
            news: None,
            images: None,
        },
    }))
}

// ---------------------------------------------------------------------------
// Search URL discovery
// ---------------------------------------------------------------------------

async fn search_urls(
    query: &str,
    limit: u32,
    lang: Option<&str>,
    country: Option<&str>,
    cfg: &Config,
    scraper: &Scraper,
) -> Result<Vec<String>, AppError> {
    // if SearXNG is configured, use it.
    if let Some(ref endpoint) = cfg.search.searxng_endpoint {
        let mut urls = searxng_search(endpoint, query, limit, lang, country, cfg, scraper).await?;
        urls = crate::llm::rank_by_relevance(urls, query);
        urls.truncate(limit as usize);
        return Ok(urls);
    }

    // fall back to google-search scraping
    let mut urls = google_search(query, limit, scraper).await?;
    urls = crate::llm::rank_by_relevance(urls, query);
    urls.truncate(limit as usize);
    Ok(urls)
}

async fn searxng_search(
    endpoint: &str,
    query: &str,
    limit: u32,
    lang: Option<&str>,
    country: Option<&str>,
    cfg: &Config,
    scraper: &Scraper,
) -> Result<Vec<String>, AppError> {
    let mut url = format!(
        "{}/search?q={}&format=json",
        endpoint,
        urlencoding::encode(query)
    );
    if let Some(ref engines) = cfg.search.searxng_engines {
        url.push_str(&format!("&engines={}", urlencoding::encode(engines)));
    }
    if let Some(ref cats) = cfg.search.searxng_categories {
        url.push_str(&format!("&categories={}", urlencoding::encode(cats)));
    }
    if let Some(language) = lang {
        url.push_str(&format!("&language={}", urlencoding::encode(language)));
    }
    if let Some(region) = country {
        url.push_str(&format!("&region={}", urlencoding::encode(region)));
    }

    let (_, _, body_text) = scraper.http.fetch(&url).await?;
    let body: serde_json::Value = serde_json::from_str(&body_text)
        .map_err(|e| AppError::Scraper(format!("SearXNG response parse failed: {e}")))?;

    let urls: Vec<String> = body["results"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|r| r["url"].as_str().map(|s| s.to_string()))
        .take(limit as usize)
        .collect();

    Ok(urls)
}

async fn google_search(
    query: &str,
    limit: u32,
    scraper: &Scraper,
) -> Result<Vec<String>, AppError> {
    let search_url = format!(
        "https://www.google.com/search?q={}&num={}",
        urlencoding::encode(query),
        limit.min(10)
    );

    let (_, _, html) = scraper.http.fetch(&search_url).await?;

    let doc = scraper::Html::parse_document(&html);
    let sel = scraper::Selector::parse("a[href]")
        .map_err(|e| AppError::Scraper(format!("Failed to parse link selector: {e}")))?;
    let mut urls: Vec<String> = Vec::new();

    for el in doc.select(&sel) {
        // google wraps results as /url?q=<actual_url>&…
        if let Some(href) = el.value().attr("href")
            && let Some(stripped) = href.strip_prefix("/url?q=")
            && let Some(end) = stripped.find('&')
        {
            let raw = &stripped[..end];
            if let Ok(decoded) = urlencoding::decode(raw)
                && decoded.starts_with("http")
            {
                urls.push(decoded.to_string());
                if urls.len() >= limit as usize {
                    break;
                }
            }
        }
    }

    Ok(urls)
}
