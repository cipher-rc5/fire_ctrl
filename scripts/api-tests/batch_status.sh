#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

batch_id="$(create_batch_id)"
f="$(new_tmp)"
code="$(request GET "/v2/batch/scrape/$batch_id" "$f")"
expect_code "$code" "200" "/v2/batch/scrape/{id} status" "$f"
assert_jq "$f" '.status | type == "string"' "/v2/batch/scrape/{id}.status"
echo "PASS /v2/batch/scrape/{id} status"
