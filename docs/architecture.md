# Architecture

## High-level components

```mermaid
flowchart TB
    subgraph ClientSide[Clients]
        SDK[Firecrawl SDK]
        CURL[curl / scripts]
    end

    subgraph Service[fire_ctrl binary]
        API[Axum API\n`src/api.rs`]
        Worker[WorkerPool\n`src/worker.rs`]
        Scraper[Scraper + BrowserPool\n`src/scraper.rs`]
        LLM[LlmClient\n`src/llm.rs`]
    end

    subgraph State[State and infrastructure]
        PG[(PostgreSQL + Timescale\n`nuq.queue_scrape`)]
        REDIS[(Redis)]
    end

    WEB[(Target websites)]
    MODEL[(OpenAI-compatible model endpoint)]

    SDK --> API
    CURL --> API
    API --> Scraper
    API --> PG
    API --> REDIS
    Worker --> PG
    Worker --> Scraper
    Scraper --> WEB
    Worker --> LLM
    API --> LLM
    LLM --> MODEL
```

The job queue is Postgres-backed: the API enqueues rows into `nuq.queue_scrape`, and workers claim work with `SELECT ... FOR UPDATE SKIP LOCKED`. There is no external message broker.

## Request lifecycle (`/v2/crawl`)

```mermaid
sequenceDiagram
    participant C as Client
    participant A as API (`/v2/crawl`)
    participant D as DbClient (PG queue)
    participant W as Worker
    participant S as Scraper

    C->>A: POST /v2/crawl
    A->>A: check_auth + check_resources
    A->>D: create crawl group + enqueue job into `nuq.queue_scrape`
    A-->>C: 200 with crawl id

    W->>D: claim job (SELECT ... FOR UPDATE SKIP LOCKED)
    W->>S: fetch and scrape URLs
    W->>D: persist status/results

    C->>A: GET /v2/crawl/:id
    A->>D: read group status + paginated results
    A-->>C: status, counts, data, next
```

## Route coverage

- Synchronous routes: `/health`, `/v2/scrape`, `/v2/map`, `/v2/search`
- Async job routes: `/v2/crawl*`, `/v2/batch/scrape*`, `/v2/extract*`
- Auth gate applies when `USE_DB_AUTHENTICATION=true`
- Resource gate applies to heavier routes (`scrape`, `crawl`, `batch`, `search`)
