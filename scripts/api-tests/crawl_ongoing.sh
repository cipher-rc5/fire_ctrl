#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

f="$(new_tmp)"
code="$(request GET "/v2/crawl/ongoing" "$f")"
expect_code "$code" "200" "/v2/crawl/ongoing" "$f"
assert_jq "$f" '.success == true and (.crawls | type == "array")' "/v2/crawl/ongoing shape"
echo "PASS /v2/crawl/ongoing"
