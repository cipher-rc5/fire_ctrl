-- firecrawl-rs initial schema
-- Matches the official Firecrawl nuq schema (TimescaleDB hypertables).

CREATE SCHEMA IF NOT EXISTS nuq;

-- Custom ENUM types
DO $$ BEGIN
  CREATE TYPE nuq.job_status AS ENUM (
    'queued', 'scraping', 'active', 'completed', 'failed', 'cancelled'
  );
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

DO $$ BEGIN
  CREATE TYPE nuq.group_status AS ENUM ('active', 'completed', 'cancelled');
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- Extensions
CREATE EXTENSION IF NOT EXISTS timescaledb CASCADE;
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- Main job queue
CREATE TABLE IF NOT EXISTS nuq.queue_scrape (
  id                uuid        NOT NULL DEFAULT gen_random_uuid(),
  status            nuq.job_status NOT NULL DEFAULT 'queued',
  data              jsonb,
  created_at        timestamptz NOT NULL DEFAULT now(),
  priority          int         NOT NULL DEFAULT 0,
  lock              uuid,
  locked_at         timestamptz,
  stalls            int,
  finished_at       timestamptz,
  listen_channel_id text,
  returnvalue       jsonb,
  failedreason      text,
  owner_id          uuid,
  group_id          uuid,
  CONSTRAINT queue_scrape_pkey PRIMARY KEY (id, created_at)
);

SELECT create_hypertable(
  'nuq.queue_scrape', 'created_at',
  if_not_exists => TRUE,
  chunk_time_interval => INTERVAL '1 day'
);

CREATE INDEX IF NOT EXISTS queue_scrape_active_locked_at_idx
  ON nuq.queue_scrape (locked_at)
  WHERE status = 'active';

CREATE INDEX IF NOT EXISTS queue_scrape_queued_optimal_idx
  ON nuq.queue_scrape (priority ASC, created_at ASC, id)
  WHERE status = 'queued';

CREATE INDEX IF NOT EXISTS queue_scrape_group_id_idx
  ON nuq.queue_scrape (group_id, created_at ASC, id);

CREATE INDEX IF NOT EXISTS queue_scrape_failed_idx
  ON nuq.queue_scrape (created_at)
  WHERE status = 'failed';

CREATE INDEX IF NOT EXISTS queue_scrape_completed_idx
  ON nuq.queue_scrape (created_at)
  WHERE status = 'completed';

-- Backlog queue (overflow)
CREATE TABLE IF NOT EXISTS nuq.queue_scrape_backlog (
  id                uuid        NOT NULL DEFAULT gen_random_uuid(),
  data              jsonb,
  created_at        timestamptz NOT NULL DEFAULT now(),
  priority          int         NOT NULL DEFAULT 0,
  listen_channel_id text,
  owner_id          uuid,
  group_id          uuid,
  times_out_at      timestamptz,
  CONSTRAINT queue_scrape_backlog_pkey PRIMARY KEY (id, created_at)
);

SELECT create_hypertable(
  'nuq.queue_scrape_backlog', 'created_at',
  if_not_exists => TRUE,
  chunk_time_interval => INTERVAL '1 day'
);

-- Completed crawl archive
CREATE TABLE IF NOT EXISTS nuq.queue_crawl_finished (
  id                uuid        NOT NULL DEFAULT gen_random_uuid(),
  status            nuq.job_status NOT NULL DEFAULT 'completed',
  data              jsonb,
  created_at        timestamptz NOT NULL DEFAULT now(),
  priority          int         NOT NULL DEFAULT 0,
  lock              uuid,
  locked_at         timestamptz,
  stalls            int,
  finished_at       timestamptz,
  listen_channel_id text,
  returnvalue       jsonb,
  failedreason      text,
  owner_id          uuid,
  group_id          uuid,
  CONSTRAINT queue_crawl_finished_pkey PRIMARY KEY (id, created_at)
);

SELECT create_hypertable(
  'nuq.queue_crawl_finished', 'created_at',
  if_not_exists => TRUE,
  chunk_time_interval => INTERVAL '1 day'
);

-- Crawl group tracking
CREATE TABLE IF NOT EXISTS nuq.group_crawl (
  id         uuid           NOT NULL,
  status     nuq.group_status NOT NULL DEFAULT 'active',
  created_at timestamptz    NOT NULL DEFAULT now(),
  owner_id   uuid           NOT NULL,
  ttl        int8           NOT NULL DEFAULT 86400000,
  expires_at timestamptz,
  CONSTRAINT group_crawl_pkey PRIMARY KEY (id)
);

-- Retention policies (7 days)
SELECT add_retention_policy(
  'nuq.queue_scrape', INTERVAL '7 days', if_not_exists => TRUE
);
SELECT add_retention_policy(
  'nuq.queue_scrape_backlog', INTERVAL '7 days', if_not_exists => TRUE
);
SELECT add_retention_policy(
  'nuq.queue_crawl_finished', INTERVAL '7 days', if_not_exists => TRUE
);
