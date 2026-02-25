#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:3002}"
TARGET_ROOT="${TARGET_ROOT:-https://quotes.toscrape.com}"
TARGET_PAGE1="${TARGET_PAGE1:-https://quotes.toscrape.com/page/1/}"
TARGET_PAGE2="${TARGET_PAGE2:-https://quotes.toscrape.com/page/2/}"
SHOW_OUTPUT="${SHOW_OUTPUT:-1}"

tmp_files=()

cleanup_tmp() {
  for f in "${tmp_files[@]:-}"; do
    rm -f "$f" 2>/dev/null || true
  done
}

new_tmp() {
  local f
  f="$(mktemp)"
  tmp_files+=("$f")
  echo "$f"
}

request() {
  local method="$1"
  local path="$2"
  local out_file="$3"
  local payload="${4:-}"
  local show_exchange="${5:-1}"
  local url="$BASE_URL$path"
  local code

  if [ -n "$payload" ]; then
    code="$(curl -sS -X "$method" "$url" -H "Content-Type: application/json" -d "$payload" -o "$out_file" -w "%{http_code}" || true)"
  else
    code="$(curl -sS -X "$method" "$url" -o "$out_file" -w "%{http_code}" || true)"
  fi

  if ! [[ "$code" =~ ^[0-9]{3}$ ]]; then
    code="000"
  fi

  if [ "$SHOW_OUTPUT" = "1" ] && [ "$show_exchange" = "1" ]; then
    echo "--- request ---" >&2
    echo "${method} ${url}" >&2
    if [ -n "$payload" ]; then
      (echo "$payload" | jq . 2>/dev/null || echo "$payload") >&2
    else
      echo "(no body)" >&2
    fi
    echo "--- response ($code) ---" >&2
    (jq . "$out_file" 2>/dev/null || cat "$out_file") >&2
    echo >&2
  fi

  echo "$code"
}

expect_code() {
  local code="$1"
  local want="$2"
  local label="$3"
  local body_file="$4"
  if [ "$code" != "$want" ]; then
    echo "FAIL: $label (expected $want got $code)" >&2
    cat "$body_file" >&2
    exit 1
  fi
}

assert_jq() {
  local file="$1"
  local expr="$2"
  local label="$3"
  if ! jq -e "$expr" "$file" >/dev/null; then
    echo "FAIL: $label" >&2
    cat "$file" >&2
    exit 1
  fi

  # Request/response echo is handled in `request()`.
}

create_crawl_id() {
  local f code
  f="$(new_tmp)"
  code="$(request POST "/v2/crawl" "$f" '{"url":"'"$TARGET_ROOT"'","maxDepth":0,"limit":2,"scrapeOptions":{"formats":["markdown"],"only_main_content":true}}')"
  expect_code "$code" "200" "create crawl" "$f"
  jq -r '.id' "$f"
}

create_batch_id() {
  local f code
  f="$(new_tmp)"
  code="$(request POST "/v2/batch/scrape" "$f" '{"urls":["'"$TARGET_PAGE1"'","'"$TARGET_PAGE2"'"],"scrapeOptions":{"formats":["markdown"],"only_main_content":true}}')"
  expect_code "$code" "200" "create batch" "$f"
  jq -r '.id' "$f"
}
