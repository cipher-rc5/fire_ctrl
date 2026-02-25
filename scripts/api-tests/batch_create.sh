#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

f="$(new_tmp)"
code="$(request POST "/v2/batch/scrape" "$f" '{"urls":["'"$TARGET_PAGE1"'","'"$TARGET_PAGE2"'","notaurl"]}')"
expect_code "$code" "200" "/v2/batch/scrape create" "$f"
assert_jq "$f" '.success == true and (.id | type == "string") and (.invalidUrls | type == "array")' "/v2/batch/scrape create shape"
echo "PASS /v2/batch/scrape create"
