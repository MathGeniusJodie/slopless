use anyhow::{Context, Result};
use clap::Parser;

use reqwest::Client;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::fs::read_to_string;
use std::sync::Arc;
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED, TEXT,
};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index};
use tokio::task::JoinSet;
use url::Url;
use fastbloom::BloomFilter;

mod lol_readability;
use lol_readability::find_main_content;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(help = "Path to text file containing domains (one per line)")]
    input_file: String,
    #[arg(short = 'x', long = "exclude")]
    excluded_url_prefixes: Vec<String>,
    #[arg(short = 'd', long = "depth", default_value_t = 5)]
    max_crawl_depth: usize,
    #[arg(short = 'c', long = "concurrency", default_value_t = 500)]
    max_concurrent_requests: usize,
    // verbosity flag could be added here
    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose_logging: bool,
}

/// A page that has been fetched and is ready to be indexed
struct IndexedPage {
    page_url: String,
    page_title: String,
    page_content: String,
}

/// A single URL to crawl, with associated metadata
#[derive(Clone, Eq, PartialEq)]
struct CrawlTask {
    crawl_depth: usize,
    target_url: Url,
    domain: Arc<str>, // Cached domain string for efficient grouping
}

impl Ord for CrawlTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Process deeper pages first (they're less likely to spawn more work)
        // This helps drain the queue faster and reduces memory usage
        other
            .crawl_depth
            .cmp(&self.crawl_depth)
            .then_with(|| self.target_url.as_str().cmp(other.target_url.as_str()))
    }
}

impl PartialOrd for CrawlTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl CrawlTask {
    fn new(target_url: Url, crawl_depth: usize) -> Option<Self> {
        let host = match target_url.host_str() {
            Some(h) => h,
            None => return None,
        };
        Some(Self {
            domain: Arc::from(host),
            target_url,
            crawl_depth,
        })
    }
    /// Check if this task should be skipped based on depth, exclusions, and deduplication
    fn should_skip(
        &self,
        max_depth: usize,
        excluded_prefixes: &[String],
        db: &mut CrawlDb,
        pending_task_urls: &HashSet<Arc<str>, ahash::RandomState>,
    ) -> bool {
        // Too deep?
        if self.crawl_depth > max_depth {
            return true;
        }

        // Explicitly excluded?
        if excluded_prefixes
            .iter()
            .any(|prefix| self.target_url.as_str().starts_with(prefix))
        {
            return true;
        }

        // Already crawled or queued?
        db.is_url_already_processed(self.target_url.as_str(), pending_task_urls)
    }
}

/// Database wrapping Tantivy index with URL deduplication via Bloom filter
struct CrawlDb {
    index: Index,
    index_writer: tantivy::IndexWriter,
    url_field: tantivy::schema::Field,
    title_field: tantivy::schema::Field,
    body_field: tantivy::schema::Field,

    // Three-layer deduplication system:
    // 1. Bloom filter (fast, probabilistic)
    seen_urls_bloom: BloomFilter<ahash::RandomState>,
    // 2. Uncommitted URLs (not yet in Tantivy index)
    uncommitted_urls: HashSet<Box<str>, ahash::RandomState>,
    // 3. Tantivy index query (fallback for Bloom collisions)

    // Metrics for Bloom filter effectiveness
    bloom_saved_lookups: usize,
    bloom_possible_collisions: usize,
}

impl CrawlDb {
    fn new(expected_url_count: usize) -> Result<Self> {
        // Set up search tokenizer with lowercasing and stemming
        let search_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .filter(tantivy::tokenizer::Stemmer::new(
                tantivy::tokenizer::Language::English,
            ))
            .build();

        // Build schema
        let body_field_indexing = TextFieldIndexing::default()
            .set_tokenizer("norm_tokenizer")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let body_field_options = TextOptions::default().set_indexing_options(body_field_indexing);
        let mut schema_builder = Schema::builder();
        let url_field = schema_builder.add_text_field("url", TEXT | STORED | FAST);
        let title_field = schema_builder.add_text_field("title", TEXT | STORED);
        let body_field = schema_builder.add_text_field("body", body_field_options);
        let schema = schema_builder.build();

        // Open or create index
        let index = if std::path::Path::new("search_db").exists() {
            Index::open_in_dir("search_db")?
        } else {
            std::fs::create_dir_all("search_db")?;
            Index::create(
                tantivy::directory::MmapDirectory::open("search_db")?,
                schema,
                tantivy::IndexSettings::default(),
            )?
        };
        index
            .tokenizers()
            .register("norm_tokenizer", search_tokenizer);

        let index_writer = index.writer(50_000_000)?;

        // Initialize Bloom filter: 8 bits per URL gives ~0.1% false positive rate
        let seen_urls_bloom = BloomFilter::with_num_bits(expected_url_count * 8)
            .hasher(ahash::RandomState::new())
            .expected_items(expected_url_count);

        Ok(Self {
            index,
            index_writer,
            url_field,
            title_field,
            body_field,
            seen_urls_bloom,
            uncommitted_urls: HashSet::with_hasher(ahash::RandomState::new()),
            bloom_saved_lookups: 0,
            bloom_possible_collisions: 0,
        })
    }

