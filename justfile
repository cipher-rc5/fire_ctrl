# fire_ctrl — Justfile
set dotenv-load := true

default:
    @just --list

# ── Install ────────────────────────────────────────────────────────────────

# Install and configure all required dependencies via Homebrew
install:
    #!/usr/bin/env bash
    set -euo pipefail

    # ── Variables ─────────────────────────────────────────────────────────
    BREW_PREFIX="/opt/homebrew"
    PG_VERSION="17"
    PG_FORMULA="postgresql@${PG_VERSION}"
    PG_BIN="${BREW_PREFIX}/opt/${PG_FORMULA}/bin"
    PG_DATA="${BREW_PREFIX}/var/${PG_FORMULA}"
    PG_CONF="${PG_DATA}/postgresql.conf"
    PG_SUPERUSER_CANDIDATES=("$(whoami)" postgres)
    PG_MAINTENANCE_DB="postgres"
    TIMESCALEDB_EXTENSION="timescaledb"
    BREW_PKGS=(
        "${PG_FORMULA}"
        timescaledb
        timescaledb-tools
        redis
        rabbitmq
        rust
        just
        jq
        hurl
    )
    SERVICE_STARTUP_WAIT_PG=4
    SERVICE_STARTUP_WAIT_SHORT=2
    SERVICE_STARTUP_WAIT_LONG=3

    # ── 1. Homebrew ──────────────────────────────────────────────────────
    if ! command -v brew &>/dev/null; then
        echo "✗ Homebrew not found — install it first: https://brew.sh"
        exit 1
    fi
    echo "✓ Homebrew found: $(brew --version | head -1)"

    # ── 2. Brew packages ─────────────────────────────────────────────────
    for pkg in "${BREW_PKGS[@]}"; do
        if brew list "$pkg" &>/dev/null; then
            echo "✓ $pkg already installed"
        else
            echo "→ Installing $pkg ..."
            brew install "$pkg"
        fi
    done

    # ── 3. Configure TimescaleDB in postgresql.conf ───────────────────────
    if [[ ! -f "$PG_CONF" ]]; then
        echo "✗ postgresql.conf not found at $PG_CONF — is ${PG_FORMULA} initialised?"
        exit 1
    fi

    if grep -qE "^shared_preload_libraries\s*=.*${TIMESCALEDB_EXTENSION}" "$PG_CONF"; then
        echo "✓ TimescaleDB already in shared_preload_libraries"
        PG_NEEDS_RESTART=false
    else
        echo "→ Adding ${TIMESCALEDB_EXTENSION} to shared_preload_libraries in postgresql.conf ..."
        if grep -qE "^#?shared_preload_libraries" "$PG_CONF"; then
            sed -i '' -E \
                "s|^#?shared_preload_libraries[[:space:]]*=.*|shared_preload_libraries = '${TIMESCALEDB_EXTENSION}'\t\t# (change requires restart)|" \
                "$PG_CONF"
        else
            echo "shared_preload_libraries = '${TIMESCALEDB_EXTENSION}'" >> "$PG_CONF"
        fi
        echo "✓ postgresql.conf updated"
        PG_NEEDS_RESTART=true
    fi

    # ── 4. Start / restart services ───────────────────────────────────────
    if [[ "${PG_NEEDS_RESTART}" == true ]]; then
        echo "→ Restarting ${PG_FORMULA} to load TimescaleDB ..."
        brew services restart "${PG_FORMULA}"
        sleep "${SERVICE_STARTUP_WAIT_PG}"
    elif ! brew services list | grep "^${PG_FORMULA}[[:space:]]" | grep -q "started"; then
        echo "→ Starting ${PG_FORMULA} ..."
        brew services start "${PG_FORMULA}"
        sleep "${SERVICE_STARTUP_WAIT_PG}"
    else
        echo "✓ ${PG_FORMULA} already running"
    fi

    if ! brew services list | grep "^redis[[:space:]]" | grep -q "started"; then
        echo "→ Starting redis ..."
        brew services start redis
        sleep "${SERVICE_STARTUP_WAIT_SHORT}"
    else
        echo "✓ redis already running"
    fi

    if ! brew services list | grep "^rabbitmq[[:space:]]" | grep -q "started"; then
        echo "→ Starting rabbitmq ..."
        brew services start rabbitmq
        sleep "${SERVICE_STARTUP_WAIT_LONG}"
    else
        echo "✓ rabbitmq already running"
    fi

    # ── 5. Initialise .env ────────────────────────────────────────────────
    if [[ ! -f .env ]]; then
        cp .env.example .env
        echo ""
        echo "✗ .env created from .env.example — edit it with your credentials, then re-run 'just install'."
        exit 0
    fi
    echo "✓ .env present"

    # ── 6. Detect PostgreSQL superuser ───────────────────────────────────
    source .env

    PG_HOST="${POSTGRES_HOST:-localhost}"
    PG_PORT="${POSTGRES_PORT:-5432}"

    # Homebrew PostgreSQL on macOS bootstraps with the OS user as the superuser,
    # not "postgres". Probe each candidate until one connects successfully.
    PG_SUPERUSER=""
    for candidate in "${PG_SUPERUSER_CANDIDATES[@]}"; do
        if "$PG_BIN/psql" -U "$candidate" -d "$PG_MAINTENANCE_DB" \
               -h "$PG_HOST" -p "$PG_PORT" \
               -tc "SELECT 1" &>/dev/null; then
            PG_SUPERUSER="$candidate"
            break
        fi
    done

    if [[ -z "$PG_SUPERUSER" ]]; then
        echo "✗ Could not connect to PostgreSQL as any known superuser."
        echo "  Tried: ${PG_SUPERUSER_CANDIDATES[*]}"
        echo "  Check that ${PG_FORMULA} is running and pg_hba.conf allows local connections."
        exit 1
    fi
    echo "✓ PostgreSQL superuser detected: $PG_SUPERUSER"

    SU_PSQL="$PG_BIN/psql -U $PG_SUPERUSER -d $PG_MAINTENANCE_DB -h $PG_HOST -p $PG_PORT"

    # ── 7. Create role + database ─────────────────────────────────────────
    if $SU_PSQL -tc "SELECT 1 FROM pg_roles WHERE rolname='${POSTGRES_USER}'" | grep -q 1; then
        echo "✓ Postgres role '${POSTGRES_USER}' already exists"
    else
        echo "→ Creating Postgres role '${POSTGRES_USER}' ..."
        $SU_PSQL -c "CREATE USER ${POSTGRES_USER} WITH PASSWORD '${POSTGRES_PASSWORD}';"
    fi

    if $SU_PSQL -tc "SELECT 1 FROM pg_database WHERE datname='${POSTGRES_DB}'" | grep -q 1; then
        echo "✓ Database '${POSTGRES_DB}' already exists"
    else
        echo "→ Creating database '${POSTGRES_DB}' ..."
        $SU_PSQL -c "CREATE DATABASE ${POSTGRES_DB} OWNER ${POSTGRES_USER};"
    fi

    # ── 8. Enable TimescaleDB extension ───────────────────────────────────
    echo "→ Ensuring ${TIMESCALEDB_EXTENSION} extension is enabled in '${POSTGRES_DB}' ..."
    $SU_PSQL -d "${POSTGRES_DB}" -c "CREATE EXTENSION IF NOT EXISTS ${TIMESCALEDB_EXTENSION} CASCADE;"
    echo "✓ ${TIMESCALEDB_EXTENSION} extension active"

    echo ""
    echo "Dependencies installed and configured. Run 'just migrate' then 'just start'."

