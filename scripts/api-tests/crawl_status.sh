#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

crawl_id="$(create_crawl_id)"
f="$(new_tmp)"
code="$(request GET "/v2/crawl/$crawl_id" "$f")"
expect_code "$code" "200" "/v2/crawl/{id} status" "$f"
assert_jq "$f" '.status | type == "string"' "/v2/crawl/{id}.status"
echo "PASS /v2/crawl/{id} status"
