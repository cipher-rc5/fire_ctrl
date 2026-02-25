#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

f="$(new_tmp)"
payload='{"urls":["'"$TARGET_PAGE1"'"],"prompt":"Extract up to 3 quotes with text and author.","schema":{"type":"object","properties":{"quotes":{"type":"array"}}}}'
code="$(request POST "/v2/extract" "$f" "$payload")"

if [ "$code" = "200" ]; then
  assert_jq "$f" '.success == true and (.id | type == "string")' "/v2/extract create shape"
  echo "PASS /v2/extract create"
else
  if jq -e '.error | tostring | test("LLM not configured"; "i")' "$f" >/dev/null 2>&1; then
    echo "SKIP /v2/extract create (LLM not configured)"
    exit 0
  fi
  expect_code "$code" "200" "/v2/extract create" "$f"
fi