# ── Build ──────────────────────────────────────────────────────────────────

build:
    cargo build

build-release:
    cargo build --release

check:
    cargo check

# ── Quality ────────────────────────────────────────────────────────────────

test:
    cargo test

contract-test:
    RUN_FIRECRAWL_CONTRACT_TESTS=1 cargo test --test sdk_v2_contract -- --nocapture

api-suite-quotes:
    bash scripts/api-suite-quotes.sh

api-test name:
    bash "scripts/api-tests/{{name}}.sh"

lint:
    cargo clippy -- -D warnings

fmt:
    cargo fmt

test-all: test lint check
    @echo "All checks passed!"

# ── Run ────────────────────────────────────────────────────────────────────

run-server:
    cargo run -- server

run-worker:
    cargo run -- worker

run-all:
    cargo run -- all

scrape-markdown url="https://www.scrapethissite.com/pages/simple":
    curl -sS -X POST http://localhost:3002/v2/scrape \
      -H "Content-Type: application/json" \
      -d '{"url":"{{url}}","formats":["markdown"],"only_main_content":true}' \
      | jq '{success, url: .data.url, title: .data.title, metadata: .data.metadata, markdownPreview: (.data.markdown // "" | split("\n") | .[:30] | join("\n"))}'

scrape-json url="https://www.scrapethissite.com/pages/simple":
    curl -sS -X POST http://localhost:3002/v2/scrape \
      -H "Content-Type: application/json" \
      -d '{"url":"{{url}}","formats":["json"]}' \
      | jq '{success, url: .data.url, title: .data.title, metadata: .data.metadata, json: .data.json}'

scrape-both url="https://www.scrapethissite.com/pages/simple":
    curl -sS -X POST http://localhost:3002/v2/scrape \
      -H "Content-Type: application/json" \
      -d '{"url":"{{url}}","formats":["markdown","json"],"only_main_content":true}' \
      | jq '{success, url: .data.url, title: .data.title, markdownPreview: (.data.markdown // "" | split("\n") | .[:20] | join("\n")), json: .data.json}'

health:
    cargo run -- healthcheck

# ── Database ───────────────────────────────────────────────────────────────

migrate:
    #!/usr/bin/env bash
    set -euo pipefail
    source .env
    psql "postgresql://${POSTGRES_USER}:${POSTGRES_PASSWORD}@${POSTGRES_HOST}:${POSTGRES_PORT}/${POSTGRES_DB}" \
        -f migrations/001_initial.sql
    echo "Migrations complete!"

