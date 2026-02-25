#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

crawl_id="$(create_crawl_id)"
f="$(new_tmp)"
code="$(request GET "/v2/crawl/$crawl_id/errors" "$f")"
expect_code "$code" "200" "/v2/crawl/{id}/errors" "$f"
assert_jq "$f" '.errors | type == "array"' "/v2/crawl/{id}/errors shape"
echo "PASS /v2/crawl/{id}/errors"
