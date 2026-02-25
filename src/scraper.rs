/// file: src/scraper.rs
/// description: HTTP/browser scraping, crawl traversal, and HTML processing.
/// Scraper — HTTP fetch, headless browser pool, BFS crawl, robots.txt,
/// sitemap parsing, and HTML processing utilities.
use crate::config::{CrawlerConfig, ProxyConfig};
use crate::models::{
    AppError, OutputFormat, ScrapeJobData, ScrapeMetadata, ScrapeOptions, ScrapeResult,
};
use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;
use reqwest::Client;
use robotstxt::DefaultMatcher;
use scraper::{Html, Selector};
use sitemap::reader::{SiteMapEntity, SiteMapReader};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, warn};
use url::Url;

// ---------------------------------------------------------------------------
// Browser pool
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BrowserPool {
    inner: Arc<Mutex<Option<Arc<Browser>>>>,
    config: Arc<CrawlerConfig>,
}

impl BrowserPool {
    pub fn new(config: CrawlerConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            config: Arc::new(config),
        }
    }

    async fn get_or_launch(&self) -> Result<Arc<Browser>, AppError> {
        let mut guard = self.inner.lock().await;
        if let Some(ref b) = *guard {
            return Ok(Arc::clone(b));
        }

        let builder = BrowserConfig::builder()
            .arg("--no-sandbox")
            .arg("--disable-setuid-sandbox")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-gpu")
            .arg(format!("--user-agent={}", self.config.user_agent));

        let browser_cfg = builder
            .build()
            .map_err(|e| AppError::Scraper(format!("Browser config error: {e}")))?;

        let (browser, mut handler) = Browser::launch(browser_cfg)
            .await
            .map_err(|e| AppError::Scraper(format!("Browser launch failed: {e}")))?;

        tokio::spawn(async move { while handler.next().await.is_some() {} });

        let shared = Arc::new(browser);
        *guard = Some(Arc::clone(&shared));
        Ok(shared)
    }

    pub async fn shutdown(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(browser) = guard.take()
            && let Ok(mut b) = Arc::try_unwrap(browser)
            && let Err(e) = b.close().await
        {
            warn!(error = %e, "Browser close error");
        }
    }

    pub async fn fetch_page(
        &self,
        url: &str,
        wait_for_ms: u32,
        timeout: Duration,
    ) -> Result<(String, Option<String>), AppError> {
        let browser = self.get_or_launch().await?;

        let page = tokio::time::timeout(timeout, browser.new_page(url))
            .await
            .map_err(|_| AppError::Timeout)?
            .map_err(|e| AppError::Scraper(format!("Failed to open page: {e}")))?;

        // Wait for navigation (best-effort).
        let _ = tokio::time::timeout(Duration::from_secs(10), page.wait_for_navigation()).await;

        // Additional wait if requested.
        if wait_for_ms > 0 {
            tokio::time::sleep(Duration::from_millis(wait_for_ms as u64)).await;
        }

        let html = tokio::time::timeout(timeout, page.content())
            .await
            .map_err(|_| AppError::Timeout)?
            .map_err(|e| AppError::Scraper(format!("Failed to get content: {e}")))?;

        let title = page.get_title().await.ok().flatten();

        if let Err(e) = page.close().await {
            debug!(url, error = %e, "Page close error (tab leak)");
        }

        Ok((html, title))
    }
}

// ---------------------------------------------------------------------------
// HTTP scraper
// ---------------------------------------------------------------------------

pub struct HttpScraper {
    client: Client,
    config: Arc<CrawlerConfig>,
    robots_cache: Mutex<HashMap<String, String>>,
}

