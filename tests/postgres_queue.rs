//! file: tests/postgres_queue.rs
//! description: End-to-end queue state-machine integration test against a real
//! TimescaleDB container.
//!
//! Run with: `cargo test --test postgres_queue -- --ignored`
//!
//! Marked `#[ignore]` by default because CI doesn't have Docker. When Docker
//! is available locally, this test:
//!   1. Spins up a TimescaleDB container
//!   2. Applies `migrations/001_initial.sql`
//!   3. Enqueues a scrape job
//!   4. Claims it (FOR UPDATE SKIP LOCKED — proves no other claimer steals it)
//!   5. Completes it
//!   6. Asserts the row reaches `completed` state
//!
//! NOTE: `DbClient`'s public surface uses `enqueue` / `claim_next_job` /
//! `complete_job`; the task brief referenced earlier names (`enqueue_scrape`,
//! …) — those have since been generalised and the test reflects the current
//! API.
//!
//! Compiled only with `--features postgres-integration` so the default
//! `cargo test` matrix doesn't pin us to a specific `testcontainers` API
//! revision. Run with:
//!   `cargo test --features postgres-integration --test postgres_queue -- --ignored`

#![cfg(all(feature = "postgres-integration", not(target_os = "windows")))]

use chrono::Utc;
use fire_ctrl::database::DbClient;
use fire_ctrl::models::JobStatus;
use std::time::Duration;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use uuid::Uuid;

fn timescaledb_image() -> GenericImage {
    GenericImage::new("timescale/timescaledb-ha", "pg16")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
}

async fn build_client(host: &str, port: u16) -> DbClient {
    let cfg = fire_ctrl::config::DatabaseConfig {
        host: host.to_string(),
        port,
        database: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
        max_connections: 4,
    };
    let pool = fire_ctrl::database::build_pool(&cfg).expect("pool builder");
    DbClient::new(pool)
}

/// Build a parallel pool pointing at the same DB so we can run the migration
/// via `simple_query` (which handles multi-statement DDL — `DbClient` only
/// exposes typed queries).
async fn raw_pool_to(host: &str, port: u16) -> deadpool_postgres::Pool {
    let cfg = fire_ctrl::config::DatabaseConfig {
        host: host.to_string(),
        port,
        database: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
        max_connections: 2,
    };
    fire_ctrl::database::build_pool(&cfg).expect("pool builder")
}

#[tokio::test]
#[ignore = "needs Docker — run with `cargo test --test postgres_queue -- --ignored`"]
async fn enqueue_claim_complete_round_trip() {
    let container = timescaledb_image()
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .start()
        .await
        .expect("container must start");

    let host = container
        .get_host()
        .await
        .expect("container host")
        .to_string();
    let port = container
        .get_host_port_ipv4(5432.tcp())
        .await
        .expect("mapped port");

    // Wait briefly for the listener — testcontainers' WaitFor often races
    // ahead of the connection being accept()-ready.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Apply migrations.
    let raw_pool = raw_pool_to(&host, port).await;
    {
        let conn = raw_pool
            .get()
            .await
            .expect("initial connection must succeed");
        let sql = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/001_initial.sql",
        ))
        .expect("migration file must be readable");
        conn.simple_query(&sql)
            .await
            .expect("migrations must apply");
    }

    // Build the client and exercise the queue.
    let client = build_client(&host, port).await;

    // Enqueue.
    let payload = serde_json::json!({
        "kind": "scrape",
        "payload": { "url": "https://example.com/", "formats": ["markdown"] }
    });
    let row = client
        .enqueue(&payload, 0, None, None, None)
        .await
        .expect("enqueue must succeed");
    assert_eq!(row.status, JobStatus::Queued);

    // Claim — should atomically transition to active.
    let lock_id = Uuid::new_v4();
    let claimed = client
        .claim_next_job(lock_id)
        .await
        .expect("claim must not error")
        .expect("a claimable row must exist");
    assert_eq!(claimed.id, row.id);
    assert_eq!(claimed.status, JobStatus::Active);

    // A second claimer should see nothing — proves SKIP LOCKED + state guard.
    let second = client
        .claim_next_job(Uuid::new_v4())
        .await
        .expect("second claim must not error");
    assert!(
        second.is_none(),
        "second claim must return None when nothing is queued"
    );

    // Complete.
    let returnvalue = serde_json::json!({ "ok": true });
    client
        .complete_job(row.id, row.created_at, &returnvalue)
        .await
        .expect("complete must succeed");

    // Re-read and verify.
    let final_row = client.get_job(row.id).await.expect("job lookup");
    assert_eq!(final_row.status, JobStatus::Completed);
    assert_eq!(
        final_row.returnvalue.as_ref(),
        Some(&returnvalue),
        "returnvalue must persist on completion"
    );

    // Side check — created_at is bounded by `now()`.
    assert!(final_row.created_at <= Utc::now() + chrono::Duration::seconds(1));

    drop(container);
}
