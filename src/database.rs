/// file: src/database.rs
/// description: PostgreSQL pool setup and queue/group SQL data access layer.
/// Database layer — deadpool_postgres pool + all SQL operations.
///
/// Schema: the official Firecrawl `nuq` schema (TimescaleDB hypertables).
/// The DDL is embedded in `migrations/001_initial.sql` and applied by
/// `just migrate`.
use crate::config::DatabaseConfig;
use crate::models::{AppError, GroupCrawlRow, GroupStatus, JobStatus, QueueScrapeRow};
use chrono::{DateTime, Utc};
use deadpool_postgres::{
    Config as PoolConfig, ManagerConfig, Pool, PoolConfig as DeadpoolPoolConfig, RecyclingMethod,
    Runtime,
};
use tokio_postgres::{NoTls, Row};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Pool construction
// ---------------------------------------------------------------------------

pub fn build_pool(cfg: &DatabaseConfig) -> anyhow::Result<Pool> {
    let mut pc = PoolConfig::new();
    pc.host = Some(cfg.host.clone());
    pc.port = Some(cfg.port);
    pc.dbname = Some(cfg.database.clone());
    pc.user = Some(cfg.user.clone());
    if !cfg.password.is_empty() {
        pc.password = Some(cfg.password.clone());
    }
    pc.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });
    pc.pool = Some(DeadpoolPoolConfig::new(cfg.max_connections));

    pc.create_pool(Some(Runtime::Tokio1), NoTls)
        .map_err(|e| anyhow::anyhow!("Failed to create DB pool: {e}"))
}

// ---------------------------------------------------------------------------
// Column-list constants (cast custom enums to text for tokio-postgres)
// ---------------------------------------------------------------------------

/// All columns of nuq.queue_scrape with status cast to text.
const QS_COLS: &str =
    "id, status::text AS status, data, created_at, returnvalue, failedreason, group_id";

/// All columns of nuq.group_crawl with status cast to text.
const GC_COLS: &str = "id, status::text AS status, created_at, owner_id, ttl, expires_at";

// ---------------------------------------------------------------------------
// Row mappers
// ---------------------------------------------------------------------------

fn map_job_status(s: &str) -> JobStatus {
    match s {
        "scraping" => JobStatus::Scraping,
        "active" => JobStatus::Active,
        "completed" => JobStatus::Completed,
        "failed" => JobStatus::Failed,
        "cancelled" => JobStatus::Cancelled,
        _ => JobStatus::Queued,
    }
}

fn map_group_status(s: &str) -> GroupStatus {
    match s {
        "completed" => GroupStatus::Completed,
        "cancelled" => GroupStatus::Cancelled,
        _ => GroupStatus::Active,
    }
}

fn map_queue_row(row: &Row) -> QueueScrapeRow {
    let status_str: String = row.get("status");
    QueueScrapeRow {
        id: row.get("id"),
        status: map_job_status(&status_str),
        data: row.get("data"),
        created_at: row.get::<_, DateTime<Utc>>("created_at"),
        returnvalue: row.get("returnvalue"),
        failedreason: row.get("failedreason"),
        group_id: row.get("group_id"),
    }
}

fn map_group_row(row: &Row) -> GroupCrawlRow {
    let status_str: String = row.get("status");
    GroupCrawlRow {
        id: row.get("id"),
        status: map_group_status(&status_str),
        created_at: row.get::<_, DateTime<Utc>>("created_at"),
        owner_id: row.get("owner_id"),
        ttl: row.get("ttl"),
        expires_at: row.get("expires_at"),
    }
}

// ---------------------------------------------------------------------------
// DbClient
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct DbClient {
    pool: Pool,
}

impl DbClient {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    // ── Liveness ────────────────────────────────────────────────────────────

    pub async fn ping(&self) -> Result<(), AppError> {
        let conn = self.pool.get().await?;
        conn.execute("SELECT 1", &[]).await?;
        Ok(())
    }

    // ── Enqueue ─────────────────────────────────────────────────────────────

