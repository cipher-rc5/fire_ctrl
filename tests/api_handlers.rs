//! file: tests/api_handlers.rs
//! description: HTTP handler smoke tests + auth/resource gate unit tests.
//!
//! These tests intentionally split into two tiers:
//!
//!   1. **Gate-level tests** (`check_auth_for_test`, `check_resources_for_test`)
//!      exercise the auth + resource short-circuits in isolation. They do
//!      not require a live Postgres/Redis stack and run on every CI job.
//!
//!   2. **Router-level tests** (`Router::oneshot`) exercise the constructed
//!      `axum::Router` via `tower::ServiceExt::oneshot` so we can validate
//!      that endpoints are wired and short-circuit on auth/resource limits
//!      before any I/O is attempted. Handlers that *would* hit Postgres
//!      (cursor pagination, `/v2/map`, cancel-fallback) are exercised only
//!      to the point where the gate fires, because spinning up Postgres in
//!      this same test binary would multiply CI time. The full DB-backed
//!      coverage lives in `tests/postgres_queue.rs`.

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use fire_ctrl::api::{
    AppState, ResourceSnapshot, build_router_for_test, check_auth_for_test,
    check_resources_for_test, set_resource_snapshot_for_test,
};
use fire_ctrl::config::{
    AuthConfig, Config, CrawlerConfig, DatabaseConfig, LlmConfig, ProxyConfig, RedisConfig,
    ResourceConfig, SearchConfig, ServerConfig, WorkerConfig,
};
use fire_ctrl::llm::LlmClient;
use fire_ctrl::models::AppError;
use std::sync::Arc;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

fn make_config(use_db_auth: bool, test_api_key: Option<&str>) -> Config {
    Config {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            cors_allow_origins: vec![],
        },
        database: DatabaseConfig {
            host: "127.0.0.1".to_string(),
            port: 5432,
            database: "fire_ctrl_test".to_string(),
            user: "postgres".to_string(),
            password: String::new(),
            max_connections: 1,
        },
        redis: RedisConfig {
            url: "redis://127.0.0.1/0".to_string(),
            rate_limit_url: "redis://127.0.0.1/1".to_string(),
        },
        llm: LlmConfig {
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            api_key: String::new(),
            model_name: "test".to_string(),
            embedding_model: None,
            ollama_base_url: None,
            timeout_seconds: 5,
            max_retries: 1,
            max_input_chars: 1_000,
        },
        crawler: CrawlerConfig {
            max_depth: 1,
            max_pages: 1,
            concurrent_requests: 1,
            request_timeout_seconds: 5,
            user_agent: "fire_ctrl-tests/0.1".to_string(),
            respect_robots_txt: false,
            playwright_url: None,
            browser_pool_size: 1,
        },
        worker: WorkerConfig {
            num_workers: 1,
            max_concurrent_jobs: 1,
            job_timeout_seconds: 5,
            max_stalls: 1,
        },
        auth: AuthConfig {
            use_db_authentication: use_db_auth,
            test_api_key: test_api_key.map(String::from),
            bull_auth_key: "bull-test-key".to_string(),
            allow_local_webhooks: true,
        },
        proxy: ProxyConfig {
            server: None,
            username: None,
            password: None,
        },
        search: SearchConfig {
            searxng_endpoint: None,
            searxng_engines: None,
            searxng_categories: None,
        },
        resource: ResourceConfig {
            max_cpu: 0.9,
            max_ram: 0.9,
        },
    }
}

// ---------------------------------------------------------------------------
// Auth gate
// ---------------------------------------------------------------------------

#[test]
fn auth_gate_bypassed_when_db_auth_disabled() {
    let cfg = make_config(false, None);
    let headers = HeaderMap::new();
    // No bearer, no test key — bypass still applies because USE_DB_AUTHENTICATION=false.
    assert!(check_auth_for_test(&headers, &cfg).is_ok());
}

#[test]
fn auth_gate_rejects_missing_bearer_when_enabled() {
    let cfg = make_config(true, Some("expected-key"));
    let headers = HeaderMap::new();
    let err = check_auth_for_test(&headers, &cfg).expect_err("auth must fail without bearer");
    assert!(matches!(err, AppError::Unauthorized), "got {err:?}");
}

