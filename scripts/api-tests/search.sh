#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

f="$(new_tmp)"
code="$(request POST "/v2/search" "$f" '{"query":"quotes toscrape","limit":3}')"
expect_code "$code" "200" "/v2/search" "$f"
assert_jq "$f" '.success == true and (.data.web | type == "array")' "/v2/search shape"
echo "PASS /v2/search"