db-console:
    #!/usr/bin/env bash
    source .env
    psql "postgresql://${POSTGRES_USER}:${POSTGRES_PASSWORD}@${POSTGRES_HOST}:${POSTGRES_PORT}/${POSTGRES_DB}"

db-stats:
    #!/usr/bin/env bash
    source .env
    psql "postgresql://${POSTGRES_USER}:${POSTGRES_PASSWORD}@${POSTGRES_HOST}:${POSTGRES_PORT}/${POSTGRES_DB}" << 'EOF'
    SELECT status, COUNT(*) AS count,
           MIN(created_at) AS oldest, MAX(created_at) AS newest
    FROM nuq.queue_scrape GROUP BY status;
    EOF

db-clean:
    #!/usr/bin/env bash
    echo "WARNING: This will delete all data!"
    read -p "Are you sure? (yes/no): " confirm
    if [ "$confirm" = "yes" ]; then
        source .env
        psql "postgresql://${POSTGRES_USER}:${POSTGRES_PASSWORD}@${POSTGRES_HOST}:${POSTGRES_PORT}/${POSTGRES_DB}" << 'EOF'
        TRUNCATE nuq.queue_scrape, nuq.queue_scrape_backlog,
                 nuq.queue_crawl_finished, nuq.group_crawl CASCADE;
        EOF
        echo "Database cleaned"
    else
        echo "Cancelled"
    fi

redis-clean:
    #!/usr/bin/env bash
    set -euo pipefail
    source .env
    echo "WARNING: This will delete Firecrawl Redis keys (nuq:* and firecrawl:*)"
    read -p "Are you sure? (yes/no): " confirm
    if [ "$confirm" = "yes" ]; then
        redis-cli -u "${REDIS_URL}" --scan --pattern 'nuq:*' | xargs -r -n 100 redis-cli -u "${REDIS_URL}" DEL >/dev/null
        redis-cli -u "${REDIS_URL}" --scan --pattern 'firecrawl:*' | xargs -r -n 100 redis-cli -u "${REDIS_URL}" DEL >/dev/null
        echo "Redis namespace keys cleaned"
    else
        echo "Cancelled"
    fi

redis-flush-all:
    #!/usr/bin/env bash
    set -euo pipefail
    source .env
    echo "WARNING: This will FLUSHDB for REDIS_URL=${REDIS_URL}"
    read -p "Are you sure? (yes/no): " confirm
    if [ "$confirm" = "yes" ]; then
        redis-cli -u "${REDIS_URL}" FLUSHDB
        echo "Redis DB flushed"
    else
        echo "Cancelled"
    fi

rabbit-clean:
    #!/usr/bin/env bash
    set -euo pipefail
    source .env
    echo "Attempting RabbitMQ cleanup (if rabbitmqadmin is installed)"
    if command -v rabbitmqadmin >/dev/null 2>&1; then
        rabbitmqadmin --url="${NUQ_RABBITMQ_URL}" purge queue name=default || true
        rabbitmqadmin --url="${NUQ_RABBITMQ_URL}" purge queue name=firecrawl || true
        echo "RabbitMQ queue purge attempted"
    else
        echo "rabbitmqadmin not found; install it to enable RabbitMQ purge"
    fi

data-clean: db-clean redis-clean rabbit-clean
    @echo "Data cleanup complete"

# ── Production ─────────────────────────────────────────────────────────────

start:
    cargo run --release -- all

stop:
    pkill -f fire_ctrl || true

# ── Misc ────────────────────────────────────────────────────────────────────

clean:
    cargo clean

docs:
    cargo doc --no-deps

docs-open:
    cargo doc --no-deps --open

update-deps:
    cargo update

audit:
    cargo audit

deny:
    #!/usr/bin/env bash
    if ! command -v cargo-deny &> /dev/null; then
        echo "cargo-deny not found — install via: cargo install cargo-deny"
        exit 1
    fi
    cargo deny check

# Run the full local security suite (audit + deny)
security: audit deny
    @echo "Security checks passed!"

bench:
    #!/usr/bin/env bash
    if ! command -v hurl &> /dev/null; then
        echo "hurl not found — install via: brew install hurl"
        exit 1
    fi
    hurl --repeat 1000 --parallel --jobs 10 --very-verbose bench/health.hurl 2>&1 | tail -20

bench-scrape:
    #!/usr/bin/env bash
    if ! command -v hurl &> /dev/null; then
        echo "hurl not found — install via: brew install hurl"
        exit 1
    fi
    hurl --repeat 100 --parallel --jobs 5 bench/scrape.hurl

init-env:
    #!/usr/bin/env bash
    if [ ! -f .env ]; then
        cp .env.example .env
        echo ".env created — edit it with your credentials"
    else
        echo ".env already exists"
    fi

setup: init-env migrate build-release
    @echo "Setup complete — run: just start"
