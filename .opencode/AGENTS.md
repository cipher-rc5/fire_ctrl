# fire_ctrl — LLM Contribution Rules

`fire_ctrl` is a spec-compliant, self-hosted Firecrawl v2 runtime written in
**native Rust** (edition 2024). These rules govern how LLMs assist with this
codebase. They are mandatory — not advisory.

---

## 1. Language & Edition

- **Rust only.** No Python, TypeScript, shell logic embedded in source files.
- Target **Rust stable** (edition 2024). Do not use nightly-only features.
- All `let … else`, `let`-chains, and `async`/`await` patterns must compile on
  the current stable toolchain.

---

## 2. Code Quality Gates

Every change must satisfy all three gates before being considered complete:

```
cargo check          # must be error-free
cargo clippy -- -D warnings   # zero warnings; no #[allow] without explanation
cargo test           # all tests pass
```

- `cargo fmt` must be run before any commit (formatting is non-negotiable).
- Do **not** suppress clippy lints with `#[allow(...)]` unless you include a
  comment explaining *why* the lint is incorrect in this specific case.

---

## 3. Error Handling

- Use `AppError` (defined in `src/models.rs`) for all handler errors. Do not
  introduce new error enums without a strong justification.
- `anyhow::Error` is acceptable inside async worker and LLM logic where
  heterogeneous errors must propagate up to a single return type.
- Never `.unwrap()` or `.expect()` in production paths (`src/`). These are
  allowed only inside `#[cfg(test)]` blocks or `fn main()` startup (for
  config validation that must panic-fail fast).
- Map HTTP status codes correctly via `AppError`'s `IntoResponse` impl —
  do not hand-roll `StatusCode` responses unless adding a new variant.

---

## 4. Async & Concurrency

- All I/O must be async (`tokio`). No blocking calls on the async runtime
  (`std::fs`, `std::thread::sleep`, etc.) — use `tokio::fs` / `tokio::time`.
- Shared mutable state must use `Arc<Mutex<T>>` or `Arc<RwLock<T>>` from
  `tokio::sync`. Prefer `RwLock` for read-heavy data.
- Do **not** spawn unbounded tasks (`tokio::spawn` in a loop without a
  semaphore or `JoinSet`). Concurrency is gated by the `Semaphore` in
  `WorkerPool`.
- Background tasks that loop forever must have exponential back-off on errors
  (see `stall_recovery_loop` in `src/worker.rs` as the reference pattern).

---

## 5. HTTP & Networking

- **Reuse `reqwest::Client`** — never construct `Client::new()` inside a
  hot path (per-request or per-job). Clients must be allocated at startup
  and shared via `Arc`.
- All outbound HTTP calls must respect the configured timeouts
  (`CRAWLER_REQUEST_TIMEOUT_SECONDS`, `LLM_TIMEOUT_SECONDS`).
- Webhook delivery must honour `ALLOW_LOCAL_WEBHOOKS`. Never bypass
  `is_local_webhook()`.

---

## 6. Security

- **Authentication**: when `USE_DB_AUTHENTICATION=true`, every protected
  route must reject requests that don't present a valid bearer token. There
  is no silent bypass.
- **Secrets**: never log API keys, passwords, or bearer tokens — not even at
  `debug` level. Use `%` formatting for structured fields and keep values
  opaque.
- **SSRF**: webhook URLs are validated by `is_local_webhook()`. URL
  allowlisting must be preserved — do not add new outbound HTTP paths
  without SSRF consideration.
- **Input sizes**: all LLM inputs are capped at `LLM_MAX_INPUT_CHARS`. Any
  new LLM path must enforce this cap and emit a `warning` field in the
  response.
- Do not introduce `skip_tls_verification = true` as a default; it must
  remain opt-in per-request.

---

## 7. Database

- All SQL is written as raw parameterised strings passed to `tokio-postgres`.
  Do not introduce an ORM.
- Queries against `nuq.queue_scrape` (a TimescaleDB hypertable) must include
  `created_at` in the `WHERE` clause when targeting a specific row, to avoid
  full partition scans.
- New schema changes go in a new numbered migration file under `migrations/`
  (e.g. `002_add_column.sql`). Never modify `001_initial.sql`.
- Connection pool size is controlled by `DATABASE_MAX_CONNECTIONS`. Do not
  hard-code pool sizes.

---

## 8. Configuration

- All tunables must be read from environment variables via `config.rs`.
- New config fields require: a field in the appropriate `*Config` struct, an
  `env_required_*` / `env_opt` call in `from_env()`, and a documented entry
  in `.env.example`.
- Required variables use `env_required_*` (fail-fast on startup). Optional
  variables use `env_opt` (return `None`).
- Do not add `dotenv` calls outside of `Config::from_env()`.

---

## 9. Logging & Observability

- Use `tracing` macros (`info!`, `warn!`, `error!`, `debug!`) — never
  `println!` or `eprintln!` in production code.
- Structured fields use the `key = value` syntax:
  `info!(worker_id = self.id, %job_id, "Job started")`.
- Production log level defaults to `info`; `debug` is for development.
  `LOG_FORMAT=json` enables machine-readable output for log aggregators.
- The `/health` endpoint must remain fast (no blocking I/O in the hot path)
  and must return `503` when any critical dependency is down.

---

## 10. API Compatibility

- All route shapes must remain wire-compatible with the official Firecrawl
  v2 REST API. The SDK contract test in `tests/sdk_v2_contract.rs` is the
  ground truth — it must pass.
- Response field names follow `camelCase` JSON (via `#[serde(rename_all)]`
  or explicit `#[serde(rename)]`). Do not change existing field names.
