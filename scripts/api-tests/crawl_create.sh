#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

f="$(new_tmp)"
code="$(request POST "/v2/crawl" "$f" '{"url":"'"$TARGET_ROOT"'","maxDepth":0,"limit":2}')"
expect_code "$code" "200" "/v2/crawl create" "$f"
assert_jq "$f" '.success == true and (.id | type == "string")' "/v2/crawl create shape"
echo "PASS /v2/crawl create"
