#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TEST_DIR="$ROOT_DIR/scripts/api-tests"

echo "Running per-endpoint API scripts against ${BASE_URL:-http://localhost:3002}"

for script in \
  health.sh \
  scrape.sh \
  map.sh \
  crawl_create.sh \
  crawl_status.sh \
  crawl_ongoing.sh \
  crawl_errors.sh \
  crawl_cancel.sh \
  batch_create.sh \
  batch_status.sh \
  batch_cancel.sh \
  extract_create.sh \
  extract_status.sh \
  search.sh
do
  echo "===== $script ====="
  bash "$TEST_DIR/$script"
done

echo "All endpoint scripts completed"