impl HttpScraper {
    pub fn new(config: &CrawlerConfig, proxy: &ProxyConfig) -> Result<Self, AppError> {
        let mut builder = Client::builder()
            .user_agent(&config.user_agent)
            .timeout(Duration::from_secs(config.request_timeout_seconds));

        if let Some(server) = &proxy.server {
            let mut px = reqwest::Proxy::all(server)
                .map_err(|e| AppError::Scraper(format!("Proxy config error: {e}")))?;
            if let Some(username) = &proxy.username {
                px = px.basic_auth(username, proxy.password.as_deref().unwrap_or(""));
            }
            builder = builder.proxy(px);
        }

        let client = builder
            .build()
            .map_err(|e| AppError::Scraper(format!("HTTP client build failed: {e}")))?;

        Ok(Self {
            client,
            config: Arc::new(config.clone()),
            robots_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Returns `(status_code, content_type, body)`.
    pub async fn fetch(&self, url: &str) -> Result<(u16, Option<String>, String), AppError> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| AppError::Scraper(format!("HTTP request failed: {e}")))?;

        let status = resp.status().as_u16();
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_owned());

        let body = resp
            .text()
            .await
            .map_err(|e| AppError::Scraper(format!("Failed to read body: {e}")))?;

        Ok((status, ct, body))
    }

    pub async fn is_allowed_by_robots(&self, url: &Url) -> bool {
        if !self.config.respect_robots_txt {
            return true;
        }
        let host = url.host_str().unwrap_or("").to_owned();
        let robots_url = format!("{}://{}/robots.txt", url.scheme(), host);

        {
            let cache = self.robots_cache.lock().await;
            if let Some(text) = cache.get(&host) {
                let mut m = DefaultMatcher::default();
                return m.one_agent_allowed_by_robots(text, &self.config.user_agent, url.as_str());
            }
        }

        let text = match self.client.get(&robots_url).send().await {
            Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
            _ => return true,
        };

        let mut cache = self.robots_cache.lock().await;
        // Evict oldest entries when the cache exceeds 1 000 hosts to prevent
        // unbounded memory growth during large crawls.
        if cache.len() >= 1_000
            && let Some(key) = cache.keys().next().cloned()
        {
            cache.remove(&key);
        }
        cache.insert(host.clone(), text.clone());
        drop(cache);

        let mut m = DefaultMatcher::default();
        m.one_agent_allowed_by_robots(&text, &self.config.user_agent, url.as_str())
    }

    pub async fn fetch_sitemap_urls(&self, base_url: &Url, limit: usize) -> Vec<String> {
        let sitemap_url = format!(
            "{}://{}/sitemap.xml",
            base_url.scheme(),
            base_url.host_str().unwrap_or(""),
        );
        let mut collected = Vec::new();
        let mut visited = HashSet::new();
        self.collect_sitemap_urls(&sitemap_url, limit, &mut collected, &mut visited)
            .await;
        collected
    }

