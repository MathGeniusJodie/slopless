# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a high-performance web crawler written in Rust that:
- Crawls websites starting from seed domains
- Extracts readable content using a custom readability algorithm
- Indexes content using Tantivy (full-text search engine)
- Uses Bloom filters for efficient URL deduplication
- Implements concurrent crawling with configurable limits

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
cargo test bloom
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
- `CrawlTask`: Represents a URL to crawl with its depth and domain
- `CrawlDb`: Manages the Tantivy search index and Bloom filter for URL deduplication
- `SearchIndex`: Wraps Tantivy index with URL, title, and body fields
- Main crawl loop uses `JoinSet` for concurrent task management

**lol_readability.rs** - Content extraction:
- Single-pass HTML parser using `lol_html` streaming rewriter
- Implements a readability algorithm inspired by Mozilla Readability
- Scores elements based on tag type, id/class attributes, text length, and link density
- Returns: (readable_text, extracted_urls, page_title)
- Key function: `find_main_content(html: &[u8])`

**bloom.rs** - URL deduplication:
- Custom Bloom filter implementation with double hashing
- Uses `ahash::RandomState` for fast hashing
- `BloomFilter::insert()` returns true if URL may have been seen before
- Reduces expensive Tantivy lookups by ~99% for unseen URLs

### Crawl Flow

1. Parse seed domains from input file
2. Initialize Tantivy index (reuses existing `search_db/` if present)
3. Create initial `CrawlTask` queue with depth 0
4. Main loop:
   - Spawn tasks up to `max_concurrent_requests` limit
   - Domain-level concurrency control: only one request per domain at a time (prevents hammering)
   - When task completes:
     - Extract links and content via `find_main_content()`
     - Filter discovered URLs (same-domain only, depth limit, Bloom filter check)
     - Index page content in Tantivy
   - Commit index every 5 seconds
5. Exit when queue is empty and all workers are done

### Task Queue Design

- Uses `BTreeSet<CrawlTask>` sorted by depth (DESCENDING) to process deepest pages first
- `domains_in_progress` HashSet ensures only one concurrent request per domain
- `queued_urls` HashSet prevents duplicate tasks before they're added to Bloom filter
- Tasks are removed from queue only when spawned (allows domain-lock to release)

### Deduplication Strategy

Three-layer approach to prevent duplicate crawls:
1. **queued_urls** HashSet: Fast check for URLs already in the task queue
2. **Bloom filter**: Probabilistic check with ~0.1% false positive rate
3. **Tantivy query**: Fallback for Bloom filter collisions (queries the url field)

The Bloom filter dramatically reduces expensive Tantivy lookups.

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

### HTTP Client Configuration
- User-Agent set to Chrome (some sites block default reqwest UA)
- `pool_idle_timeout(None)`: Keep connections alive for performance
- `tcp_nodelay(true)`: Reduce latency for small requests

### Tantivy Schema
- `url`: TEXT | STORED | FAST (for exact lookups and sorting)
- `title`: TEXT | STORED
- `body`: TEXT with custom "norm_tokenizer" (lowercasing + stemming)

### Unicode Normalization
Page content is normalized with NFKC before indexing to handle equivalent Unicode representations.

### Tokio Runtime
Configured with 20 worker threads (`#[tokio::main(flavor = "multi_thread", worker_threads = 20)]`)

## Common Patterns

### Adding a new CLI argument
Edit the `Args` struct in main.rs with the `#[arg(...)]` attribute from clap.

### Modifying the readability algorithm
The scoring logic is in `Frame::new()` and the element closing handler in `lol_readability.rs`.
Key tunables: minimum text length (line 321), base scores (lines 66-73), regex patterns (lines 10-25).

### Changing Bloom filter sizing
`expected_url_count` in main.rs:344 controls the Bloom filter size.
Current: 100M URLs with 8 bits per URL (96 MB memory).

### Running tests with specific output
The `lol_readability` module has extensive tests demonstrating various HTML patterns.
Run with `cargo test lol_readability -- --nocapture` to see extracted content.
