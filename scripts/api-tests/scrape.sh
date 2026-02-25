#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/_lib.sh"
trap cleanup_tmp EXIT

FORMAT="${1:-markdown}"
case "$FORMAT" in
  markdown|json) ;;
  *)
    echo "Usage: $0 [markdown|json]" >&2
    exit 2
    ;;
esac

payload="$(jq -nc --arg url "$TARGET_PAGE1" --arg format "$FORMAT" '{url: $url, formats: [$format]}')"

f="$(new_tmp)"
code="$(request POST "/v2/scrape" "$f" "$payload")"
expect_code "$code" "200" "/v2/scrape" "$f"
assert_jq "$f" '.success == true and (.data.url | type == "string")' "/v2/scrape shape"

if [ "$FORMAT" = "markdown" ]; then
  assert_jq "$f" '(.data.markdown | type == "string")' "/v2/scrape markdown present"
  echo "PASS /v2/scrape markdown"
  jq -r '.data.markdown' "$f"
else
  assert_jq "$f" '(.data.json | type == "object")' "/v2/scrape json present"
  echo "PASS /v2/scrape json"
  jq '.data.json' "$f"
fi