    async fn collect_sitemap_urls(
        &self,
        url: &str,
        limit: usize,
        collected: &mut Vec<String>,
        visited: &mut HashSet<String>,
    ) {
        if collected.len() >= limit || !visited.insert(url.to_string()) {
            return;
        }
        let body = match self.client.get(url).send().await {
            Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
            _ => return,
        };
        let cursor = Cursor::new(body.into_bytes());
        let parser = SiteMapReader::new(cursor);
        let mut child_sitemaps = Vec::new();
        for entity in parser {
            if collected.len() >= limit {
                break;
            }
            match entity {
                SiteMapEntity::Url(url_entry) => {
                    if let Some(loc) = url_entry.loc.get_url() {
                        collected.push(loc.to_string());
                    }
                }
                SiteMapEntity::SiteMap(sm) => {
                    if let Some(loc) = sm.loc.get_url() {
                        child_sitemaps.push(loc.to_string());
                    }
                }
                SiteMapEntity::Err(_) => {}
            }
        }
        for child in child_sitemaps {
            if collected.len() >= limit {
                break;
            }
            Box::pin(self.collect_sitemap_urls(&child, limit, collected, visited)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// HTML helpers
// ---------------------------------------------------------------------------

pub fn extract_title(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("title").ok()?;
    doc.select(&sel)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_owned())
}

pub fn html_to_markdown(html: &str) -> String {
    htmd::convert(html).unwrap_or_default()
}

pub fn html_to_text(html: &str) -> String {
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse("body") else {
        return String::new();
    };
    doc.select(&sel)
        .next()
        .map(|body| {
            body.text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

pub fn extract_links(html: &str, base_url: &Url) -> Vec<String> {
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse("a[href]") else {
        return Vec::new();
    };
    let mut links = Vec::new();
    for el in doc.select(&sel) {
        if let Some(href) = el.value().attr("href")
            && let Ok(resolved) = base_url.join(href)
            && matches!(resolved.scheme(), "http" | "https")
        {
            links.push(resolved.to_string());
        }
    }
    links
}

/// Strip HTML tags and reduce to main content using a simple heuristic:
/// prefer `<article>`, `<main>`, `<div role="main">` before falling back
/// to `<body>`.
pub fn extract_main_content(html: &str) -> String {
    let doc = Html::parse_document(html);
    let candidates = ["article", "main", "[role='main']", "body"];
    for selector_str in candidates {
        if let Ok(sel) = Selector::parse(selector_str)
            && let Some(el) = doc.select(&sel).next()
        {
            let text = el
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Top-level Scraper (combines HTTP + browser)
// ---------------------------------------------------------------------------

pub struct Scraper {
    pub http: HttpScraper,
    pub browser: BrowserPool,
    config: Arc<CrawlerConfig>,
}

impl Scraper {
    pub fn new(cfg: &CrawlerConfig, proxy: &ProxyConfig) -> Result<Self, AppError> {
        debug!(
            concurrent_requests = cfg.concurrent_requests,
            browser_pool_size = cfg.browser_pool_size,
            "Crawler runtime settings"
        );

        Ok(Self {
            http: HttpScraper::new(cfg, proxy)?,
            browser: BrowserPool::new(cfg.clone()),
            config: Arc::new(cfg.clone()),
        })
    }

    pub async fn shutdown(&self) {
        self.browser.shutdown().await;
    }

    /// Scrape a single URL and return a `ScrapeResult`.
    pub async fn scrape(&self, job: &ScrapeJobData) -> Result<ScrapeResult, AppError> {
        let _start = Instant::now();
        let parsed = Url::parse(&job.url).map_err(AppError::InvalidUrl)?;

        // Robots check.
        if !self.http.is_allowed_by_robots(&parsed).await {
            return Err(AppError::Scraper(format!(
                "URL blocked by robots.txt: {}",
                job.url
            )));
        }

        let timeout = Duration::from_secs(
            job.timeout
                .map(|t| t as u64)
                .unwrap_or(self.config.request_timeout_seconds),
        );

        let use_browser = !job.actions.is_empty() || job.wait_for > 0;

        let (html, title, status_code, content_type) =
            if use_browser && self.config.playwright_url.is_none() {
                let (html, browser_title) = self
                    .browser
                    .fetch_page(&job.url, job.wait_for, timeout)
                    .await?;
                let title = browser_title.or_else(|| extract_title(&html));
                (html, title, 200u16, Some("text/html".to_string()))
            } else {
                let (status, ct, body) = self.http.fetch(&job.url).await?;
                let title = extract_title(&body);
                (body, title, status, ct)
            };

        let formats = if job.formats.is_empty() {
            vec![OutputFormat::Markdown]
        } else {
            job.formats.clone()
        };

        let mut result = ScrapeResult {
            url: job.url.clone(),
            title,
            markdown: None,
            html: None,
            raw_html: None,
            screenshot: None,
            links: None,
            extract: None,
            json: None,
            warning: None,
            metadata: ScrapeMetadata {
                status_code,
                content_type: content_type.clone(),
                source_url: job.url.clone(),
                scrape_id: None,
                error: None,
            },
        };

        for fmt in &formats {
            match fmt {
                OutputFormat::Markdown => {
                    result.markdown = Some(html_to_markdown(&html));
                }
                OutputFormat::Html => {
                    let main = if job.only_main_content {
                        extract_main_content(&html)
                    } else {
                        html_to_text(&html)
                    };
                    result.html = Some(main);
                }
                OutputFormat::RawHtml => {
                    result.raw_html = Some(html.clone());
                }
                OutputFormat::Links => {
                    result.links = Some(extract_links(&html, &parsed));
                }
                OutputFormat::Json => {
                    result.json = Some(html_to_structured_json(&html, &parsed));
                }
                OutputFormat::Extract => {
                    // LLM extraction — handled by the caller (worker) after scrape.
                }
                OutputFormat::Screenshot => {
                    // Screenshot via browser — not yet implemented.
                    debug!("Screenshot format requested but not yet implemented");
                }
            }
        }

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Crawl parameters (groups the BFS tuning knobs to avoid too-many-args lint)
// ---------------------------------------------------------------------------

pub struct CrawlParams<'a> {
    pub root_url: String,
    pub max_depth: u32,
    pub limit: u32,
    pub scrape_options: &'a ScrapeOptions,
    pub allow_external_links: bool,
    pub ignore_sitemap: bool,
    pub include_paths: &'a [String],
    pub exclude_paths: &'a [String],
}

impl Scraper {
    /// BFS crawl starting from `root_url`.
    /// Returns all scraped results up to `limit` pages at most `max_depth` deep.
    pub async fn crawl(&self, params: CrawlParams<'_>) -> Result<Vec<ScrapeResult>, AppError> {
        let CrawlParams {
            root_url,
            max_depth,
            limit,
            scrape_options: options,
            allow_external_links,
            ignore_sitemap,
            include_paths,
            exclude_paths,
        } = params;
        let base = Url::parse(&root_url).map_err(AppError::InvalidUrl)?;
        let base_host = base.host_str().unwrap_or("").to_owned();

        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        let mut results: Vec<ScrapeResult> = Vec::new();

        // Seed from sitemap if available.
        if !ignore_sitemap {
            let sitemap_urls = self.http.fetch_sitemap_urls(&base, limit as usize).await;
            for u in sitemap_urls {
                if visited.insert(u.clone()) {
                    queue.push_back((u, 1));
                }
            }
        }

        // Always include root.
        if visited.insert(root_url.clone()) {
            queue.push_front((root_url, 0));
        }

        let formats = if options.formats.is_empty() {
            vec![OutputFormat::Markdown]
        } else {
            options.formats.clone()
        };

        while let Some((url, depth)) = queue.pop_front() {
            if results.len() >= limit as usize {
                break;
            }

            // Path filter.
            if !include_paths.is_empty() && !include_paths.iter().any(|p| url.contains(p.as_str()))
            {
                continue;
            }
            if exclude_paths.iter().any(|p| url.contains(p.as_str())) {
                continue;
            }

            // Host filter.
            if !allow_external_links
                && let Ok(parsed) = Url::parse(&url)
                && parsed.host_str().unwrap_or("") != base_host
            {
                continue;
            }

            let job = ScrapeJobData {
                url: url.clone(),
                formats: formats.clone(),
                only_main_content: options.only_main_content,
                include_tags: options.include_tags.clone(),
                exclude_tags: options.exclude_tags.clone(),
                headers: options.headers.clone(),
                wait_for: options.wait_for,
                mobile: options.mobile,
                timeout: options.timeout,
                extract: None,
                actions: options.actions.clone(),
                location: options.location.clone(),
                webhook: None,
            };

            // Always fetch raw HTML so BFS can discover child links without a
            // second HTTP round-trip.  We strip it from the result before
            // pushing unless the caller explicitly requested it.
            let raw_html_requested = formats.contains(&OutputFormat::RawHtml);
            let mut bfs_job = job.clone();
            if depth < max_depth && !bfs_job.formats.contains(&OutputFormat::RawHtml) {
                bfs_job.formats.push(OutputFormat::RawHtml);
            }

            match self.scrape(&bfs_job).await {
                Ok(mut result) => {
                    // Discover child links for next BFS level using the raw HTML
                    // we already have — no second fetch needed.
                    if depth < max_depth
                        && let Some(raw) = &result.raw_html
                        && let Ok(parsed) = Url::parse(&url)
                    {
                        for link in extract_links(raw, &parsed) {
                            if visited.insert(link.clone()) {
                                queue.push_back((link, depth + 1));
                            }
                        }
                    }
                    // Strip rawHtml unless the caller requested it.
                    if !raw_html_requested {
                        result.raw_html = None;
                    }
                    results.push(result);
                }
                Err(e) => {
                    warn!(url, error = %e, "Crawl: page scrape failed (skipping)");
                }
            }
        }

        Ok(results)
    }

    /// URL discovery (for /v2/map) — returns all reachable URLs without
    /// scraping content.
    pub async fn map_urls(
        &self,
        root_url: &str,
        search: Option<&str>,
        ignore_sitemap: bool,
        include_subdomains: bool,
        limit: u32,
    ) -> Result<Vec<String>, AppError> {
        let base = Url::parse(root_url).map_err(AppError::InvalidUrl)?;
        let base_host = base.host_str().unwrap_or("").to_owned();

        let mut found: HashSet<String> = HashSet::new();
        let mut urls: Vec<String> = Vec::new();

        // Start with sitemap.
        if !ignore_sitemap {
            let sitemap_urls = self.http.fetch_sitemap_urls(&base, limit as usize).await;
            for u in sitemap_urls {
                if found.insert(u.clone()) {
                    urls.push(u);
                }
            }
        }

        // Also crawl root for links.
        if let Ok((_, _, body)) = self.http.fetch(root_url).await {
            for link in extract_links(&body, &base) {
                if let Ok(parsed) = Url::parse(&link) {
                    let host = parsed.host_str().unwrap_or("");
                    let host_ok = if include_subdomains {
                        host == base_host || host.ends_with(&format!(".{base_host}"))
                    } else {
                        host == base_host
                    };
                    if host_ok && found.insert(link.clone()) {
                        urls.push(link);
                        if urls.len() >= limit as usize {
                            break;
                        }
                    }
                }
            }
        }

        // Apply search filter.
        if let Some(q) = search {
            let q_lower = q.to_lowercase();
            urls.retain(|u| u.to_lowercase().contains(&q_lower));
        }

        urls.truncate(limit as usize);
        Ok(urls)
    }
}

fn html_to_structured_json(html: &str, base_url: &Url) -> serde_json::Value {
    fn text_of_first(doc: &Html, selector: &str) -> Option<String> {
        let sel = Selector::parse(selector).ok()?;
        let text = doc
            .select(&sel)
            .next()?
            .text()
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() { None } else { Some(text) }
    }

    fn first_meta_content(doc: &Html, selectors: &[&str]) -> Option<String> {
        for selector in selectors {
            let Some(sel) = Selector::parse(selector).ok() else {
                continue;
            };
            if let Some(content) = doc
                .select(&sel)
                .next()
                .and_then(|n| n.value().attr("content"))
            {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }

    fn collect_text(doc: &Html, selector: &str, max_items: usize) -> Vec<String> {
        let Some(sel) = Selector::parse(selector).ok() else {
            return Vec::new();
        };
        doc.select(&sel)
            .filter_map(|el| {
                let text = el
                    .text()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if text.is_empty() { None } else { Some(text) }
            })
            .take(max_items)
            .collect()
    }

    let doc = Html::parse_document(html);
    let title = text_of_first(&doc, "title");
    let description = first_meta_content(
        &doc,
        &[
            "meta[name='description']",
            "meta[property='og:description']",
            "meta[name='twitter:description']",
        ],
    );

    let h1 = collect_text(&doc, "h1", 10);
    let h2 = collect_text(&doc, "h2", 20);
    let h3 = collect_text(&doc, "h3", 30);
    let paragraphs = collect_text(&doc, "p", 30);
    let links = extract_links(html, base_url)
        .into_iter()
        .take(200)
        .collect::<Vec<_>>();

    let main_text = extract_main_content(html);
    let text_preview = main_text.chars().take(3000).collect::<String>();

    serde_json::json!({
        "title": title,
        "description": description,
        "headings": {
            "h1": h1,
            "h2": h2,
            "h3": h3,
        },
        "paragraphs": paragraphs,
        "links": links,
        "text": text_preview,
    })
}
