/// file: src/models.rs
/// description: Domain models, request/response schemas, and AppError mapping.
/// Domain types, API shapes, and DB row types for fire_ctrl.
///
/// API shapes are designed to be wire-compatible with the official Firecrawl
/// v2 REST API so that any SDK (including the official Rust SDK configured
/// with `new_selfhosted`) works without modification.
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum AppError {
    #[error("database pool error: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),

    #[error("postgres error: {0}")]
    Postgres(#[from] tokio_postgres::Error),

    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),

    #[error("scraper error: {0}")]
    Scraper(String),

    #[error("LLM error: {0}")]
    Llm(#[from] anyhow::Error),

    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("timeout")]
    Timeout,

    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),

    #[error("bad request: {0}")]
    BadRequest(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            AppError::InvalidUrl(_) | AppError::Serialization(_) | AppError::BadRequest(_) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            AppError::Timeout => (StatusCode::GATEWAY_TIMEOUT, self.to_string()),
            AppError::ResourceLimit(_) => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };

        #[derive(Serialize)]
        struct Body {
            success: bool,
            error: String,
        }

        (
            status,
            Json(Body {
                success: false,
                error: message,
            }),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// Status enums — mirror PostgreSQL ENUMs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Scraping, // used for in-progress crawl pages (matches official API)
    Active,
    Completed,
    Failed,
    Cancelled,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::Queued => write!(f, "queued"),
            JobStatus::Scraping => write!(f, "scraping"),
            JobStatus::Active => write!(f, "active"),
            JobStatus::Completed => write!(f, "completed"),
            JobStatus::Failed => write!(f, "failed"),
            JobStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupStatus {
    Active,
    Completed,
    Cancelled,
}

impl std::fmt::Display for GroupStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupStatus::Active => write!(f, "active"),
            GroupStatus::Completed => write!(f, "completed"),
            GroupStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

// ---------------------------------------------------------------------------
// Output formats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputFormat {
    Markdown,
    Html,
    RawHtml,
    Screenshot,
    Links,
    Extract,
    Json,
}

// ---------------------------------------------------------------------------
// Job envelope — JSONB discriminated union stored in queue_scrape.data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "lowercase")]
pub enum JobEnvelope {
    Scrape(ScrapeJobData),
    Crawl(CrawlJobData),
    BatchScrape(BatchScrapeJobData),
    Extract(ExtractJobData),
    Map(MapJobData),
}

// ---------------------------------------------------------------------------
// Scrape job
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrapeJobData {
    pub url: String,
    #[serde(default)]
    pub formats: Vec<OutputFormat>,
    #[serde(default)]
    pub only_main_content: bool,
    #[serde(default)]
    pub include_tags: Vec<String>,
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
    #[serde(default)]
    pub wait_for: u32,
    #[serde(default)]
    pub mobile: bool,
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(default)]
    pub extract: Option<ExtractOptions>,
    #[serde(default)]
    pub actions: Vec<PageAction>,
    #[serde(default)]
    pub location: Option<LocationOptions>,
    /// Webhook URL to POST result to on completion.
    #[serde(default)]
    pub webhook: Option<String>,
}

// ---------------------------------------------------------------------------
// Crawl job
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlJobData {
    pub url: String,
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_max_pages")]
    pub limit: u32,
    #[serde(default)]
    pub scrape_options: ScrapeOptions,
    #[serde(default)]
    pub allow_backward_links: bool,
    #[serde(default)]
    pub allow_external_links: bool,
    #[serde(default)]
    pub ignore_sitemap: bool,
    #[serde(default)]
    pub include_paths: Vec<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
    pub priority: i32,
    pub ttl_ms: i64,
    /// The crawl group ID that the API handler already created and exposed.
    /// The worker must use this ID (not create a new group) to avoid orphaning.
    #[serde(default)]
    pub group_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Batch scrape job
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchScrapeJobData {
    pub urls: Vec<String>,
    #[serde(default)]
    pub scrape_options: ScrapeOptions,
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
    pub priority: i32,
}

// ---------------------------------------------------------------------------
// Extract job (/v2/extract — LLM extraction across multiple URLs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractJobData {
    pub urls: Vec<String>,
    pub prompt: Option<String>,
    pub schema: Option<serde_json::Value>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub allow_external_links: bool,
    #[serde(default)]
    pub enable_web_search: bool,
    #[serde(default)]
    pub ignore_invalid_urls: bool,
}

// ---------------------------------------------------------------------------
// Map job (/v2/map — URL discovery)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapJobData {
    pub url: String,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub ignore_sitemap: bool,
    #[serde(default)]
    pub include_subdomains: bool,
    #[serde(default = "default_map_limit")]
    pub limit: u32,
}

