//! file: benches/scraper.rs
//! description: Criterion microbenchmarks for BFS frontier expansion and HTML→markdown.
//!
//! Not exercised in CI. Run locally with:
//!   `cargo bench --bench scraper`
//!
//! Baselines (committed under `benches/baselines/` once stable) provide a
//! coarse regression fence — Criterion's `--save-baseline` / `--baseline`
//! comparison is the intended workflow.

use criterion::{Criterion, criterion_group, criterion_main};
use fire_ctrl::scraper::{extract_links, html_to_markdown};
use std::collections::{HashSet, VecDeque};
use std::hint::black_box;
use url::Url;

// ---------------------------------------------------------------------------
// BFS frontier expansion over a synthetic 100-node internal-link graph.
// ---------------------------------------------------------------------------

fn build_synthetic_graph(node_count: usize) -> Vec<(String, Vec<String>)> {
    // Build a "site" where node N links to nodes (N+1), (N+2), and 0 (back to root).
    (0..node_count)
        .map(|i| {
            let url = format!("https://example.com/page/{i}");
            let mut out = Vec::new();
            for j in [i + 1, i + 2] {
                if j < node_count {
                    out.push(format!("https://example.com/page/{j}"));
                }
            }
            out.push("https://example.com/page/0".to_string());
            (url, out)
        })
        .collect()
}

fn bfs_frontier(graph: &[(String, Vec<String>)], root: &str, max_depth: u32) -> usize {
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    let mut visits = 0usize;

    visited.insert(root.to_string());
    queue.push_back((root.to_string(), 0));

    while let Some((url, depth)) = queue.pop_front() {
        visits += 1;
        if depth >= max_depth {
            continue;
        }
        if let Some((_, neighbours)) = graph.iter().find(|(k, _)| k == &url) {
            for n in neighbours {
                if visited.insert(n.clone()) {
                    queue.push_back((n.clone(), depth + 1));
                }
            }
        }
    }
    visits
}

fn bench_bfs_crawl_100_nodes(c: &mut Criterion) {
    let graph = build_synthetic_graph(100);
    c.bench_function("bfs_crawl_100_nodes", |b| {
        b.iter(|| {
            let n = bfs_frontier(black_box(&graph), "https://example.com/page/0", 5);
            black_box(n)
        });
    });
}

// ---------------------------------------------------------------------------
// HTML → markdown conversion on a representative ~50KB document.
// ---------------------------------------------------------------------------

fn synth_html_50kb() -> String {
    // Build a moderately-realistic page: nav, article with headings + lists +
    // paragraphs, and a few hundred outbound links so `extract_links` has
    // something meaty to chew on.
    let mut s = String::with_capacity(60_000);
    s.push_str("<!doctype html><html><head><title>Bench</title></head><body>");
    s.push_str("<header><nav>");
    for i in 0..50 {
        s.push_str(&format!("<a href=\"/nav/{i}\">nav {i}</a>"));
    }
    s.push_str("</nav></header><main><article>");
    for i in 0..40 {
        s.push_str(&format!("<h2>Section {i}</h2>"));
        s.push_str("<p>");
        s.push_str(&"Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(15));
        s.push_str("</p>");
        s.push_str("<ul>");
        for j in 0..5 {
            s.push_str(&format!(
                "<li><a href=\"/article/{i}/item/{j}\">item {j}</a></li>"
            ));
        }
        s.push_str("</ul>");
    }
    s.push_str("</article></main></body></html>");
    s
}

fn bench_markdown_conversion(c: &mut Criterion) {
    let html = synth_html_50kb();
    assert!(
        html.len() >= 45_000,
        "synth html should be ~50KB, got {} bytes",
        html.len()
    );
    c.bench_function("markdown_conversion_50kb", |b| {
        b.iter(|| {
            let md = html_to_markdown(black_box(&html));
            black_box(md)
        });
    });
}

fn bench_extract_links_50kb(c: &mut Criterion) {
    let html = synth_html_50kb();
    let base = Url::parse("https://example.com/").unwrap();
    c.bench_function("extract_links_50kb", |b| {
        b.iter(|| {
            let links = extract_links(black_box(&html), black_box(&base));
            black_box(links)
        });
    });
}

criterion_group!(
    benches,
    bench_bfs_crawl_100_nodes,
    bench_markdown_conversion,
    bench_extract_links_50kb,
);
criterion_main!(benches);
