/// file: src/worker.rs
/// description: Background worker pool, job execution, stall recovery, and webhooks.
/// Worker pool — claims jobs from the queue, executes scrape/crawl/batch/extract,
/// handles stall recovery, and fires webhooks on completion.
use crate::config::WorkerConfig;
use crate::database::DbClient;
use crate::llm::{ExtractionRequest, LlmClient};
use crate::models::{
    AppError, BatchScrapeJobData, CrawlJobData, ExtractJobData, GroupStatus, JobEnvelope,
    ScrapeJobData, ScrapeResult,
};
use crate::scraper::Scraper;
use anyhow::Result;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{error, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// WorkerPool
// ---------------------------------------------------------------------------

pub struct WorkerPool {
    db: DbClient,
    scraper: Arc<Scraper>,
    llm: Arc<LlmClient>,
    cfg: WorkerConfig,
    allow_local_webhooks: bool,
}

impl WorkerPool {
    pub fn new(
        db: DbClient,
        scraper: Arc<Scraper>,
        llm: Arc<LlmClient>,
        cfg: WorkerConfig,
        allow_local_webhooks: bool,
    ) -> Self {
        Self {
            db,
            scraper,
            llm,
            cfg,
            allow_local_webhooks,
        }
    }

    pub async fn run(self) -> Result<()> {
        let db = Arc::new(self.db);
        let scraper = Arc::clone(&self.scraper);
        let llm = Arc::clone(&self.llm);
        let cfg = Arc::new(self.cfg);
        // Single shared HTTP client for all webhook deliveries.
        let webhook_client = Arc::new(reqwest::Client::new());

        let sem = Arc::new(Semaphore::new(cfg.max_concurrent_jobs as usize));

        // Stall-recovery ticker — runs the first scan immediately on startup
        // (don't wait the full interval), then on a fixed cadence.
        {
            let db2 = Arc::clone(&db);
            let cfg2 = Arc::clone(&cfg);
            tokio::spawn(async move {
                stall_recovery_loop(db2, cfg2).await;
            });
        }

        // Worker tasks.  Keyed by stable worker id so that a respawned worker
        // reuses the dead worker's id (logs/metrics keep their identity)
        // rather than collapsing to `set.len()`, which collides as workers die.
        //
        // `next_id` is a monotonically increasing counter — ids are never
        // recycled across a fresh allocation; an id is only reused when its
        // owning worker dies and we respawn it in place.
        let mut next_id: u32 = 0;
        let mut workers: HashMap<u32, tokio::task::JoinHandle<()>> =
            HashMap::with_capacity(cfg.num_workers as usize);

        #[allow(clippy::explicit_counter_loop)] // next_id survives the loop intentionally
        for _ in 0..cfg.num_workers {
            let id = next_id;
            next_id = next_id.saturating_add(1);
            let handle = spawn_worker(
                id,
                Arc::clone(&db),
                Arc::clone(&scraper),
                Arc::clone(&llm),
                Arc::clone(&cfg),
                Arc::clone(&sem),
                self.allow_local_webhooks,
                Arc::clone(&webhook_client),
            );
            workers.insert(id, handle);
        }

        // Monitor; respawn workers under their original (stable) id.  We
        // poll all handles via `futures::future::select_all` so the id of the
        // finishing task is preserved regardless of clean exit vs panic.
        while !workers.is_empty() {
            // Take ownership of all handles into a Vec so select_all can
            // await them; pair each with its id.
            let (ids, handles): (Vec<u32>, Vec<tokio::task::JoinHandle<()>>) =
                workers.drain().unzip();

            let (res, idx, remaining) = futures::future::select_all(handles).await;
            let dead_id = ids[idx];

            if let Err(e) = res {
                warn!(worker_id = dead_id, error = %e, "Worker task panicked — respawning");
            }

            // Restore the surviving handles to the map, then respawn the
            // dead worker under the same id.
            for (i, h) in remaining.into_iter().enumerate() {
                let id = if i >= idx { ids[i + 1] } else { ids[i] };
                workers.insert(id, h);
            }

            let handle = spawn_worker(
                dead_id,
                Arc::clone(&db),
                Arc::clone(&scraper),
                Arc::clone(&llm),
                Arc::clone(&cfg),
                Arc::clone(&sem),
                self.allow_local_webhooks,
                Arc::clone(&webhook_client),
            );
            workers.insert(dead_id, handle);
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_worker(
    id: u32,
    db: Arc<DbClient>,
    scraper: Arc<Scraper>,
    llm: Arc<LlmClient>,
    cfg: Arc<WorkerConfig>,
    sem: Arc<Semaphore>,
    allow_local_webhooks: bool,
    webhook_client: Arc<reqwest::Client>,
) -> tokio::task::JoinHandle<()> {
    let w = Worker {
        id,
        lock_id: Uuid::new_v4(),
        db,
        scraper,
        llm,
        cfg,
        sem,
        allow_local_webhooks,
        webhook_client,
    };
    tokio::spawn(async move { w.run().await })
}

// ---------------------------------------------------------------------------
// Stall recovery loop
// ---------------------------------------------------------------------------

async fn stall_recovery_loop(db: Arc<DbClient>, cfg: Arc<WorkerConfig>) {
    let stale_after = Duration::from_secs(cfg.job_timeout_seconds + 30);
    let mut backoff = Duration::from_secs(2);
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    // First tick fires immediately — we want the initial scan at startup.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        match db.requeue_stalled_jobs(stale_after).await {
            Ok(n) if n > 0 => {
                info!(count = n, "Re-queued stalled jobs");
                backoff = Duration::from_secs(2);
            }
            Ok(_) => {
                backoff = Duration::from_secs(2);
            }
            Err(e) => {
                warn!(error = %e, "Stall-recovery requeue failed; backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(120));
                interval.tick().await;
                continue;
            }
        }
        match db.fail_over_stalled_jobs(cfg.max_stalls as i32).await {
            Ok(n) if n > 0 => warn!(count = n, "Permanently failed over-stalled jobs"),
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "Stall fail-over DB error");
            }
        }
        interval.tick().await;
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

struct Worker {
    id: u32,
    lock_id: Uuid,
    db: Arc<DbClient>,
    scraper: Arc<Scraper>,
    llm: Arc<LlmClient>,
    cfg: Arc<WorkerConfig>,
    sem: Arc<Semaphore>,
    allow_local_webhooks: bool,
    webhook_client: Arc<reqwest::Client>,
}

impl Worker {
    async fn run(self) {
        info!(worker_id = self.id, lock_id = %self.lock_id, "Worker started");
        loop {
            // Acquire concurrency permit before touching the DB.
            // If the semaphore has been closed (pool shutdown), exit cleanly —
            // never panic.
            let permit = match self.sem.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    warn!(worker_id = self.id, "worker semaphore closed, exiting");
                    return;
                }
            };

            match self.db.claim_next_job(self.lock_id).await {
                Ok(None) => {
                    drop(permit);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Ok(Some(row)) => {
                    let job_id = row.id;
                    let created_at = row.created_at;

                    let envelope: JobEnvelope = match row
                        .data
                        .as_ref()
                        .and_then(|d| serde_json::from_value(d.clone()).ok())
                    {
                        Some(e) => e,
                        None => {
                            error!(worker_id = self.id, %job_id, "Failed to deserialise job data");
                            let _ = self
                                .db
                                .fail_job(job_id, created_at, "invalid job envelope")
                                .await;
                            drop(permit);
                            continue;
                        }
                    };

                    let timeout = Duration::from_secs(self.cfg.job_timeout_seconds);
                    let result =
                        tokio::time::timeout(timeout, self.execute(job_id, created_at, envelope))
                            .await;

                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            error!(worker_id = self.id, %job_id, error = %e, "Job failed");
                            let _ = self.db.fail_job(job_id, created_at, &e.to_string()).await;
                        }
                        Err(_) => {
                            error!(worker_id = self.id, %job_id, "Job timed out");
                            let _ = self.db.fail_job(job_id, created_at, "job timeout").await;
                        }
                    }

                    drop(permit);
                }
                Err(e) => {
                    let cause_chain = {
                        let mut parts = Vec::new();
                        let mut src: Option<&dyn StdError> = (&e as &dyn StdError).source();
                        while let Some(cause) = src {
                            parts.push(cause.to_string());
                            src = cause.source();
                        }
                        if parts.is_empty() {
                            String::new()
                        } else {
                            format!(" -> {}", parts.join(" -> "))
                        }
                    };
                    error!(
                        worker_id = self.id,
                        error = %e,
                        cause_chain = %cause_chain,
                        "Error claiming job; retrying"
                    );
                    drop(permit);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    async fn execute(
        &self,
        job_id: Uuid,
        created_at: chrono::DateTime<chrono::Utc>,
        envelope: JobEnvelope,
    ) -> Result<(), AppError> {
        match envelope {
            JobEnvelope::Scrape(job) => self.run_scrape(job_id, created_at, job).await,
            JobEnvelope::Crawl(job) => self.run_crawl(job_id, created_at, job).await,
            JobEnvelope::BatchScrape(job) => self.run_batch_scrape(job_id, created_at, job).await,
            JobEnvelope::Extract(job) => self.run_extract(job_id, created_at, job).await,
            JobEnvelope::Map(_job) => {
                // Map jobs are served synchronously by POST /v2/map — the API
                // never enqueues them, and there is no per-URL work for a
                // worker to do.  If a Map envelope reaches the queue (legacy
                // or malformed enqueue), reject it with a terminal failure
                // rather than silently no-op'ing a fake "completed" result.
                warn!(%job_id, "Map envelope reached worker queue — rejecting");
                Err(AppError::BadRequest(
                    "map jobs are synchronous and must not be enqueued".to_string(),
                ))
            }
        }
    }

    // ── Scrape ────────────────────────────────────────────────────────────────

    async fn run_scrape(
        &self,
        job_id: Uuid,
        created_at: chrono::DateTime<chrono::Utc>,
        job: ScrapeJobData,
    ) -> Result<(), AppError> {
        let webhook = job.webhook.clone();
        let mut result = self.scraper.scrape(&job).await?;

        // LLM extraction if requested.
        if let Some(ref extract) = job.extract {
            result = self
                .apply_extraction(
                    result,
                    extract.prompt.as_deref(),
                    extract.schema.as_ref(),
                    extract.system_prompt.as_deref(),
                )
                .await;
        }

        let rv = serde_json::to_value(&result)?;
        self.db.complete_job(job_id, created_at, &rv).await?;

        if let Some(url) = webhook {
            fire_webhook(
                &self.webhook_client,
                self.allow_local_webhooks,
                &url,
                &serde_json::json!({
                    "type": "scrape.completed",
                    "job_id": job_id,
                    "data": result,
                }),
            )
            .await;
        }

        Ok(())
    }

    // ── Crawl ─────────────────────────────────────────────────────────────────

    async fn run_crawl(
        &self,
        job_id: Uuid,
        created_at: chrono::DateTime<chrono::Utc>,
        job: CrawlJobData,
    ) -> Result<(), AppError> {
        let webhook = job.webhook.clone();

        // Reuse the group that the API handler already created and embedded in
        // the job data.  If (for any legacy reason) it's missing, create one.
        let group = if let Some(gid) = job.group_id {
            self.db.get_crawl_group(gid).await?
        } else {
            let g = self.db.create_crawl_group(Uuid::nil(), job.ttl_ms).await?;
            self.db.update_job_group(job_id, created_at, g.id).await?;
            g
        };

        // Fire webhook "started".
        if let Some(ref wh) = webhook {
            fire_webhook(
                &self.webhook_client,
                self.allow_local_webhooks,
                &wh.url,
                &serde_json::json!({
                    "type": "crawl.started",
                    "job_id": job_id,
                    "group_id": group.id,
                }),
            )
            .await;
        }

        let results = self
            .scraper
            .crawl(crate::scraper::CrawlParams {
                root_url: job.url.clone(),
                max_depth: job.max_depth,
                limit: job.limit,
                scrape_options: &job.scrape_options,
                allow_external_links: job.allow_external_links,
                ignore_sitemap: job.ignore_sitemap,
                include_paths: &job.include_paths,
                exclude_paths: &job.exclude_paths,
            })
            .await?;

        // Enqueue each page result as a completed sub-job under the group.
        for page_result in &results {
            let rv = serde_json::to_value(page_result)?;
            let sub_data = serde_json::json!({
                "kind": "scrape",
                "payload": { "url": page_result.url, "formats": ["markdown"] }
            });
            self.db
                .enqueue_completed(&sub_data, &rv, job.priority, None, Some(group.id), None)
                .await?;

            if let Some(ref wh) = webhook
                && wh.events.iter().any(|e| e == "page")
            {
                fire_webhook(
                    &self.webhook_client,
                    self.allow_local_webhooks,
                    &wh.url,
                    &serde_json::json!({
                        "type": "crawl.page",
                        "job_id": job_id,
                        "data": page_result,
                    }),
                )
                .await;
            }
        }

        self.db
            .set_group_status(group.id, GroupStatus::Completed)
            .await?;

        let rv = serde_json::json!({ "total": results.len(), "group_id": group.id });
        self.db.complete_job(job_id, created_at, &rv).await?;

        if let Some(ref wh) = webhook {
            fire_webhook(
                &self.webhook_client,
                self.allow_local_webhooks,
                &wh.url,
                &serde_json::json!({
                    "type": "crawl.completed",
                    "job_id": job_id,
                    "group_id": group.id,
                    "total": results.len(),
                }),
            )
            .await;
        }

        Ok(())
    }

    // ── Batch scrape ──────────────────────────────────────────────────────────

    async fn run_batch_scrape(
        &self,
        job_id: Uuid,
        created_at: chrono::DateTime<chrono::Utc>,
        job: BatchScrapeJobData,
    ) -> Result<(), AppError> {
        let webhook = job.webhook.clone();
        let group = self.db.create_crawl_group(Uuid::nil(), 86_400_000).await?;
        self.db
            .update_job_group(job_id, created_at, group.id)
            .await?;

        let mut results: Vec<ScrapeResult> = Vec::new();

        // Parallelise per-URL scrapes, bounded by the crawler's
        // `concurrent_requests` so we don't saturate the network or the
        // browser pool.
        let parallel = self.scraper.concurrent_requests();
        let local_sem = Arc::new(Semaphore::new(parallel));
        let mut tasks: JoinSet<(String, Result<ScrapeResult, AppError>)> = JoinSet::new();

        for url in &job.urls {
            let scrape_job = ScrapeJobData {
                url: url.clone(),
                formats: job.scrape_options.formats.clone(),
                only_main_content: job.scrape_options.only_main_content,
                include_tags: job.scrape_options.include_tags.clone(),
                exclude_tags: job.scrape_options.exclude_tags.clone(),
                headers: job.scrape_options.headers.clone(),
                wait_for: job.scrape_options.wait_for,
                mobile: job.scrape_options.mobile,
                timeout: job.scrape_options.timeout,
                extract: None,
                actions: job.scrape_options.actions.clone(),
                location: job.scrape_options.location.clone(),
                webhook: None,
            };
            let scraper = Arc::clone(&self.scraper);
            let sem = Arc::clone(&local_sem);
            let url_owned = url.clone();
            tasks.spawn(async move {
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        return (
                            url_owned,
                            Err(AppError::Scraper("local semaphore closed".into())),
                        );
                    }
                };
                let res = scraper.scrape(&scrape_job).await;
                (url_owned, res)
            });
        }

        while let Some(joined) = tasks.join_next().await {
            let (url, res) = match joined {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(error = %e, "Batch scrape: worker task panicked (skipping)");
                    continue;
                }
            };
            match res {
                Ok(r) => {
                    let rv = serde_json::to_value(&r)?;
                    let sub_data = serde_json::json!({
                        "kind": "scrape",
                        "payload": { "url": url, "formats": ["markdown"] }
                    });
                    self.db
                        .enqueue_completed(&sub_data, &rv, job.priority, None, Some(group.id), None)
                        .await?;
                    results.push(r);
                }
                Err(e) => {
                    warn!(url, error = %e, "Batch scrape: page failed (skipping)");
                }
            }
        }

        self.db
            .set_group_status(group.id, GroupStatus::Completed)
            .await?;
        let rv = serde_json::json!({ "total": results.len(), "group_id": group.id });
        self.db.complete_job(job_id, created_at, &rv).await?;

        if let Some(ref wh) = webhook {
            fire_webhook(
                &self.webhook_client,
                self.allow_local_webhooks,
                &wh.url,
                &serde_json::json!({
                    "type": "batch_scrape.completed",
                    "job_id": job_id,
                    "total": results.len(),
                }),
            )
            .await;
        }

        Ok(())
    }

    // ── Extract ───────────────────────────────────────────────────────────────

    async fn run_extract(
        &self,
        job_id: Uuid,
        created_at: chrono::DateTime<chrono::Utc>,
        job: ExtractJobData,
    ) -> Result<(), AppError> {
        let mut combined_content = String::new();
        let origin_host = job
            .urls
            .first()
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| u.host_str().map(|h| h.to_owned()));

        // Parallelise per-URL scrapes, bounded by `concurrent_requests`.
        let parallel = self.scraper.concurrent_requests();
        let local_sem = Arc::new(Semaphore::new(parallel));
        let mut tasks: JoinSet<(String, Result<ScrapeResult, AppError>)> = JoinSet::new();

        for url in &job.urls {
            if !job.allow_external_links
                && let (Some(origin), Ok(parsed)) = (origin_host.as_deref(), url::Url::parse(url))
                && parsed.host_str().unwrap_or("") != origin
            {
                continue;
            }

            let scrape_job = ScrapeJobData {
                url: url.clone(),
                formats: vec![crate::models::OutputFormat::Markdown],
                only_main_content: true,
                include_tags: vec![],
                exclude_tags: vec![],
                headers: None,
                wait_for: 0,
                mobile: false,
                timeout: None,
                extract: None,
                actions: vec![],
                location: None,
                webhook: None,
            };
            let scraper = Arc::clone(&self.scraper);
            let sem = Arc::clone(&local_sem);
            let url_owned = url.clone();
            tasks.spawn(async move {
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        return (
                            url_owned,
                            Err(AppError::Scraper("local semaphore closed".into())),
                        );
                    }
                };
                let res = scraper.scrape(&scrape_job).await;
                (url_owned, res)
            });
        }

        while let Some(joined) = tasks.join_next().await {
            let (url, res) = match joined {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(error = %e, "Extract: scrape task panicked (skipping)");
                    continue;
                }
            };
            match res {
                Ok(result) => {
                    if let Some(md) = result.markdown {
                        combined_content.push_str(&format!("## {url}\n\n{md}\n\n"));
                    }
                }
                Err(_) if job.ignore_invalid_urls => {}
                Err(_) => {
                    warn!(url, "Extract: failed to scrape URL");
                }
            }
        }

        let prompt = job
            .prompt
            .as_deref()
            .unwrap_or("Extract all relevant information.");

        let extract_req = ExtractionRequest {
            content: combined_content,
            prompt: prompt.to_string(),
            schema: job.schema.clone(),
            system_prompt: job.system_prompt.clone(),
        };

        let extraction = self
            .llm
            .extract(&extract_req)
            .await
            .map_err(AppError::Llm)?;

        let rv = serde_json::json!({ "data": extraction.data });
        self.db.complete_job(job_id, created_at, &rv).await?;

        Ok(())
    }

    // ── LLM extraction helper ─────────────────────────────────────────────────

    async fn apply_extraction(
        &self,
        mut result: ScrapeResult,
        prompt: Option<&str>,
        schema: Option<&serde_json::Value>,
        system_prompt: Option<&str>,
    ) -> ScrapeResult {
        let mut content = result
            .markdown
            .clone()
            .or_else(|| result.html.clone())
            .unwrap_or_default();

        let total_chars = content.chars().count();
        let max_chars = self.llm.max_input_chars();
        if total_chars > max_chars {
            content = content.chars().take(max_chars).collect();
            result.warning = Some(format!(
                "Extraction input was truncated from {} to {} characters. Refine the prompt or request fewer fields for best results.",
                total_chars, max_chars
            ));
        }

        let extract_req = ExtractionRequest {
            content,
            prompt: prompt
                .unwrap_or("Extract all structured data from this page.")
                .to_string(),
            schema: schema.cloned(),
            system_prompt: system_prompt.map(|s| s.to_string()),
        };

        match self.llm.extract(&extract_req).await {
            Ok(extraction) => {
                result.extract = Some(extraction.data);
                if let Some(w) = extraction.warning {
                    result.warning = Some(match result.warning.take() {
                        Some(existing) => format!("{} {}", existing, w),
                        None => w,
                    });
                }
            }
            Err(e) => {
                warn!(error = %e, "LLM extraction failed (result still returned)");
                result.metadata.error = Some(format!("LLM extraction failed: {e}"));
            }
        }

        result
    }
}

