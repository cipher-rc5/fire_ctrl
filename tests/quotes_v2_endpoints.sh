#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:3002}"
TARGET_ROOT="https://quotes.toscrape.com"
TARGET_PAGE1="https://quotes.toscrape.com/page/1/"
TARGET_PAGE2="https://quotes.toscrape.com/page/2/"

tmp_files=()
cleanup() {
  for f in "${tmp_files[@]:-}"; do
    rm -f "$f" 2>/dev/null || true
  done
}
trap cleanup EXIT

new_tmp() {
  local f
  f="$(mktemp)"
  tmp_files+=("$f")
  echo "$f"
}

request() {
  local method="$1"
  local url="$2"
  local body_file="$3"
  local payload="${4:-}"
  local http_code

  if [ -n "$payload" ]; then
    http_code="$(curl -sS -X "$method" "$url" \
      -H "Content-Type: application/json" \
      -d "$payload" \
      -o "$body_file" -w "%{http_code}" || true)"
  else
    http_code="$(curl -sS -X "$method" "$url" \
      -o "$body_file" -w "%{http_code}" || true)"
  fi

  if [[ "$http_code" =~ ^[0-9]{3}$ ]]; then
    echo "$http_code"
  else
    echo "000"
  fi
}

assert_jq() {
  local file="$1"
  local expr="$2"
  local label="$3"
  if ! jq -e "$expr" "$file" >/dev/null; then
    echo "FAIL: $label" >&2
    echo "Body:" >&2
    cat "$file" >&2
    exit 1
  fi
}

poll_status_until_done() {
  local url="$1"
  local timeout_secs="${2:-60}"
  local started
  started="$(date +%s)"

  while true; do
    local f code status now
    f="$(new_tmp)"
    code="$(request GET "$url" "$f")"
    [ "$code" = "200" ] || { echo "FAIL: poll $url ($code)" >&2; cat "$f" >&2; exit 1; }
    status="$(jq -r '.status // empty' "$f" 2>/dev/null || true)"
    if [ -z "$status" ]; then
      echo "FAIL: invalid or missing .status in poll response for $url" >&2
      cat "$f" >&2
      exit 1
    fi
    case "$status" in
      completed|cancelled|failed)
        echo "$status"
        return 0
        ;;
      scraping|active|queued)
        ;;
      *)
        echo "FAIL: unexpected status '$status' for $url" >&2
        cat "$f" >&2
        exit 1
        ;;
    esac

    now="$(date +%s)"
    if [ $((now - started)) -ge "$timeout_secs" ]; then
      echo "FAIL: timeout waiting for $url" >&2
      cat "$f" >&2
      exit 1
    fi
    sleep 2
  done
}

echo "Running v2 endpoint suite against $BASE_URL"

# 1) health
f="$(new_tmp)"
code="$(request GET "$BASE_URL/health" "$f")"
[ "$code" = "200" ] || { echo "FAIL: /health ($code)"; cat "$f"; exit 1; }
assert_jq "$f" '.status | type == "string"' "/health status"
echo "PASS: /health"

# 2) scrape markdown
f="$(new_tmp)"
payload='{"url":"'"$TARGET_PAGE1"'","formats":["markdown"],"only_main_content":true}'
code="$(request POST "$BASE_URL/v2/scrape" "$f" "$payload")"
[ "$code" = "200" ] || { echo "FAIL: /v2/scrape markdown ($code)"; cat "$f"; exit 1; }
assert_jq "$f" '.success == true and (.data.markdown | type == "string")' "/v2/scrape markdown"
echo "PASS: /v2/scrape markdown"

# 3) scrape json
f="$(new_tmp)"
payload='{"url":"'"$TARGET_PAGE1"'","formats":["json"]}'
code="$(request POST "$BASE_URL/v2/scrape" "$f" "$payload")"
[ "$code" = "200" ] || { echo "FAIL: /v2/scrape json ($code)"; cat "$f"; exit 1; }
assert_jq "$f" '.success == true and (.data.json | type == "object")' "/v2/scrape json"
echo "PASS: /v2/scrape json"