// ---------------------------------------------------------------------------
// Shared sub-types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScrapeOptions {
    #[serde(default)]
    pub formats: Vec<OutputFormat>,
    #[serde(default)]
    pub only_main_content: bool,
    #[serde(default)]
    pub include_tags: Vec<String>,
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
    #[serde(default)]
    pub wait_for: u32,
    #[serde(default)]
    pub mobile: bool,
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(default)]
    pub actions: Vec<PageAction>,
    #[serde(default)]
    pub location: Option<LocationOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractOptions {
    pub prompt: Option<String>,
    pub schema: Option<serde_json::Value>,
    #[serde(default)]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PageAction {
    Wait {
        milliseconds: u32,
    },
    Click {
        selector: String,
    },
    Screenshot,
    Scroll {
        direction: String,
        amount: Option<u32>,
    },
    Write {
        text: String,
        selector: String,
    },
    Press {
        key: String,
    },
    ExecuteJavascript {
        script: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationOptions {
    pub country: Option<String>,
    pub languages: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
    /// Events to send: "completed", "failed", "page", "started"
    #[serde(default)]
    pub events: Vec<String>,
}

// ---------------------------------------------------------------------------
// Scrape result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrapeResult {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(rename = "rawHtml", skip_serializing_if = "Option::is_none")]
    pub raw_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extract: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub metadata: ScrapeMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrapeMetadata {
    #[serde(rename = "statusCode")]
    pub status_code: u16,
    #[serde(rename = "contentType", skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(rename = "sourceURL")]
    pub source_url: String,
    #[serde(rename = "scrapeId", skip_serializing_if = "Option::is_none")]
    pub scrape_id: Option<Uuid>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// DB row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct QueueScrapeRow {
    pub id: Uuid,
    pub status: JobStatus,
    pub data: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub returnvalue: Option<serde_json::Value>,
    pub failedreason: Option<String>,
    pub group_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct GroupCrawlRow {
    pub id: Uuid,
    pub status: GroupStatus,
    pub created_at: DateTime<Utc>,
    pub owner_id: Uuid,
    pub ttl: i64,
    pub expires_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// HTTP request shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ScrapeRequest {
    pub url: String,
    #[serde(default)]
    pub formats: Vec<OutputFormat>,
    #[serde(default)]
    pub only_main_content: bool,
    #[serde(default)]
    pub include_tags: Vec<String>,
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
    #[serde(default)]
    pub wait_for: u32,
    #[serde(default)]
    pub mobile: bool,
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(default)]
    pub extract: Option<ExtractOptions>,
    #[serde(default)]
    pub actions: Vec<PageAction>,
    #[serde(default)]
    pub location: Option<LocationOptions>,
    #[serde(default)]
    pub webhook: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CrawlRequest {
    pub url: String,
    #[serde(rename = "maxDepth", default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_max_pages")]
    pub limit: u32,
    #[serde(rename = "scrapeOptions", default)]
    pub scrape_options: ScrapeOptions,
    #[serde(rename = "allowBackwardLinks", default)]
    pub allow_backward_links: bool,
    #[serde(rename = "allowExternalLinks", default)]
    pub allow_external_links: bool,
    #[serde(rename = "ignoreSitemap", default)]
    pub ignore_sitemap: bool,
    #[serde(rename = "includePaths", default)]
    pub include_paths: Vec<String>,
    #[serde(rename = "excludePaths", default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchScrapeRequest {
    pub urls: Vec<String>,
    #[serde(rename = "scrapeOptions", default)]
    pub scrape_options: ScrapeOptions,
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MapRequest {
    pub url: String,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(rename = "ignoreSitemap", default)]
    pub ignore_sitemap: bool,
    #[serde(rename = "includeSubdomains", default)]
    pub include_subdomains: bool,
    #[serde(default = "default_map_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtractRequest {
    pub urls: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
    #[serde(rename = "systemPrompt", default)]
    pub system_prompt: Option<String>,
    #[serde(rename = "allowExternalLinks", default)]
    pub allow_external_links: bool,
    #[serde(rename = "enableWebSearch", default)]
    pub enable_web_search: bool,
    #[serde(rename = "ignoreInvalidUrls", default)]
    pub ignore_invalid_urls: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(rename = "scrapeOptions", default)]
    pub scrape_options: Option<ScrapeOptions>,
    #[serde(default)]
    pub timeout: Option<u32>,
}

// ---------------------------------------------------------------------------
// HTTP response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ScrapeResponse {
    pub success: bool,
    pub data: ScrapeResult,
}

#[derive(Debug, Serialize)]
pub struct CrawlResponse {
    pub success: bool,
    pub id: Uuid,
    pub url: String, // polling URL e.g. /v2/crawl/{id}
}

#[derive(Debug, Serialize)]
pub struct CrawlStatusResponse {
    pub status: JobStatus,
    /// Total pages discovered so far (or at completion).
    pub total: u32,
    /// Pages successfully scraped.
    pub completed: u32,
    /// Pages that failed.
    pub failed: u32,
    #[serde(rename = "creditsUsed")]
    pub credits_used: u32,
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    pub data: Vec<ScrapeResult>,
}

#[derive(Debug, Serialize)]
pub struct BatchScrapeResponse {
    pub success: bool,
    pub id: Uuid,
    pub url: String,
    #[serde(rename = "invalidUrls")]
    pub invalid_urls: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MapResponse {
    pub success: bool,
    pub links: Vec<SearchWebResult>,
}

#[derive(Debug, Serialize)]
pub struct ExtractResponse {
    pub success: bool,
    pub id: Uuid,
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct ExtractStatusResponse {
    pub status: JobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub success: bool,
    pub data: SearchResponseData,
}

#[derive(Debug, Serialize, Default)]
pub struct SearchResponseData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web: Option<Vec<SearchWebResult>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub news: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize, Clone)]
pub struct SearchWebResult {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub services: ServiceHealth,
}

#[derive(Debug, Serialize)]
pub struct ServiceHealth {
    pub database: ComponentStatus,
    pub redis: ComponentStatus,
    pub rabbitmq: ComponentStatus,
    pub llm: ComponentStatus,
}

#[derive(Debug, Serialize)]
pub struct ComponentStatus {
    pub healthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

fn default_max_depth() -> u32 {
    5
}
fn default_max_pages() -> u32 {
    100
}
fn default_map_limit() -> u32 {
    5000
}
fn default_search_limit() -> u32 {
    5
}
