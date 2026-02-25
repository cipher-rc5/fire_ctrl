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

        // Stall-recovery ticker.
        {
            let db2 = Arc::clone(&db);
            let cfg2 = Arc::clone(&cfg);
            tokio::spawn(async move {
                stall_recovery_loop(db2, cfg2).await;
            });
        }

        // Worker tasks.
        let mut set: JoinSet<()> = JoinSet::new();
        for id in 0..cfg.num_workers {
            let w = Worker {
                id,
                lock_id: Uuid::new_v4(),
                db: Arc::clone(&db),
                scraper: Arc::clone(&scraper),
                llm: Arc::clone(&llm),
                cfg: Arc::clone(&cfg),
                sem: Arc::clone(&sem),
                allow_local_webhooks: self.allow_local_webhooks,
                webhook_client: Arc::clone(&webhook_client),
            };
            set.spawn(async move { w.run().await });
        }

        // Monitor; respawn crashed workers.
        while let Some(res) = set.join_next().await {
            let new_id = set.len() as u32;
            if let Err(e) = res {
                warn!(error = %e, "Worker task panicked — respawning");
            }
            let w = Worker {
                id: new_id,
                lock_id: Uuid::new_v4(),
                db: Arc::clone(&db),
                scraper: Arc::clone(&scraper),
                llm: Arc::clone(&llm),
                cfg: Arc::clone(&cfg),
                sem: Arc::clone(&sem),
                allow_local_webhooks: self.allow_local_webhooks,
                webhook_client: Arc::clone(&webhook_client),
            };
            set.spawn(async move { w.run().await });
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Stall recovery loop
// ---------------------------------------------------------------------------

async fn stall_recovery_loop(db: Arc<DbClient>, cfg: Arc<WorkerConfig>) {
    let stale_after = Duration::from_secs(cfg.job_timeout_seconds + 30);
    let mut backoff = Duration::from_secs(2);
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        match db.requeue_stalled_jobs(stale_after).await {
            Ok(n) if n > 0 => info!(count = n, "Re-queued stalled jobs"),
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "Stall-recovery requeue failed; backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(120));
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
        backoff = Duration::from_secs(2); // reset on success
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
            let permit = self.sem.clone().acquire_owned().await;

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
                // Map jobs complete synchronously inside the API handler, so
                // nothing should arrive here.  If one does, no-op it.
                warn!(%job_id, "Map job arrived in worker queue (unexpected)");
                let rv = serde_json::json!({ "links": [] });
                self.db.complete_job(job_id, created_at, &rv).await
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
            match self.scraper.scrape(&scrape_job).await {
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
            if let Ok(result) = self.scraper.scrape(&scrape_job).await {
                if let Some(md) = result.markdown {
                    combined_content.push_str(&format!("## {url}\n\n{md}\n\n"));
                }
            } else if !job.ignore_invalid_urls {
                warn!(url, "Extract: failed to scrape URL");
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