    /// Check if URL has already been processed (three-layer check)
    fn is_url_already_processed(
        &mut self,
        url: &str,
        pending_task_urls: &HashSet<Arc<str>, ahash::RandomState>,
    ) -> bool {
        // Layer 1: Check if URL is already in the task queue (not yet crawled)
        if pending_task_urls.contains(url) {
            return true;
        }

        // Layer 2: Check if URL was recently indexed but not yet committed
        if self.uncommitted_urls.contains(url) {
            return true;
        }

        // Layer 3: Check Bloom filter (probabilistic, may have false positives)
        if !self.seen_urls_bloom.contains(url) {
            // Bloom filter says we haven't seen it - trust this (no false negatives)
            self.bloom_saved_lookups += 1;
            return false;
        }

        // Bloom filter says we might have seen it
        self.bloom_possible_collisions += 1;

        // Layer 4: Query Tantivy index to confirm (handles Bloom false positives)
        self.is_url_in_index(url)
    }

    /// Query Tantivy index to check if URL exists (expensive, used only for Bloom collisions)
    fn is_url_in_index(&self, url: &str) -> bool {
        use tantivy::collector::DocSetCollector;
        use tantivy::query::TermQuery;

        let term = tantivy::Term::from_field_text(self.url_field, url);
        let query = TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);

        let Ok(searcher) = self.index.reader().map(|r| r.searcher()) else {
            return false;
        };

        let Ok(doc_addresses) = searcher.search(&query, &DocSetCollector) else {
            return false;
        };

        !doc_addresses.is_empty()
    }

    fn index_page(&mut self, page: IndexedPage) -> Result<()> {
        let doc = doc!(
            self.url_field => &*page.page_url,
            self.title_field => &*page.page_title,
            self.body_field => &*page.page_content
        );
        self.index_writer.add_document(doc)?;
        self.uncommitted_urls.insert(page.page_url.into());
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.index_writer.commit()?;
        self.uncommitted_urls.clear();
        Ok(())
    }

}

/// Fetch a URL and extract content + links
async fn fetch_and_process_page(
    http_client: &Client,
    task: &CrawlTask,
) -> Result<(Vec<CrawlTask>, IndexedPage)> {
    let CrawlTask {
        target_url,
        crawl_depth,
        ..
    } = task;

    // Fetch the page
    let response = http_client.get(target_url.clone()).send().await?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to fetch URL: {} with status: {}",
            target_url,
            response.status()
        ));
    }

    // Check content type (skip non-HTML)
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|header_value| header_value.to_str().ok())
        .unwrap_or("");

    if !content_type.starts_with("text/html") {
        return Err(anyhow::anyhow!(
            "Non-HTML content type: {} for URL: {}",
            content_type,
            target_url
        ));
    }

    // Extract content and links
    let html_body = response.text().await?;
    let (readable_text, extracted_links, page_title) = find_main_content(html_body.as_bytes())?;
    use unicode_normalization::UnicodeNormalization;
    let page_content = readable_text.nfkc().collect::<String>();

    // Convert extracted links to crawl tasks (same-domain only, one level deeper)
    let child_tasks = extracted_links
        .into_iter()
        .filter_map(|link| {
            let mut url = target_url.join(&link).ok()?;
            url.set_fragment(None); // Remove #anchors

            // Only crawl links on the same domain
            (url.host_str() == target_url.host_str())
                .then_some(CrawlTask::new(url, crawl_depth + 1)?)
        })
        .collect();

    Ok((
        child_tasks,
        IndexedPage {
            page_url: target_url.to_string(),
            page_content,
            page_title,
        },
    ))
}

/// Load domain list from file and convert to seed URLs
fn load_seed_urls(file_path: &str) -> Result<Vec<Url>> {
    let file_content = read_to_string(file_path)
        .with_context(|| format!("Failed to read input file: {}", file_path))?;

    let mut seed_urls = Vec::new();

    for line in file_content.lines() {
        let domain = line.trim();
        if domain.is_empty() {
            continue;
        }

        // Clean domain (remove http:// or https:// prefix if present)
        let clean_domain = domain
            .trim_start_matches("http://")
            .trim_start_matches("https://");

        // Parse as HTTPS URL
        let parsed_url =
            match Url::parse(&format!("https://{clean_domain}")).context("Invalid domain") {
                Ok(url) => url,
                Err(parse_error) => {
                    eprintln!("Skipping invalid domain '{}': {:?}", domain, parse_error);
                    continue;
                }
            };

        if let Some(_host) = parsed_url.host_str() {
            seed_urls.push(parsed_url);
        }
    }

    Ok(seed_urls)
}

