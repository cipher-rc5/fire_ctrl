# Usage Guide (v2 API)

This file documents practical request patterns for all supported runtime
functionality.

Base URL used below:

```bash
export FIRECRAWL_URL="http://localhost:3002"
```

Optional auth header (when `USE_DB_AUTHENTICATION=true`):

```bash
export FIRECRAWL_API_KEY="<your-key>"
export FIRECRAWL_AUTH_HEADER="Authorization: Bearer $FIRECRAWL_API_KEY"
```

## 1) Health

```bash
curl -sS "$FIRECRAWL_URL/health" | jq
```

## 2) Scrape (`/v2/scrape`)

Scrape as markdown:

```bash
curl -sS -X POST "$FIRECRAWL_URL/v2/scrape" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://www.scrapethissite.com/pages/simple",
    "formats": ["markdown"],
    "only_main_content": true
  }' \
  | jq '{success, title: .data.title, markdownPreview: (.data.markdown // "" | split("\n") | .[:20] | join("\n"))}'
```

Scrape as structured JSON:

```bash
curl -sS -X POST "$FIRECRAWL_URL/v2/scrape" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://www.scrapethissite.com/pages/simple",
    "formats": ["json"]
  }' \
  | jq '{success, title: .data.title, json: .data.json}'
```

Scrape as markdown + JSON:

```bash
curl -sS -X POST "$FIRECRAWL_URL/v2/scrape" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://www.scrapethissite.com/pages/simple",
    "formats": ["markdown", "json"],
    "only_main_content": true
  }' \
  | jq '{
      success,
      title: .data.title,
      markdownPreview: (.data.markdown // "" | split("\n") | .[:12] | join("\n")),
      jsonKeys: (.data.json | keys)
    }'
```

Inline extraction inside scrape:

```bash
curl -sS -X POST "$FIRECRAWL_URL/v2/scrape" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://www.scrapethissite.com/pages/simple",
    "formats": ["extract"],
    "extract": {
      "prompt": "Extract the first 10 countries with name and capital.",
      "schema": {
        "type": "object",
        "properties": {
          "countries": {
            "type": "array",
            "items": {
              "type": "object",
              "properties": {
                "name": {"type": "string"},
                "capital": {"type": "string"}
              }
            }
          }
        }
      }
    }
  }' | jq '{warning: .data.warning, extract: .data.extract}'
```

If input content exceeds `LLM_MAX_INPUT_CHARS`, the response can include
`data.warning` for truncation/provider retry behavior.

## 3) Crawl (`/v2/crawl*`)

Start crawl:

```bash
CRAWL_ID=$(curl -sS -X POST "$FIRECRAWL_URL/v2/crawl" \
  -H "Content-Type: application/json" \
  -d '{
    "url":"https://example.com",
    "limit":5,
    "maxDepth":1,
    "scrapeOptions":{"formats":["markdown"]}
  }' \
  | jq -r '.id')
echo "crawl id: $CRAWL_ID"
```

Get status:

```bash
curl -sS "$FIRECRAWL_URL/v2/crawl/$CRAWL_ID" | jq
```

Get paginated status page:

```bash
curl -sS "$FIRECRAWL_URL/v2/crawl/$CRAWL_ID?limit=10" | jq
```

Get errors:

```bash
curl -sS "$FIRECRAWL_URL/v2/crawl/$CRAWL_ID/errors" | jq
```

Get ongoing crawls:

```bash
curl -sS "$FIRECRAWL_URL/v2/crawl/ongoing" | jq
```

Cancel crawl:

```bash
curl -sS -X DELETE "$FIRECRAWL_URL/v2/crawl/$CRAWL_ID" | jq
```

## 4) Batch scrape (`/v2/batch/scrape*`)

Start batch scrape:

```bash
BATCH_ID=$(curl -sS -X POST "$FIRECRAWL_URL/v2/batch/scrape" \
  -H "Content-Type: application/json" \
  -d '{
    "urls": [
      "https://example.com",
      "https://www.rust-lang.org"
    ],
    "scrapeOptions": {"formats": ["markdown"]}
  }' | jq -r '.id')
echo "batch id: $BATCH_ID"
```

Get batch status:

```bash
curl -sS "$FIRECRAWL_URL/v2/batch/scrape/$BATCH_ID" | jq
```

Cancel batch:

```bash
curl -sS -X DELETE "$FIRECRAWL_URL/v2/batch/scrape/$BATCH_ID" | jq
```

## 5) Map (`/v2/map`)

```bash
curl -sS -X POST "$FIRECRAWL_URL/v2/map" \
  -H "Content-Type: application/json" \
  -d '{
    "url":"https://example.com",
    "includeSubdomains":false,
    "ignoreSitemap":false,
    "limit":20
  }' | jq '{success, count: (.links | length), first: .links[0]}'
```

## 6) Extract (`/v2/extract*`, async)

Start extract job:

```bash
EXTRACT_ID=$(curl -sS -X POST "$FIRECRAWL_URL/v2/extract" \
  -H "Content-Type: application/json" \
  -d '{
    "urls": ["https://example.com"],
    "prompt": "Return company name and a one-line summary",
    "schema": {
      "type": "object",
      "properties": {
        "company": {"type": "string"},
        "summary": {"type": "string"}
      },
      "required": ["company", "summary"]
    }
  }' | jq -r '.id')
echo "extract id: $EXTRACT_ID"
```

Poll extract status:

```bash
curl -sS "$FIRECRAWL_URL/v2/extract/$EXTRACT_ID" | jq
```

## 7) Search (`/v2/search`)

Basic search:

```bash
curl -sS -X POST "$FIRECRAWL_URL/v2/search" \
  -H "Content-Type: application/json" \
  -d '{"query":"rust web scraping", "limit":3}' \
  | jq '{success, count: (.data.web | length), first: .data.web[0].url}'
```

Search with scrape options:

```bash
curl -sS -X POST "$FIRECRAWL_URL/v2/search" \
  -H "Content-Type: application/json" \
  -d '{
    "query":"rust async tutorial",
    "limit":3,
    "scrapeOptions": {
      "formats": ["markdown"],
      "onlyMainContent": true
    }
  }' | jq
```

## 8) Endpoint smoke suite

Run all endpoint smoke tests against `quotes.toscrape.com`:

```bash
just api-suite-quotes
```

Run one endpoint test at a time:

```bash
just api-test health
just api-test scrape
just api-test crawl_status
```

Control output verbosity:

```bash
SHOW_OUTPUT=1 just api-test health
SHOW_OUTPUT=0 just api-test health
```

## 9) Developer helpers

```bash
just scrape-markdown
just scrape-json
just scrape-both
```

## 10) Cleanup operations

```bash
just db-clean         # truncate Postgres job/state tables
just redis-clean      # delete Firecrawl Redis keys only
just redis-flush-all  # flush current Redis DB
just rabbit-clean     # purge Rabbit queues (if rabbitmqadmin installed)
just data-clean       # run all cleanup steps above
```

## Notes

- Config is strict/fail-fast; keep `.env` aligned with `.env.example`.
- Webhook calls are blocked for local addresses unless `ALLOW_LOCAL_WEBHOOKS=true`.
- Contract tests are opt-in: `RUN_FIRECRAWL_CONTRACT_TESTS=1 just contract-test`.