    pub async fn enqueue(
        &self,
        data: &serde_json::Value,
        priority: i32,
        owner_id: Option<Uuid>,
        group_id: Option<Uuid>,
        listen_channel_id: Option<&str>,
    ) -> Result<QueueScrapeRow, AppError> {
        let conn = self.pool.get().await?;
        let sql = format!(
            "INSERT INTO nuq.queue_scrape \
             (data, priority, owner_id, group_id, listen_channel_id) \
             VALUES ($1, $2, $3, $4, $5) \
             RETURNING {QS_COLS}"
        );
        let row = conn
            .query_one(
                &sql,
                &[&data, &priority, &owner_id, &group_id, &listen_channel_id],
            )
            .await?;
        Ok(map_queue_row(&row))
    }

    pub async fn enqueue_completed(
        &self,
        data: &serde_json::Value,
        returnvalue: &serde_json::Value,
        priority: i32,
        owner_id: Option<Uuid>,
        group_id: Option<Uuid>,
        listen_channel_id: Option<&str>,
    ) -> Result<QueueScrapeRow, AppError> {
        let conn = self.pool.get().await?;
        let sql = format!(
            "INSERT INTO nuq.queue_scrape \
             (data, priority, owner_id, group_id, listen_channel_id, status, returnvalue, finished_at) \
             VALUES ($1, $2, $3, $4, $5, 'completed', $6, now()) \
             RETURNING {QS_COLS}"
        );
        let row = conn
            .query_one(
                &sql,
                &[
                    &data,
                    &priority,
                    &owner_id,
                    &group_id,
                    &listen_channel_id,
                    &returnvalue,
                ],
            )
            .await?;
        Ok(map_queue_row(&row))
    }

    // ── Claim (atomic, skip-locked) ──────────────────────────────────────────

    pub async fn claim_next_job(&self, lock_id: Uuid) -> Result<Option<QueueScrapeRow>, AppError> {
        let conn = self.pool.get().await?;
        let sql = format!(
            "UPDATE nuq.queue_scrape \
             SET status = 'active', lock = $1, locked_at = now() \
             WHERE (id, created_at) = ( \
               SELECT id, created_at FROM nuq.queue_scrape \
               WHERE status = 'queued' \
               ORDER BY priority ASC, created_at ASC, id ASC \
               LIMIT 1 \
               FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING {QS_COLS}"
        );
        let rows = conn.query(&sql, &[&lock_id]).await?;
        Ok(rows.first().map(map_queue_row))
    }

    // ── Status updates ───────────────────────────────────────────────────────

    pub async fn complete_job(
        &self,
        id: Uuid,
        created_at: DateTime<Utc>,
        returnvalue: &serde_json::Value,
    ) -> Result<(), AppError> {
        let conn = self.pool.get().await?;
        let n = conn
            .execute(sql_complete_job(), &[&id, &created_at, &returnvalue])
            .await?;
        if n == 0 {
            return Err(AppError::NotFound(id.to_string()));
        }
        Ok(())
    }

    pub async fn fail_job(
        &self,
        id: Uuid,
        created_at: DateTime<Utc>,
        reason: &str,
    ) -> Result<(), AppError> {
        let conn = self.pool.get().await?;
        conn.execute(sql_fail_job(), &[&id, &created_at, &reason])
            .await?;
        Ok(())
    }

    pub async fn cancel_job(&self, id: Uuid) -> Result<(), AppError> {
        let conn = self.pool.get().await?;
        let n = conn.execute(sql_cancel_job_latest_by_id(), &[&id]).await?;
        if n == 0 {
            return Err(AppError::NotFound(id.to_string()));
        }
        Ok(())
    }

    // ── Queries ──────────────────────────────────────────────────────────────

    pub async fn get_job(&self, id: Uuid) -> Result<QueueScrapeRow, AppError> {
        let conn = self.pool.get().await?;
        let sql = format!(
            "SELECT {QS_COLS} FROM nuq.queue_scrape \
             WHERE (id, created_at) = (\
               SELECT id, created_at FROM nuq.queue_scrape\
               WHERE id = $1\
               ORDER BY created_at DESC\
               LIMIT 1\
             )"
        );
        let rows = conn.query(&sql, &[&id]).await?;
        rows.first()
            .map(map_queue_row)
            .ok_or(AppError::NotFound(id.to_string()))
    }

