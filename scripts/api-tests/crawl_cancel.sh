#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

crawl_id="$(create_crawl_id)"
f="$(new_tmp)"
code="$(request DELETE "/v2/crawl/$crawl_id" "$f")"
expect_code "$code" "200" "/v2/crawl/{id} cancel" "$f"
assert_jq "$f" '.status == "cancelled"' "/v2/crawl/{id} cancel body"
echo "PASS /v2/crawl/{id} cancel"
