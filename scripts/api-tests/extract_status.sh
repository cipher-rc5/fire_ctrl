#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

f="$(new_tmp)"
payload='{"urls":["'"$TARGET_PAGE1"'"],"prompt":"Extract up to 3 quotes with text and author.","schema":{"type":"object","properties":{"quotes":{"type":"array"}}}}'
code="$(request POST "/v2/extract" "$f" "$payload")"

if [ "$code" != "200" ]; then
  if jq -e '.error | tostring | test("LLM not configured"; "i")' "$f" >/dev/null 2>&1; then
    echo "SKIP /v2/extract/{id} status (LLM not configured)"
    exit 0
  fi
  expect_code "$code" "200" "/v2/extract create for status" "$f"
fi

extract_id="$(jq -r '.id' "$f")"
f2="$(new_tmp)"
code2="$(request GET "/v2/extract/$extract_id" "$f2")"
expect_code "$code2" "200" "/v2/extract/{id} status" "$f2"
assert_jq "$f2" '.status | type == "string"' "/v2/extract/{id}.status"
echo "PASS /v2/extract/{id} status"