    /// Return all completed scrape results that belong to a group (crawl/batch).
    /// Paginates: pass `after_id` + `after_created_at` from the last row for
    /// cursor-based pagination (matches the official Firecrawl crawl-status
    /// polling pattern).
    pub async fn get_group_results(
        &self,
        group_id: Uuid,
        limit: i64,
        after_id: Option<Uuid>,
        after_created_at: Option<DateTime<Utc>>,
    ) -> Result<Vec<QueueScrapeRow>, AppError> {
        let conn = self.pool.get().await?;
        let rows = match (after_id, after_created_at) {
            (Some(aid), Some(ats)) => {
                let sql = format!(
                    "SELECT {QS_COLS} FROM nuq.queue_scrape \
                     WHERE group_id = $1 \
                       AND (created_at, id) > ($3, $2) \
                     ORDER BY created_at ASC, id ASC \
                     LIMIT $4"
                );
                conn.query(&sql, &[&group_id, &aid, &ats, &limit]).await?
            }
            _ => {
                let sql = format!(
                    "SELECT {QS_COLS} FROM nuq.queue_scrape \
                     WHERE group_id = $1 \
                     ORDER BY created_at ASC, id ASC \
                     LIMIT $2"
                );
                conn.query(&sql, &[&group_id, &limit]).await?
            }
        };
        Ok(rows.iter().map(map_queue_row).collect())
    }

    /// Count rows in each status for a group — used to build CrawlStatusResponse.
    pub async fn get_group_counts(&self, group_id: Uuid) -> Result<GroupCounts, AppError> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_one(
                "SELECT \
                   COUNT(*) FILTER (WHERE status = 'completed') AS completed, \
                   COUNT(*) FILTER (WHERE status = 'failed')    AS failed, \
                   COUNT(*) FILTER (WHERE status = 'queued' OR status = 'active') AS pending, \
                   COUNT(*) AS total \
                 FROM nuq.queue_scrape \
                 WHERE group_id = $1",
                &[&group_id],
            )
            .await?;
        Ok(GroupCounts {
            completed: row.get::<_, i64>("completed") as u32,
            failed: row.get::<_, i64>("failed") as u32,
            pending: row.get::<_, i64>("pending") as u32,
            total: row.get::<_, i64>("total") as u32,
        })
    }

    // ── Crawl group ──────────────────────────────────────────────────────────

    pub async fn create_crawl_group(
        &self,
        owner_id: Uuid,
        ttl_ms: i64,
    ) -> Result<GroupCrawlRow, AppError> {
        let conn = self.pool.get().await?;
        let ttl_secs = ttl_ms as f64 / 1000.0;
        let sql = format!(
            "INSERT INTO nuq.group_crawl (id, owner_id, ttl, expires_at) \
             VALUES (gen_random_uuid(), $1, $2, now() + make_interval(secs => $3)) \
             RETURNING {GC_COLS}"
        );
        let row = conn
            .query_one(&sql, &[&owner_id, &ttl_ms, &ttl_secs])
            .await?;
        Ok(map_group_row(&row))
    }

    pub async fn get_crawl_group(&self, id: Uuid) -> Result<GroupCrawlRow, AppError> {
        let conn = self.pool.get().await?;
        let sql = format!("SELECT {GC_COLS} FROM nuq.group_crawl WHERE id = $1");
        let rows = conn.query(&sql, &[&id]).await?;
        rows.first()
            .map(map_group_row)
            .ok_or(AppError::NotFound(id.to_string()))
    }

    pub async fn set_group_status(&self, id: Uuid, status: GroupStatus) -> Result<(), AppError> {
        let conn = self.pool.get().await?;
        let status_str = status.to_string();
        conn.execute(
            "UPDATE nuq.group_crawl SET status = $2::text::nuq.group_status WHERE id = $1",
            &[&id, &status_str.as_str()],
        )
        .await?;
        Ok(())
    }

    pub async fn cancel_group(&self, id: Uuid) -> Result<(), AppError> {
        let conn = self.pool.get().await?;
        // Cancel all pending jobs in the group, then mark the group cancelled.
        conn.execute(
            "UPDATE nuq.queue_scrape \
             SET status = 'cancelled', finished_at = now(), lock = NULL, locked_at = NULL \
             WHERE group_id = $1 AND status IN ('queued', 'active')",
            &[&id],
        )
        .await?;
        conn.execute(
            "UPDATE nuq.group_crawl SET status = 'cancelled' WHERE id = $1",
            &[&id],
        )
        .await?;
        Ok(())
    }

    pub async fn update_job_group(
        &self,
        id: Uuid,
        created_at: DateTime<Utc>,
        group_id: Uuid,
    ) -> Result<(), AppError> {
        let conn = self.pool.get().await?;
        conn.execute(sql_update_job_group(), &[&id, &created_at, &group_id])
            .await?;
        Ok(())
    }

    // ── Completed crawl archive ──────────────────────────────────────────────

    // ── Stall recovery ───────────────────────────────────────────────────────

    pub async fn requeue_stalled_jobs(
        &self,
        stale_after: std::time::Duration,
    ) -> Result<u64, AppError> {
        let conn = self.pool.get().await?;
        let secs = stale_after.as_secs() as f64;
        let n = conn.execute(sql_requeue_stalled_jobs(), &[&secs]).await?;
        Ok(n)
    }

    pub async fn fail_over_stalled_jobs(&self, max_stalls: i32) -> Result<u64, AppError> {
        let conn = self.pool.get().await?;
        let n = conn
            .execute(sql_fail_over_stalled_jobs(), &[&max_stalls])
            .await?;
        Ok(n)
    }

    // ── Ongoing crawls (for GET /v2/crawl/ongoing) ───────────────────────────

    pub async fn get_active_groups(&self) -> Result<Vec<GroupCrawlRow>, AppError> {
        let conn = self.pool.get().await?;
        let sql = format!(
            "SELECT {GC_COLS} FROM nuq.group_crawl WHERE status = 'active' ORDER BY created_at DESC"
        );
        let rows = conn.query(&sql, &[]).await?;
        Ok(rows.iter().map(map_group_row).collect())
    }
}