/// Print crawl status and commit index
fn report_status_and_commit(
    pages_crawled: usize,
    pages_failed: usize,
    task_queue_size: usize,
    active_domains_count: usize,
    db: &mut CrawlDb,
) -> Result<()> {
    println!(
        "Crawled: {pages_crawled}, Failed: {pages_failed}, Queue: {task_queue_size}, Active: {active_domains_count}"
    );

    let bloom_total = db.bloom_saved_lookups + db.bloom_possible_collisions;
    let bloom_efficiency = if bloom_total > 0 {
        (db.bloom_saved_lookups as f64 / bloom_total as f64) * 100.0
    } else {
        0.0
    };

    println!(
        "  Uncommitted: {} URLs, Bloom filter efficiency: {:.2}% saved lookups",
        db.uncommitted_urls.len(),
        bloom_efficiency
    );

    db.commit()?;
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {
    let cli_args = Args::parse();

    // Load seed URLs from input file
    let seed_urls = load_seed_urls(&cli_args.input_file)?;

    // Configure HTTP client with Chrome user agent and performance optimizations
    let http_client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
        .pool_idle_timeout(None) // Keep connections alive indefinitely
        .tcp_nodelay(true)        // Send small packets immediately (lower latency)
        .build()?;

    let expected_url_count = 100_000_000;
    let mut db = CrawlDb::new(expected_url_count)?;

    // Initialize crawl state
    let (mut pages_crawled, mut pages_failed) = (0usize, 0usize);

    // Track which domains currently have active requests (one request per domain at a time)
    let mut active_request_domains: HashSet<Arc<str>, ahash::RandomState> =
        HashSet::with_hasher(ahash::RandomState::new());

    // Track URLs that are in the task queue but not yet crawled
    let mut pending_task_urls: HashSet<Arc<str>, ahash::RandomState> =
        HashSet::with_hasher(ahash::RandomState::new());

    // Priority queue: deeper tasks first (BTreeSet with custom Ord)
    let mut task_queue: BTreeSet<CrawlTask> = seed_urls
        .into_iter()
        .filter_map(|url| {
            db.seen_urls_bloom.insert(url.as_str());
            pending_task_urls.insert(url.as_str().into());
            CrawlTask::new(url, 0)
        })
        .collect();

    // Pool of async worker tasks
    let mut worker_pool = JoinSet::new();
    let mut last_status_report = std::time::Instant::now();

    loop {
        // Spawn new tasks up to concurrency limit
        let available_worker_slots = cli_args
            .max_concurrent_requests
            .saturating_sub(worker_pool.len());

        // Extract tasks from queue, ensuring only one request per domain at a time
        for task in task_queue
            .extract_if(.., |task| {
                // Try to mark this domain as in-progress
                // If it's already in progress, leave task in queue
                active_request_domains.insert(task.domain.clone())
            })
            .take(available_worker_slots)
        {
            // Remove from pending set since we're about to crawl it
            pending_task_urls.remove(task.target_url.as_str());

            let http_client = http_client.clone();
            worker_pool.spawn(async move {
                let result = fetch_and_process_page(&http_client, &task).await;
                (result, task)
            });
        }

        // Exit when all work is done
        if worker_pool.is_empty() && task_queue.is_empty() {
            break;
        }

        // Wait for next task to complete
        let Some(Ok(crawl_result)) = worker_pool.join_next().await else {
            continue;
        };

        // Process completed task
        let (fetch_result, completed_task) = crawl_result;

        // Mark domain as no longer in progress
        active_request_domains.remove(&completed_task.domain);

        // Handle fetch result
        let (discovered_child_tasks, page_data) = match fetch_result {
            Ok(result) => result,
            Err(e) => {
                if cli_args.verbose_logging {
                    eprintln!("Error fetching {}: {:?}", completed_task.target_url, e);
                }
                pages_failed += 1;
                continue;
            }
        };

        // Add discovered links to queue (after filtering)
        for child_task in discovered_child_tasks {
            if !child_task.should_skip(
                cli_args.max_crawl_depth,
                &cli_args.excluded_url_prefixes,
                &mut db,
                &pending_task_urls,
            ) {
                let url_str = child_task.target_url.as_str();
                db.seen_urls_bloom.insert(url_str);
                pending_task_urls.insert(url_str.into());
                task_queue.insert(child_task);
            }
        }

        // Index the page
        let _ = db.index_page(page_data);
        pages_crawled += 1;

        // Periodic status reporting and commit
        if last_status_report.elapsed().as_secs() >= 5 {
            report_status_and_commit(
                pages_crawled,
                pages_failed,
                task_queue.len(),
                active_request_domains.len(),
                &mut db,
            )
            .expect("Failed to commit index");
            last_status_report = std::time::Instant::now();
        }
    }
    println!(
        "Crawl finished. Docs in index: {}",
        db.index.reader()?.searcher().num_docs()
    );

    Ok(())
}
