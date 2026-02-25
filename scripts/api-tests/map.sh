#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

f="$(new_tmp)"
code="$(request POST "/v2/map" "$f" '{"url":"'"$TARGET_ROOT"'","limit":25,"ignoreSitemap":true,"includeSubdomains":false}')"
expect_code "$code" "200" "/v2/map" "$f"
assert_jq "$f" '.success == true and (.links | type == "array")' "/v2/map shape"
echo "PASS /v2/map"
