/// file: src/config.rs
/// description: Typed environment configuration with strict fail-fast parsing.
/// Configuration — loaded from environment variables matching the official
/// Firecrawl self-hosting spec (.env template from SELF_HOST.md).
///
/// Variable names are kept 1-to-1 with the official spec so that an existing
/// Firecrawl `.env` file works without modification.
use std::net::SocketAddr;

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub rabbitmq: RabbitMqConfig,
    pub llm: LlmConfig,
    pub crawler: CrawlerConfig,
    pub worker: WorkerConfig,
    pub auth: AuthConfig,
    pub proxy: ProxyConfig,
    pub search: SearchConfig,
    pub resource: ResourceConfig,
}

impl Config {
    /// Load from environment (dotenv + real env).
    ///
    /// This loader is intentionally fail-fast: required settings must be
    /// provided by environment/.env so production deployments are explicit.
    pub fn from_env() -> anyhow::Result<Self> {
        dotenvy::dotenv().ok();

        Ok(Config {
            server: ServerConfig::from_env()?,
            database: DatabaseConfig::from_env()?,
            redis: RedisConfig::from_env()?,
            rabbitmq: RabbitMqConfig::from_env()?,
            llm: LlmConfig::from_env()?,
            crawler: CrawlerConfig::from_env()?,
            worker: WorkerConfig::from_env()?,
            auth: AuthConfig::from_env()?,
            proxy: ProxyConfig::from_env(),
            search: SearchConfig::from_env(),
            resource: ResourceConfig::from_env()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub cors_allow_origins: Vec<String>,
}

impl ServerConfig {
    fn from_env() -> anyhow::Result<Self> {
        let cors_allow_origins = env_opt("CORS_ALLOW_ORIGINS")
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(Self {
            host: env_required_str("HOST")?,
            port: env_required_u16("PORT")?,
            cors_allow_origins,
        })
    }

    pub fn addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(format!("{}:{}", self.host, self.port).parse()?)
    }
}

// ---------------------------------------------------------------------------
// Database (PostgreSQL / TimescaleDB)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub max_connections: usize,
}

impl DatabaseConfig {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            host: env_required_str("POSTGRES_HOST")?,
            port: env_required_u16("POSTGRES_PORT")?,
            database: env_required_str("POSTGRES_DB")?,
            user: env_required_str("POSTGRES_USER")?,
            password: env_required_str("POSTGRES_PASSWORD")?,
            max_connections: env_required_usize("DATABASE_MAX_CONNECTIONS")?,
        })
    }
}

// ---------------------------------------------------------------------------
// Redis
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RedisConfig {
    pub url: String,
    pub rate_limit_url: String,
}

impl RedisConfig {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            url: env_required_str("REDIS_URL")?,
            rate_limit_url: env_required_str("REDIS_RATE_LIMIT_URL")?,
        })
    }
}

// ---------------------------------------------------------------------------
// RabbitMQ
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RabbitMqConfig {
    pub url: String,
}

impl RabbitMqConfig {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            url: env_required_str("NUQ_RABBITMQ_URL")?,
        })
    }
}

// ---------------------------------------------------------------------------
// LLM — OpenAI-compatible (OpenAI, Groq, Ollama, …)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Base URL of the OpenAI-compatible endpoint.
    pub base_url: String,
    pub api_key: String,
    pub model_name: String,
    /// Optional embedding model (used for future semantic features).
    pub embedding_model: Option<String>,
    /// Ollama base URL (alternate path; takes precedence over base_url when set).
    pub ollama_base_url: Option<String>,
    pub timeout_seconds: u64,
    pub max_retries: u32,
    pub max_input_chars: usize,
}

impl LlmConfig {
    fn from_env() -> anyhow::Result<Self> {
        let ollama = env_opt("OLLAMA_BASE_URL");
        let base_url = if let Some(ref o) = ollama {
            o.clone()
        } else {
            env_required_str("OPENAI_BASE_URL")?
        };

        Ok(Self {
            base_url,
            api_key: env_opt("OPENAI_API_KEY").unwrap_or_default(),
            model_name: env_required_str("MODEL_NAME")?,
            embedding_model: env_opt("MODEL_EMBEDDING_NAME"),
            ollama_base_url: ollama,
            timeout_seconds: env_required_u64("LLM_TIMEOUT_SECONDS")?,
            max_retries: env_required_u32("LLM_MAX_RETRIES")?,
            max_input_chars: {
                let v = env_required_usize("LLM_MAX_INPUT_CHARS")?;
                if v == 0 {
                    return Err(anyhow::anyhow!("LLM_MAX_INPUT_CHARS must be > 0"));
                }
                v
            },
        })
    }

    pub fn is_configured(&self) -> bool {
        !self.api_key.is_empty() || self.ollama_base_url.is_some()
    }
}

// ---------------------------------------------------------------------------
// Crawler
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CrawlerConfig {
    pub max_depth: u32,
    pub max_pages: u32,
    pub concurrent_requests: u32,
    pub request_timeout_seconds: u64,
    pub user_agent: String,
    pub respect_robots_txt: bool,
    pub playwright_url: Option<String>,
    pub browser_pool_size: u32,
}

