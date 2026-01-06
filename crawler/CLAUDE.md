# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a high-performance web crawler written in Rust that:
- Crawls websites starting from seed domains using the `spider` library
- Extracts readable content using a custom readability algorithm
- Indexes content using Tantivy (full-text search engine)
- Implements concurrent crawling with configurable domain-level concurrency

## Build, Test, and Run Commands

### Building
```bash
# Development build
cargo build

# Release build (optimized)
cargo build --release

# The binary will be at target/release/crawler
```

### Testing
```bash
# Run all tests
cargo test

# Run tests for a specific module
cargo test lol_readability

# Run tests with output visible
cargo test -- --nocapture

# Run a specific test
cargo test test_finds_article_by_id
```

### Running
```bash
# Run with a domains file (one domain per line)
cargo run --release -- input.txt

# With options
cargo run --release -- input.txt \
  --depth 5 \
  --concurrency 500 \
  --exclude "https://example.com/admin" \
  --verbose

# View help
cargo run --release -- --help

# Test with real input (IndieWiki domains, excluding http:// URLs)
cargo run --release -- ../get_domains/getindiewiki.txt -x "http://"
```

### Cleanup
```bash
# Clean build artifacts
cargo clean

# Remove search index database
rm -rf search_db/
```

## Architecture

### Core Components

**main.rs** - Orchestrates the crawl:
- `CrawlDb`: Manages the Tantivy search index with fields for URL, domain, title, and body
- `CrawlMetrics`: Atomic counters for pages crawled/failed (lock-free status updates)
- `crawl_domain()`: Crawls a single domain using the spider library
- Main loop uses `futures::stream::for_each_concurrent` for domain-level concurrency

**lol_readability.rs** - Content extraction:
- Single-pass HTML parser using `lol_html` streaming rewriter
- Implements a readability algorithm inspired by Mozilla Readability
- Scores elements based on tag type, id/class attributes, text length, and link density
- Returns: `(readable_text, page_title)`
- Key function: `find_main_content(html: &[u8]) -> Result<(String, String)>`

### Crawl Flow

1. Parse seed domains from input file
2. Initialize Tantivy index (reuses existing `search_db/` if present)
3. Create shared `CrawlDb` and `CrawlMetrics` (atomic counters)
4. Start background task for periodic status updates and index commits
5. Crawl domains concurrently using `for_each_concurrent`:
   - Each domain uses spider library with depth limit and robots.txt respect
   - Subscribe to page events, extract content, index to Tantivy
6. Final commit when all domains complete

### Concurrency Model

- **Domain-level concurrency**: Configurable max concurrent domains (default 50)
- **Per-domain rate limiting**: Spider configured with `with_limit(1)` - one request at a time per domain
- **Lock-free metrics**: Atomic counters avoid contention during status updates
- **Dedicated DB thread**: `CrawlDb` runs in its own thread, communicates via mpsc channel

### Tantivy Schema

- `url`: TEXT | STORED | FAST (for storage and retrieval)
- `domain`: STRING | FAST (for efficient domain-based queries)
- `title`: TEXT | STORED
- `body`: TEXT with custom "norm_tokenizer" (lowercasing + stemming)

### Readability Algorithm

The `lol_readability.rs` module uses a single-pass streaming parser with a scoring system:

**Base scores by tag type:**
- `<article>`, `<main>`, `<section>`: +30 (semantic content)
- `<p>`: +10
- `<div>`: +5
- `<h1>`-`<h6>`: -5
- `<ul>`, `<ol>`, `<dl>`: -3

**Attribute-based scoring:**
- Positive classes/ids (e.g., "article", "content", "main"): +25
- Negative classes/ids (e.g., "sidebar", "nav", "footer"): -25
- Unlikely patterns (e.g., "ad-break", "sponsor"): -50

**Content quality signals:**
- Text length contributes `sqrt(text_len)` to score
- Link density penalty: if >50% of text is links, score *= (1 - density)
- Minimum 100 characters required
- Average word length must be >2 (filters garbage like "a a a b b b")

The element with the highest final score becomes the main content.

## Key Implementation Details

### Spider Configuration
- `respect_robots_txt = true`: Polite crawling
- `subdomains = false`: Stay within the exact domain
- `depth`: Configurable crawl depth (default 5)
- `with_limit(1)`: One concurrent request per domain

### IMPORTANT: Stream pages, don't buffer them
**DO NOT use `website.scrape()` + `website.get_pages()`** - this buffers all pages in memory and will cause OOM on large crawls.

Instead, use `website.subscribe()` to get a channel, spawn the crawl in a separate tokio task (so the Website is dropped when done, closing the channel), and process pages as they stream in:
```rust
let mut rx = website.subscribe(16).unwrap();
let crawl_task = tokio::spawn(async move {
    website.crawl().await;
});
while let Ok(page) = rx.recv().await {
    // process page
}
let _ = crawl_task.await;
```

### Unicode Normalization
Page content is normalized with NFKC before indexing to handle equivalent Unicode representations.

### Tokio Runtime
Configured with 20 worker threads (`#[tokio::main(flavor = "multi_thread", worker_threads = 20)]`)

## Common Patterns

### Adding a new CLI argument
Edit the `Args` struct in main.rs with the `#[arg(...)]` attribute from clap.

### Modifying the readability algorithm
The scoring logic is in `ElementFrame::new()` and `ElementFrame::calculate_base_score()` in `lol_readability.rs`.
Key tunables: minimum text length (line ~197), base scores (lines ~100-107), regex patterns (lines ~19-40).

### Running tests with specific output
The `lol_readability` module has extensive tests demonstrating various HTML patterns.
Run with `cargo test lol_readability -- --nocapture` to see extracted content.
