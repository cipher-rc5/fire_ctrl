# Running fire_ctrl

## First run

Install and configure all dependencies:

```bash
just install
```

This will:

- Install all required Homebrew packages
- Configure `shared_preload_libraries = 'timescaledb'` in `postgresql.conf`
- Start postgresql@17, redis, and rabbitmq as background services
- Create the Postgres role and database declared in `.env`
- Enable the TimescaleDB extension on the database

If `.env` does not exist, `just install` creates it from `.env.example` and exits.
Edit `.env` with your credentials and re-run `just install`.

Once install completes, run migrations and start the service:

```bash
just migrate
just start
```

---

## Subsequent runs

Services (postgresql@17, redis, rabbitmq) are registered as launchd agents by
`brew services` and start automatically on login. You only need to run the
binary:

```bash
just start        # release build, server + workers
just run-all      # dev build, server + workers
```

To run the server and worker separately (two terminals):

```bash
# Terminal 1
just run-server

# Terminal 2
just run-worker
```

---

## Verify

```bash
curl -sS http://localhost:3002/health | jq
```

```bash
curl -sS -X POST http://localhost:3002/v2/scrape \
  -H "Content-Type: application/json" \
  -d '{"url":"https://www.scrapethissite.com/pages/simple","formats":["markdown"]}' \
  | jq '.success'
```

---

## Stop

```bash
just stop
```

---

## Notes

- Config is strict: missing required env vars prevent startup.
- Keep `.env` in sync with `.env.example` after any config changes.
- See `docs/ops.md` for cleanup, incident response, and restart procedures.
- Rust is pinned to 1.93 via `Cargo.toml` and `rust-toolchain.toml`.
- If using rustup, run `rustup override set 1.93.0` in this repo.