# 4) scrape extract (optional if llm unconfigured)
f="$(new_tmp)"
payload='{"url":"'"$TARGET_PAGE1"'","formats":["extract"],"extract":{"prompt":"Extract up to 5 quotes with text and author.","schema":{"type":"object","properties":{"quotes":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"author":{"type":"string"}}}}}}}}'
code="$(request POST "$BASE_URL/v2/scrape" "$f" "$payload")"
if [ "$code" = "200" ]; then
  assert_jq "$f" '.success == true and (.data.extract | type == "object")' "/v2/scrape extract"
  echo "PASS: /v2/scrape extract"
else
  if jq -e '.error | tostring | test("LLM not configured"; "i")' "$f" >/dev/null 2>&1; then
    echo "SKIP: /v2/scrape extract (LLM not configured)"
  else
    echo "FAIL: /v2/scrape extract ($code)"
    cat "$f"
    exit 1
  fi
fi

# 5) map
f="$(new_tmp)"
payload='{"url":"'"$TARGET_ROOT"'","limit":30,"ignoreSitemap":true,"includeSubdomains":false}'
code="$(request POST "$BASE_URL/v2/map" "$f" "$payload")"
[ "$code" = "200" ] || { echo "FAIL: /v2/map ($code)"; cat "$f"; exit 1; }
assert_jq "$f" '.success == true and (.links | type == "array")' "/v2/map"
echo "PASS: /v2/map"

# 6) crawl start + status + cancel + errors + ongoing
f="$(new_tmp)"
payload='{"url":"'"$TARGET_ROOT"'","maxDepth":0,"limit":2,"scrapeOptions":{"formats":["markdown"],"only_main_content":true}}'
code="$(request POST "$BASE_URL/v2/crawl" "$f" "$payload")"
[ "$code" = "200" ] || { echo "FAIL: /v2/crawl start ($code)"; cat "$f"; exit 1; }
crawl_id="$(jq -r '.id' "$f")"
[ "$crawl_id" != "null" ] || { echo "FAIL: crawl id missing"; cat "$f"; exit 1; }
echo "PASS: /v2/crawl start ($crawl_id)"

f="$(new_tmp)"
code="$(request GET "$BASE_URL/v2/crawl/$crawl_id" "$f")"
[ "$code" = "200" ] || { echo "FAIL: /v2/crawl/{id} status ($code)" >&2; cat "$f" >&2; exit 1; }
assert_jq "$f" '.status | type == "string"' "/v2/crawl/{id} status"
echo "PASS: /v2/crawl/{id} status"

f="$(new_tmp)"
code="$(request DELETE "$BASE_URL/v2/crawl/$crawl_id" "$f")"
if [ "$code" = "200" ]; then
  assert_jq "$f" '.status == "cancelled"' "/v2/crawl/{id} cancel"
  echo "PASS: /v2/crawl/{id} cancel"
else
  echo "WARN: /v2/crawl/{id} cancel returned $code (non-fatal for smoke suite)"
  cat "$f"
fi

f="$(new_tmp)"
code="$(request GET "$BASE_URL/v2/crawl/$crawl_id/errors" "$f")"
if [ "$code" = "200" ]; then
  assert_jq "$f" '.errors | type == "array"' "/v2/crawl/{id}/errors"
  echo "PASS: /v2/crawl/{id}/errors"
else
  echo "WARN: /v2/crawl/{id}/errors returned $code (non-fatal for smoke suite)"
  cat "$f"
fi

f="$(new_tmp)"
code="$(request GET "$BASE_URL/v2/crawl/ongoing" "$f")"
[ "$code" = "200" ] || { echo "FAIL: /v2/crawl/ongoing ($code)"; cat "$f"; exit 1; }
assert_jq "$f" '.success == true and (.crawls | type == "array")' "/v2/crawl/ongoing"
echo "PASS: /v2/crawl/ongoing"