- New endpoints must be added to the router in `src/api.rs` and documented
  in the route map comment at the top of that file.

---

## 11. Testing

- Unit tests live in `#[cfg(test)]` modules inside the relevant source file.
- Integration / contract tests live in `tests/`.
- Every new public function with non-trivial logic should have at least one
  unit test.
- Tests must not make real network calls. Use `mockito` for HTTP mocking.
- The contract test suite (`just contract-test`) is opt-in via
  `RUN_FIRECRAWL_CONTRACT_TESTS=1` — do not make it run unconditionally.

---

## 12. Dependencies

- Before adding a crate, check whether the functionality already exists in
  the dependency tree (e.g. prefer `serde_json` utilities over pulling in a
  JSON-path crate).
- New dependencies must have a comment in `Cargo.toml` explaining their
  purpose (follow the existing `# category` grouping).
- Licenses must be permissive (MIT, Apache-2.0, BSD). Copyleft (GPL) crates
  are not permitted. `cargo deny check` enforces this in CI.
- Security advisories are checked by `cargo audit` in CI (`just audit`). Do
  not merge a PR with an unresolved advisory unless a tracking issue is open.

---

## 13. File & Module Structure

```
src/
  main.rs      — CLI entry, tokio runtime, sub-command dispatch
  api.rs       — all HTTP route handlers + AppState
  config.rs    — typed env-var config, fail-fast loading
  models.rs    — domain types, AppError, request/response shapes
  database.rs  — DbClient, SQL, pool construction
  scraper.rs   — BrowserPool, HttpScraper, BFS crawl, HTML utils
  worker.rs    — WorkerPool, per-job executors, webhook delivery
  llm.rs       — LlmClient, OpenAI-compatible chat, JSON extraction
migrations/    — numbered .sql files, applied by `just migrate`
tests/         — integration / contract tests
bench/         — hurl benchmark files
scripts/       — bash smoke-test helpers
.github/       — CI / CD workflows, Dependabot config
.opencode/     — LLM contribution rules (this file)
```

- Do not create new top-level `src/` modules without discussion. Prefer
  extending an existing module or adding a sub-module.
- Keep modules focused: `api.rs` handles HTTP only; business logic belongs
  in `scraper.rs`, `worker.rs`, or `llm.rs`.

---

## 14. Commit Message Convention

All commits **must** follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).

### Format

```
<type>(<scope>): <short summary>

[optional body]

[optional footer(s)]
```

### Types

| Type | When to use |
|---|---|
| `feat` | A new feature visible to users or callers |
| `fix` | A bug fix |
| `perf` | A performance improvement with no behaviour change |
| `refactor` | Code restructuring with no feature or fix |
| `security` | Security hardening or vulnerability remediation |
| `test` | Adding or updating tests only |
| `ci` | CI/CD workflow or tooling changes |
| `docs` | Documentation only (comments, markdown, `AGENTS.md`) |
| `chore` | Maintenance (dependency bumps, `.gitignore`, config) |
| `revert` | Reverts a previous commit |

### Scopes (use the module / subsystem name)

`api` · `worker` · `scraper` · `llm` · `db` · `config` · `models` · `bench` · `ci` · `deps` · `auth`

### Rules

- The **summary line** must be ≤ 72 characters, imperative mood, lowercase,
  no trailing period. Example: `fix(auth): reject requests when no key configured`
- Use `!` after the type/scope for **breaking changes**:
  `feat(api)!: rename crawl status field`
- Breaking changes must also include a `BREAKING CHANGE:` footer explaining
  what callers must update.
- The body (if present) wraps at 72 characters and explains *why*, not *what*.
- Reference issues in footers: `Closes #42`, `Fixes #17`.
- Do **not** mix unrelated changes in a single commit. One logical change per
  commit.
- LLMs generating commits must use this format — never free-form prose.

### Examples

```
feat(scraper): add CrawlParams struct to reduce argument count

Replaces the 10-argument crawl() signature with a typed params struct,
eliminating the too-many-arguments clippy lint and making call sites
self-documenting.
```

```
fix(worker): reuse reqwest Client across webhook deliveries

Previously a new Client was allocated per webhook call, causing
connection pool exhaustion under load. A shared Arc<Client> is now
constructed once per WorkerPool and passed to all workers.
```

```
security(auth): remove silent bypass when TEST_API_KEY unset

When USE_DB_AUTHENTICATION=true but TEST_API_KEY was not configured,
any bearer token was silently accepted. The bypass arm is removed;
auth now always rejects when no valid key matches.
```

```
ci: add security workflow with cargo-audit, cargo-deny, and gitleaks
```

---

## 15. Justfile Conventions

- New developer tasks go in `justfile` with a `# ── Category ───` header
  comment matching the existing style.
- Recipes that require external tools must check for availability and print
  an install hint (`brew install <tool>`) before failing.
- Benchmarks use `hurl` (see `bench/`). Do not re-introduce `ab` / `httpd`.

---

## 16. What LLMs Must NOT Do

- Do not remove or weaken the auth check in `check_auth()`.
- Do not change the `nuq` schema name or table names — they must match the
  official Firecrawl schema.
- Do not add `CorsLayer::permissive()` alternatives that disable CORS
  entirely for production builds.
- Do not commit `.env` files or any file containing real credentials.
- Do not rewrite working code speculatively. Make the smallest correct
  change that satisfies the requirement.
- Do not introduce `unsafe` blocks without a clear, documented justification
  and a corresponding `// SAFETY:` comment.
