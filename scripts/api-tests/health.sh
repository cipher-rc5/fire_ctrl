#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

f="$(new_tmp)"
code="$(request GET "/health" "$f")"
expect_code "$code" "200" "/health" "$f"
assert_jq "$f" '.status | type == "string"' "/health.status"
echo "PASS /health"