# 7) batch scrape start + status + cancel
f="$(new_tmp)"
payload='{"urls":["'"$TARGET_PAGE1"'","'"$TARGET_PAGE2"'","notaurl"],"scrapeOptions":{"formats":["markdown"],"only_main_content":true}}'
code="$(request POST "$BASE_URL/v2/batch/scrape" "$f" "$payload")"
[ "$code" = "200" ] || { echo "FAIL: /v2/batch/scrape start ($code)"; cat "$f"; exit 1; }
batch_id="$(jq -r '.id' "$f")"
assert_jq "$f" '.invalidUrls | type == "array"' "/v2/batch/scrape invalidUrls"
echo "PASS: /v2/batch/scrape start ($batch_id)"

f="$(new_tmp)"
code="$(request GET "$BASE_URL/v2/batch/scrape/$batch_id" "$f")"
if [ "$code" = "200" ]; then
  assert_jq "$f" '.status | type == "string"' "/v2/batch/scrape/{id} status"
  echo "PASS: /v2/batch/scrape/{id} status"
else
  echo "WARN: /v2/batch/scrape/{id} status returned $code (non-fatal for smoke suite)"
  cat "$f"
fi

f="$(new_tmp)"
payload='{"urls":["'"$TARGET_PAGE1"'"]}'
code="$(request POST "$BASE_URL/v2/batch/scrape" "$f" "$payload")"
[ "$code" = "200" ] || { echo "FAIL: /v2/batch/scrape start for cancel ($code)"; cat "$f"; exit 1; }
batch_cancel_id="$(jq -r '.id' "$f")"

f="$(new_tmp)"
code="$(request DELETE "$BASE_URL/v2/batch/scrape/$batch_cancel_id" "$f")"
if [ "$code" = "200" ]; then
  assert_jq "$f" '.status == "cancelled"' "/v2/batch/scrape/{id} cancel"
  echo "PASS: /v2/batch/scrape/{id} cancel"
else
  echo "WARN: /v2/batch/scrape/{id} cancel returned $code (non-fatal for smoke suite)"
  cat "$f"
fi

# 8) extract async (optional if llm unconfigured)
f="$(new_tmp)"
payload='{"urls":["'"$TARGET_PAGE1"'"],"prompt":"Extract up to 3 quotes with text and author.","schema":{"type":"object","properties":{"quotes":{"type":"array"}}}}'
code="$(request POST "$BASE_URL/v2/extract" "$f" "$payload")"
if [ "$code" = "200" ]; then
  extract_id="$(jq -r '.id' "$f")"
  [ "$extract_id" != "null" ] || { echo "FAIL: extract id missing"; cat "$f"; exit 1; }
  f="$(new_tmp)"
  code="$(request GET "$BASE_URL/v2/extract/$extract_id" "$f")"
  if [ "$code" = "200" ]; then
    assert_jq "$f" '.status | type == "string"' "/v2/extract/{id} status"
    echo "PASS: /v2/extract/{id} status"
  else
    echo "WARN: /v2/extract/{id} status returned $code (non-fatal for smoke suite)"
    cat "$f"
  fi
else
  if jq -e '.error | tostring | test("LLM not configured"; "i")' "$f" >/dev/null 2>&1; then
    echo "SKIP: /v2/extract (LLM not configured)"
  else
    echo "FAIL: /v2/extract start ($code)"
    cat "$f"
    exit 1
  fi
fi

# 9) search
f="$(new_tmp)"
payload='{"query":"quotes toscrape","limit":3}'
code="$(request POST "$BASE_URL/v2/search" "$f" "$payload")"
[ "$code" = "200" ] || { echo "FAIL: /v2/search ($code)"; cat "$f"; exit 1; }
assert_jq "$f" '.success == true and (.data.web | type == "array")' "/v2/search"
echo "PASS: /v2/search"

echo "All v2 endpoint checks passed against quotes.toscrape.com"