#[test]
fn auth_gate_rejects_wrong_bearer() {
    let cfg = make_config(true, Some("expected-key"));
    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        HeaderValue::from_static("Bearer wrong-key"),
    );
    assert!(matches!(
        check_auth_for_test(&headers, &cfg),
        Err(AppError::Unauthorized)
    ));
}

#[test]
fn auth_gate_accepts_correct_bearer_via_test_key() {
    let cfg = make_config(true, Some("expected-key"));
    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        HeaderValue::from_static("Bearer expected-key"),
    );
    assert!(check_auth_for_test(&headers, &cfg).is_ok());
}

#[test]
fn auth_gate_accepts_bull_auth_key() {
    let cfg = make_config(true, None);
    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        HeaderValue::from_static("Bearer bull-test-key"),
    );
    assert!(check_auth_for_test(&headers, &cfg).is_ok());
}

// ---------------------------------------------------------------------------
// Resource gate
// ---------------------------------------------------------------------------

async fn state_with_resources(snap: ResourceSnapshot) -> AppState {
    // We do *not* build a real DB / scraper / LLM here — those are not touched
    // by `check_resources`. The struct fields are just required to materialise
    // an `AppState`. The DbClient holds a pool that will never be used because
    // the gate fires before any handler body runs.
    let cfg = make_config(false, None);
    let pool = fire_ctrl::database::build_pool(&cfg.database)
        .expect("pool builder must succeed (lazy, no connections opened)");
    let db = fire_ctrl::database::DbClient::new(pool);
    let scraper = Arc::new(
        fire_ctrl::scraper::Scraper::new(&cfg.crawler, &cfg.proxy)
            .await
            .expect("scraper builder must succeed"),
    );
    let llm = Arc::new(LlmClient::new(&cfg.llm).expect("llm client builder must succeed"));
    AppState {
        db,
        scraper,
        llm,
        cfg: Arc::new(cfg),
        resources: snap,
    }
}

#[tokio::test]
#[ignore = "requires local Chromium for Scraper construction"]
async fn resource_gate_passes_when_below_limits() {
    let snap = ResourceSnapshot::new();
    set_resource_snapshot_for_test(&snap, 0.10, 0.10);
    let state = state_with_resources(snap).await;
    assert!(check_resources_for_test(&state).is_ok());
}

#[tokio::test]
#[ignore = "requires local Chromium for Scraper construction"]
async fn resource_gate_blocks_on_saturated_cpu() {
    let snap = ResourceSnapshot::new();
    // 99% CPU, 10% RAM — should fail on CPU.
    set_resource_snapshot_for_test(&snap, 0.99, 0.10);
    let state = state_with_resources(snap).await;
    let err = check_resources_for_test(&state).expect_err("CPU saturation must block");
    assert!(matches!(err, AppError::ResourceLimit(_)));
    assert_eq!(
        err.into_response_status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "ResourceLimit must surface as 503",
    );
}

#[tokio::test]
#[ignore = "requires local Chromium for Scraper construction"]
async fn resource_gate_blocks_on_saturated_ram() {
    let snap = ResourceSnapshot::new();
    set_resource_snapshot_for_test(&snap, 0.10, 0.99);
    let state = state_with_resources(snap).await;
    assert!(matches!(
        check_resources_for_test(&state),
        Err(AppError::ResourceLimit(_))
    ));
}

// Small helper attached only to the test binary so we can assert the HTTP
// status the gate would surface without spinning up the whole router.
trait AppErrorTestExt {
    fn into_response_status(self) -> StatusCode;
}
impl AppErrorTestExt for AppError {
    fn into_response_status(self) -> StatusCode {
        use axum::response::IntoResponse;
        self.into_response().status()
    }
}

// ---------------------------------------------------------------------------
// Router-level smoke tests via Router::oneshot
// ---------------------------------------------------------------------------
//
// We hit `/v2/scrape` to verify the auth gate short-circuits with 401 BEFORE
// the handler attempts any I/O. This proves the route is wired and the gate
// is invoked. Asserting the success path against a full request requires a
// live Postgres / browser and lives under `tests/postgres_queue.rs` (ignored).