impl CrawlerConfig {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            max_depth: env_required_u32("CRAWLER_MAX_DEPTH")?,
            max_pages: env_required_u32("CRAWLER_MAX_PAGES")?,
            concurrent_requests: env_required_u32("CRAWL_CONCURRENT_REQUESTS")?,
            request_timeout_seconds: env_required_u64("CRAWLER_REQUEST_TIMEOUT_SECONDS")?,
            user_agent: env_required_str("CRAWLER_USER_AGENT")?,
            respect_robots_txt: env_required_bool("CRAWLER_RESPECT_ROBOTS_TXT")?,
            playwright_url: env_opt("PLAYWRIGHT_MICROSERVICE_URL"),
            browser_pool_size: env_required_u32("BROWSER_POOL_SIZE")?,
        })
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub num_workers: u32,
    pub max_concurrent_jobs: u32,
    pub job_timeout_seconds: u64,
    pub max_stalls: u32,
}

impl WorkerConfig {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            num_workers: env_required_u32("NUM_WORKERS_PER_QUEUE")?,
            max_concurrent_jobs: env_required_u32("MAX_CONCURRENT_JOBS")?,
            job_timeout_seconds: env_required_u64("WORKER_JOB_TIMEOUT_SECONDS")?,
            max_stalls: env_required_u32("WORKER_MAX_STALLS")?,
        })
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// When false (default for self-hosted), auth is bypassed.
    pub use_db_authentication: bool,
    /// Static API key for local testing (no Supabase required).
    pub test_api_key: Option<String>,
    /// Key protecting the Bull-compatible queue admin UI.
    pub bull_auth_key: String,
    /// Allow webhooks to localhost (useful for dev).
    pub allow_local_webhooks: bool,
}

impl AuthConfig {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            use_db_authentication: env_required_bool("USE_DB_AUTHENTICATION")?,
            test_api_key: env_opt("TEST_API_KEY"),
            bull_auth_key: env_required_str("BULL_AUTH_KEY")?,
            allow_local_webhooks: env_required_bool("ALLOW_LOCAL_WEBHOOKS")?,
        })
    }
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub server: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyConfig {
    fn from_env() -> Self {
        Self {
            server: env_opt("PROXY_SERVER"),
            username: env_opt("PROXY_USERNAME"),
            password: env_opt("PROXY_PASSWORD"),
        }
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// SearXNG JSON endpoint (optional; falls back to direct Google SERP scraping).
    pub searxng_endpoint: Option<String>,
    pub searxng_engines: Option<String>,
    pub searxng_categories: Option<String>,
}

impl SearchConfig {
    fn from_env() -> Self {
        Self {
            searxng_endpoint: env_opt("SEARXNG_ENDPOINT"),
            searxng_engines: env_opt("SEARXNG_ENGINES"),
            searxng_categories: env_opt("SEARXNG_CATEGORIES"),
        }
    }
}

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ResourceConfig {
    /// Reject new jobs when CPU fraction exceeds this (0.0–1.0).
    pub max_cpu: f64,
    /// Reject new jobs when RAM fraction exceeds this (0.0–1.0).
    pub max_ram: f64,
}

impl ResourceConfig {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            max_cpu: env_required_f64("MAX_CPU")?,
            max_ram: env_required_f64("MAX_RAM")?,
        })
    }
}

// ---------------------------------------------------------------------------
// Env-reading helpers
// ---------------------------------------------------------------------------

fn env_required_str(key: &str) -> anyhow::Result<String> {
    std::env::var(key).map_err(|_| anyhow::anyhow!("Missing required env var: {key}"))
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_required_u16(key: &str) -> anyhow::Result<u16> {
    let raw = env_required_str(key)?;
    raw.parse()
        .map_err(|_| anyhow::anyhow!("Invalid u16 value for env var {key}: {raw}"))
}

fn env_required_u32(key: &str) -> anyhow::Result<u32> {
    let raw = env_required_str(key)?;
    raw.parse()
        .map_err(|_| anyhow::anyhow!("Invalid u32 value for env var {key}: {raw}"))
}

fn env_required_u64(key: &str) -> anyhow::Result<u64> {
    let raw = env_required_str(key)?;
    raw.parse()
        .map_err(|_| anyhow::anyhow!("Invalid u64 value for env var {key}: {raw}"))
}

fn env_required_usize(key: &str) -> anyhow::Result<usize> {
    let raw = env_required_str(key)?;
    raw.parse()
        .map_err(|_| anyhow::anyhow!("Invalid usize value for env var {key}: {raw}"))
}

fn env_required_bool(key: &str) -> anyhow::Result<bool> {
    match std::env::var(key).as_deref() {
        Ok("true" | "1" | "yes") => Ok(true),
        Ok("false" | "0" | "no") => Ok(false),
        Ok(other) => Err(anyhow::anyhow!(
            "Invalid bool value for env var {key}: {other} (expected true/false, 1/0, yes/no)"
        )),
        Err(_) => Err(anyhow::anyhow!("Missing required env var: {key}")),
    }
}

fn env_required_f64(key: &str) -> anyhow::Result<f64> {
    let raw = env_required_str(key)?;
    raw.parse()
        .map_err(|_| anyhow::anyhow!("Invalid f64 value for env var {key}: {raw}"))
}