// ---------------------------------------------------------------------------
// Webhook helper
// ---------------------------------------------------------------------------

async fn fire_webhook(
    client: &reqwest::Client,
    allow_local_webhooks: bool,
    url: &str,
    payload: &serde_json::Value,
) -> bool {
    if !allow_local_webhooks && is_local_webhook(url) {
        warn!(
            url,
            "Blocked local webhook target (set ALLOW_LOCAL_WEBHOOKS=true to enable)"
        );
        return false;
    }

    let mut backoff = Duration::from_millis(200);
    for attempt in 0..3 {
        match client.post(url).json(payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                return true;
            }
            Ok(resp) => {
                let status = resp.status();
                let retryable =
                    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                warn!(url, %status, attempt, "Webhook delivery returned non-success status");
                if !retryable || attempt == 2 {
                    return false;
                }
            }
            Err(e) => {
                warn!(url, error = %e, attempt, "Webhook delivery failed");
                if attempt == 2 {
                    return false;
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }

    false
}

fn is_local_webhook(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host() else {
        return false;
    };

    match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
        }
        url::Host::Ipv6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
                || v6.is_multicast()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_local_and_private_webhook_targets() {
        assert!(is_local_webhook("http://localhost:8080/hook"));
        assert!(is_local_webhook("http://127.0.0.1:8080/hook"));
        assert!(is_local_webhook("http://10.0.0.2/hook"));
        assert!(is_local_webhook("http://192.168.1.10/hook"));
        assert!(is_local_webhook("http://172.16.0.9/hook"));
        assert!(is_local_webhook("http://[::1]/hook"));
        assert!(!is_local_webhook("https://example.com/webhook"));
    }

    #[tokio::test]
    async fn blocks_ssrf_target_when_local_webhooks_disabled() {
        let client = reqwest::Client::new();
        let sent = fire_webhook(
            &client,
            false,
            "http://127.0.0.1:65535/hook",
            &serde_json::json!({"ok": true}),
        )
        .await;
        assert!(!sent);
    }

    #[tokio::test]
    async fn retries_webhook_on_retryable_failures() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/hook")
            .with_status(500)
            .expect(3)
            .create();

        let client = reqwest::Client::new();
        let sent = fire_webhook(
            &client,
            true,
            &format!("{}/hook", server.url()),
            &serde_json::json!({"event": "test"}),
        )
        .await;

        assert!(!sent);
    }
}
