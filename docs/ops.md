# Operations Runbook

This runbook covers installation, configuration, and common operational tasks for local/self-hosted deployments.

## Install

Run once on a fresh machine to install and configure all dependencies:

```bash
just install
```

### What it does

| Step | Detail |
|---|---|
| Homebrew check | Exits with a message if `brew` is not found |
| Package install | Installs each package idempotently — already-installed packages are skipped |
| TimescaleDB config | Ensures `shared_preload_libraries = 'timescaledb'` is set in `postgresql.conf`; restarts postgresql@17 only if the file was modified |
| Service start | Starts postgresql@17, redis, and rabbitmq via `brew services` if not already running |
| `.env` bootstrap | Copies `.env.example` to `.env` if `.env` does not exist, then exits so credentials can be filled in before re-running |
| Superuser detection | Probes `$(whoami)` then `postgres` to find a working superuser — Homebrew on macOS bootstraps with the OS username, not `postgres` |
| Role + database | Creates the Postgres role and database declared in `.env` if they do not already exist |
| Extension | Runs `CREATE EXTENSION IF NOT EXISTS timescaledb CASCADE` on the application database |

### What it does not do

- Run migrations — call `just migrate` after install completes
- Build the binary — call `just build-release` or `just start` separately
- Start the application

### Typical first-run sequence

```bash
just install        # dependencies and DB setup
just migrate        # apply schema
just start          # build release binary and run
```

If `.env` does not exist when `just install` runs, it will be created from `.env.example` and the recipe will exit. Edit `.env` with your credentials before re-running.

### macOS superuser note

Homebrew initialises PostgreSQL with the OS user as the superuser, not a `postgres` role. `just install` probes candidates in order (`$(whoami)`, then `postgres`) and uses the first one that connects successfully. If neither works, the recipe exits with a diagnostic listing which usernames were tried and advising a check of `pg_hba.conf`.

---

## Safety levels

- Safe: read-only checks and targeted cleanup.
- Destructive: irreversible data deletion (confirm before running).

## Preflight checks (safe)

```bash
just health
just db-stats
```

Optional direct checks:

```bash
curl -sS http://localhost:3002/health | jq
```

## Cleanup options

### 1) Targeted Postgres cleanup (destructive)

Removes queue/crawl state while keeping schema/migrations.

```bash
just db-clean
```

### 2) Targeted Redis namespace cleanup (safer)

Deletes only known Firecrawl key prefixes.

```bash
just redis-clean
```

### 3) Full Redis DB flush (destructive)

Removes all keys in the configured Redis DB.

```bash
just redis-flush-all
```

### 4) RabbitMQ queue purge (destructive)

Purges known queues if `rabbitmqadmin` is installed.

```bash
just rabbit-clean
```

### 5) Full data cleanup (destructive)

Runs Postgres + Redis namespace cleanup + RabbitMQ purge.

```bash
just data-clean
```

## Recommended incident workflow

1. Check health and DB stats.
2. Try `just redis-clean` first if queue/cache looks stuck.
3. If jobs remain inconsistent, run `just db-clean`.
4. If message backlog remains, run `just rabbit-clean`.
5. As last resort for cache corruption, run `just redis-flush-all`.

## Restart sequence

```bash
just stop
just run-all
```

## Notes

- Config is strict: missing required env vars prevent startup.
- Keep `.env` synced with `.env.example` after config changes.
- Check `data.warning` on scrape responses for non-fatal extraction conditions (input truncation or provider retry behavior).
- Rotate API keys immediately if exposed in logs, screenshots, or chat.
