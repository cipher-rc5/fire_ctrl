# Benchmark: GET /health
# Usage: hurl --repeat 1000 --parallel --jobs 10 bench/health.hurl
# Or via justfile: just bench

GET http://localhost:3002/health

HTTP 200
[Asserts]
jsonpath "$.status" exists
jsonpath "$.services.database.healthy" == true
