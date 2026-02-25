/// file: tests/sdk_v2_contract.rs
/// description: Optional Firecrawl SDK v2 compatibility contract integration tests.
use std::time::{Duration, Instant};

use firecrawl::v2::{Client, CrawlOptions, Format, JobStatus, ScrapeOptions};

fn contract_enabled() -> bool {
    std::env::var("RUN_FIRECRAWL_CONTRACT_TESTS")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn base_url() -> String {
    std::env::var("FIRECRAWL_BASE_URL").unwrap_or_else(|_| "http://localhost:3002".to_string())
}

fn client() -> Client {
    Client::new_selfhosted(base_url(), None::<&str>).expect("failed to create v2 SDK client")
}

#[tokio::test]
async fn sdk_v2_scrape_markdown_and_json() {
    if !contract_enabled() {
        return;
    }

    let c = client();
    let opts = ScrapeOptions {
        formats: Some(vec![Format::Markdown, Format::Json]),
        only_main_content: Some(true),
        ..Default::default()
    };

    let doc = c
        .scrape("https://www.scrapethissite.com/pages/simple", opts)
        .await
        .expect("v2 scrape failed");

    assert!(
        doc.markdown.is_some(),
        "expected markdown in scrape response"
    );
    assert!(doc.json.is_some(), "expected json in scrape response");
}

#[tokio::test]
async fn sdk_v2_crawl_start_and_status() {
    if !contract_enabled() {
        return;
    }

    let c = client();
    let opts = CrawlOptions {
        limit: Some(1),
        max_discovery_depth: Some(0),
        poll_interval: Some(1000),
        ..Default::default()
    };

    let started = c
        .start_crawl("https://example.com", opts)
        .await
        .expect("failed to start v2 crawl");

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = c
            .get_crawl_status(&started.id)
            .await
            .expect("failed to get v2 crawl status");

        match status.status {
            JobStatus::Completed => {
                assert!(status.total >= status.completed);
                break;
            }
            JobStatus::Failed | JobStatus::Cancelled => {
                panic!("crawl ended unexpectedly with status: {:?}", status.status);
            }
            JobStatus::Scraping => {
                if Instant::now() > deadline {
                    panic!("timed out waiting for crawl completion");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}
