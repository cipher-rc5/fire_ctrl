# Benchmark: POST /v2/scrape
# Usage: hurl --repeat 100 --parallel --jobs 5 bench/scrape.hurl
# Or via justfile: just bench-scrape

POST http://localhost:3002/v2/scrape
Content-Type: application/json
{
  "url": "https://www.scrapethissite.com/pages/simple",
  "formats": ["markdown"]
}

HTTP 200
[Asserts]
jsonpath "$.success" == true
jsonpath "$.data.url" exists