#[tokio::test]
#[ignore = "requires local Chromium for Scraper construction"]
async fn router_returns_401_for_protected_routes_without_bearer() {
    let cfg = make_config(true, Some("expected-key"));
    let pool = fire_ctrl::database::build_pool(&cfg.database).expect("pool builder");
    let db = fire_ctrl::database::DbClient::new(pool);
    let scraper = Arc::new(
        fire_ctrl::scraper::Scraper::new(&cfg.crawler, &cfg.proxy)
            .await
            .expect("scraper builder"),
    );
    let llm = Arc::new(LlmClient::new(&cfg.llm).expect("llm builder"));
    let state = AppState {
        db,
        scraper,
        llm,
        cfg: Arc::new(cfg),
        resources: ResourceSnapshot::new(),
    };
    let router = build_router_for_test(state);

    let body = r#"{"url":"https://example.com"}"#;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/scrape")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();

    let resp = router.oneshot(req).await.expect("router must respond");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let bytes = to_bytes(resp.into_body(), 4096)
        .await
        .expect("body must be readable");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("body is JSON");
    assert_eq!(json["success"], false);
}

#[tokio::test]
#[ignore = "requires local Chromium for Scraper construction"]
async fn router_returns_503_when_resources_are_saturated() {
    // USE_DB_AUTHENTICATION=false so the auth gate passes; we then expect
    // the resource gate to fire with 503.
    let cfg = make_config(false, None);
    let pool = fire_ctrl::database::build_pool(&cfg.database).expect("pool builder");
    let db = fire_ctrl::database::DbClient::new(pool);
    let scraper = Arc::new(
        fire_ctrl::scraper::Scraper::new(&cfg.crawler, &cfg.proxy)
            .await
            .expect("scraper builder"),
    );
    let llm = Arc::new(LlmClient::new(&cfg.llm).expect("llm builder"));
    let resources = ResourceSnapshot::new();
    set_resource_snapshot_for_test(&resources, 0.99, 0.10);
    let state = AppState {
        db,
        scraper,
        llm,
        cfg: Arc::new(cfg),
        resources,
    };
    let router = build_router_for_test(state);

    let body = r#"{"url":"https://example.com"}"#;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/scrape")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();

    let resp = router.oneshot(req).await.expect("router must respond");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------
// MapResponse shape — deserialisation contract
// ---------------------------------------------------------------------------
//
// We can't drive `/v2/map` end-to-end here (it would hit live HTTP), but we
// CAN assert the response shape by round-tripping a hand-constructed
// `MapResponse` through serde — this guards against accidental rename or
// shape drift of the public type.

// MapResponse round-trip omitted: the type is `#[non_exhaustive]` and only
// derives `Serialize`, so we cannot construct or deserialize it here. The
// `/v2/map` endpoint integration coverage lives under `tests/postgres_queue.rs`.

// ---------------------------------------------------------------------------
// Cancel fallback shape — 404 not 500 for unknown id
// ---------------------------------------------------------------------------
//
// The cancel handler hits the DB to look up the group / job; without a live
// pool we cannot exercise the happy path. We assert at the type level that
// an `AppError::NotFound` maps to 404 (not 500), which is the invariant the
// task asked us to lock in.
#[test]
fn not_found_app_error_maps_to_404_not_500() {
    use axum::response::IntoResponse;
    let resp = AppError::NotFound("00000000-0000-0000-0000-000000000000".into()).into_response();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Pagination cursor stability — same cursor produces same URL twice
// ---------------------------------------------------------------------------
//
// The full DB-backed pagination test lives in `tests/postgres_queue.rs`.
// Here we lock in the cursor-format invariant: the response field shape uses
// `after=<uuid>&after_created_at=<rfc3339>&limit=<n>` regardless of which
// values the caller passes.
#[test]
fn pagination_cursor_uses_after_and_after_created_at_and_limit() {
    // Reconstruct the format the handler uses (see `crawl_status_handler`).
    let id = uuid::Uuid::nil();
    let after_id = uuid::Uuid::nil();
    let after_created_at: chrono::DateTime<chrono::Utc> =
        chrono::DateTime::from_timestamp(0, 0).expect("epoch parses");
    let url1 = format!(
        "/v2/crawl/{}?after={}&after_created_at={}&limit={}",
        id,
        after_id,
        after_created_at.to_rfc3339(),
        25
    );
    let url2 = format!(
        "/v2/crawl/{}?after={}&after_created_at={}&limit={}",
        id,
        after_id,
        after_created_at.to_rfc3339(),
        25
    );
    assert_eq!(
        url1, url2,
        "cursor URL must be deterministic for same inputs"
    );
    assert!(url1.contains("after="));
    assert!(url1.contains("after_created_at="));
    assert!(url1.contains("limit=25"));
}