fn sql_complete_job() -> &'static str {
    "UPDATE nuq.queue_scrape \
     SET status = 'completed', returnvalue = $3, finished_at = now(), \
         lock = NULL, locked_at = NULL \
     WHERE id = $1 AND created_at = $2"
}

fn sql_fail_job() -> &'static str {
    "UPDATE nuq.queue_scrape \
     SET status = 'failed', failedreason = $3, finished_at = now(), \
         lock = NULL, locked_at = NULL \
     WHERE id = $1 AND created_at = $2"
}

fn sql_cancel_job_latest_by_id() -> &'static str {
    "UPDATE nuq.queue_scrape \
     SET status = 'cancelled', finished_at = now(), \
         lock = NULL, locked_at = NULL \
     WHERE (id, created_at) = (\
       SELECT id, created_at FROM nuq.queue_scrape \
       WHERE id = $1 \
       ORDER BY created_at DESC \
       LIMIT 1\
     ) \
       AND status IN ('queued', 'active')"
}

fn sql_update_job_group() -> &'static str {
    "UPDATE nuq.queue_scrape SET group_id = $3 WHERE id = $1 AND created_at = $2"
}

fn sql_requeue_stalled_jobs() -> &'static str {
    "UPDATE nuq.queue_scrape \
     SET status = 'queued', lock = NULL, locked_at = NULL, \
         stalls = COALESCE(stalls, 0) + 1 \
     WHERE status = 'active' \
       AND locked_at < now() - make_interval(secs => $1)"
}

fn sql_fail_over_stalled_jobs() -> &'static str {
    "UPDATE nuq.queue_scrape \
     SET status = 'failed', \
         failedreason = 'exceeded maximum stall count', \
         finished_at = now() \
     WHERE status = 'queued' AND stalls >= $1"
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct GroupCounts {
    pub completed: u32,
    pub failed: u32,
    pub pending: u32,
    pub total: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specific_row_queries_include_created_at_predicate() {
        assert!(sql_complete_job().contains("created_at = $2"));
        assert!(sql_fail_job().contains("created_at = $2"));
        assert!(sql_update_job_group().contains("created_at = $2"));
        assert!(sql_cancel_job_latest_by_id().contains("(id, created_at)"));
    }

    #[test]
    fn cancel_job_sql_limits_statuses() {
        let sql = sql_cancel_job_latest_by_id();
        assert!(sql.contains("status IN ('queued', 'active')"));
        assert!(sql.contains("ORDER BY created_at DESC"));
    }

    #[test]
    fn stall_recovery_queries_target_expected_rows() {
        let requeue_sql = sql_requeue_stalled_jobs();
        assert!(requeue_sql.contains("WHERE status = 'active'"));
        assert!(requeue_sql.contains("locked_at < now() - make_interval"));

        let fail_over_sql = sql_fail_over_stalled_jobs();
        assert!(fail_over_sql.contains("WHERE status = 'queued' AND stalls >= $1"));
    }
}
