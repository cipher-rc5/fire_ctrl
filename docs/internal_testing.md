# internal_testing

spec-compliant self-hosted Firecrawl in Rust

file to be omitted at git upload, leaving for now

sample one

```bash
curl -sS -X POST "http://localhost:3002/v2/scrape" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://www.scrapethissite.com/pages/simple",
    "formats": ["markdown"],
    "only_main_content": false
  }' \
  | jq '{
      success,
      title: .data.title,
      markdownPreview: (.data.markdown // "" | split("\n") | .[:12] | join("\n"))
    }'
```

sample two
```bash
curl -X POST http://localhost:3002/v2/scrape \
  -H "Content-Type: application/json" \
  -d '{"url": "https://www.scrapethissite.com/pages/simple"}' | jq
```


```bash
curl -sS -X POST http://localhost:3002/v2/scrape \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://www.scrapethissite.com/pages/simple",
    "formats": ["extract"],
    "extract": {
      "prompt": "Extract the first 50 countries with name, capital, population, and area_km2.",
      "schema": {
        "type": "object",
        "properties": {
          "countries": {
            "type": "array",
            "items": {
              "type": "object",
              "properties": {
                "name": {"type": "string"},
                "capital": {"type": "string"},
                "population": {"type": "number"},
                "area_km2": {"type": "number"}
              }
            }
          }
        }
      }
    }
  }' | jq '{warning: .data.warning, extract: .data.extract}'
  ```
