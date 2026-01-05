use anyhow::{Context, Result};
use clap::Parser;

use fastbloom::BloomFilter;
use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::fs::read_to_string;
use std::sync::Arc;
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED, TEXT,
};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index, TantivyDocument};
use tokio::task::JoinSet;
use unicode_normalization::UnicodeNormalization;
use url::Url;

mod lol_readability;
mod robots;
use lol_readability::find_main_content;
use robots::{is_robots_url, is_sitemap_url, parse_robots, parse_sitemap, RobotsCache};

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
    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose_logging: bool,
}

/// A page that has been fetched and is ready to be indexed
struct IndexedPage {
    page_title: String,
    page_content: String,
}

/// A single URL to crawl, with associated metadata
#[derive(Clone)]
struct CrawlTask {
    crawl_depth: usize,
    target_url: Url,
}

impl CrawlTask {
    fn new(target_url: Url, crawl_depth: usize) -> Self {
        Self {
            target_url,
            crawl_depth,
        }
    }
    /// Check if this task should be skipped based on depth, exclusions, robots.txt, and deduplication.
    /// Note: Updates bloom filter statistics as a side effect.
    fn should_skip(
        &self,
        max_depth: usize,
        excluded_prefixes: &[String],
        db: &mut CrawlDb,
        crawl_buckets: &CrawlTaskBuckets,
        robots_cache: &RobotsCache,
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

        // Blocked by robots.txt?
        let domain = match self.target_url.host_str() {
            Some(d) => d,
            None => return true,
        };
        if !robots_cache.is_url_allowed(&self.target_url, domain) {
            return true;
        }

        // Already crawled or queued?
        db.is_url_already_processed(&self.target_url, crawl_buckets)
    }
}

/// Manages Tantivy search index and URL deduplication via Bloom filter
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
    uncommitted_urls: HashSet<Url, ahash::RandomState>,
    // 3. Tantivy index query (fallback for Bloom collisions)

    // Metrics for Bloom filter effectiveness
    bloom_saved_lookups: usize,
    bloom_positive_checks: usize,
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

        let mut db = Self {
            index,
            index_writer,
            url_field,
            title_field,
            body_field,
            seen_urls_bloom,
            uncommitted_urls: HashSet::with_hasher(ahash::RandomState::new()),
            bloom_saved_lookups: 0,
            bloom_positive_checks: 0,
        };

        let reader = db.index.reader()?;
        let searcher = reader.searcher();

        println!("Loading existing indexed URLs into Bloom filter...");
        for segment_reader in searcher.segment_readers() {
            let max_doc = segment_reader.max_doc();

            println!("  Segment with {} docs", segment_reader.num_docs());
            let store_reader = segment_reader.get_store_reader(1_000)?;
            for doc_id in 0..max_doc {
                // Skip deleted docs
                if segment_reader.is_deleted(doc_id) {
                    continue;
                }

                let doc: TantivyDocument = store_reader.get(doc_id)?;
                if let Some(url) = doc.get_first(url_field).and_then(|v| v.as_str()) {
                    db.seen_urls_bloom.insert(url);
                }
            }
        }
        println!(
            "Bloom filter bits set: {}",
            db.seen_urls_bloom
                .as_slice()
                .iter()
                .map(|b| b.count_ones() as usize)
                .sum::<usize>()
        );

        Ok(db)
    }

    /// Check if URL has already been processed (three-layer check)
    fn is_url_already_processed(&mut self, url: &Url, crawl_buckets: &CrawlTaskBuckets) -> bool {
        // Layer 1: Check if URL is already in the task queue (not yet crawled)
        if crawl_buckets.contains(url) {
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
        self.bloom_positive_checks += 1;

        // Layer 4: Query Tantivy index to confirm (handles Bloom false positives)
        self.is_url_in_index(url.as_str())
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

    fn index_page(&mut self, page: IndexedPage, url: &Url) -> Result<()> {
        let doc = doc!(
            self.url_field => url.as_str(),
            self.title_field => &*page.page_title,
            self.body_field => &*page.page_content
        );
        self.index_writer.add_document(doc)?;
        self.uncommitted_urls.insert(url.clone());
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.index_writer.commit()?;
        self.uncommitted_urls.clear();
        Ok(())
    }
}

/// Result of fetching a URL (without child tasks, which are returned separately)
enum FetchResult {
    /// HTML page to index
    Page(IndexedPage),
    /// Sitemap (no content to index)
    Sitemap,
    /// robots.txt with parsed rules
    Robots(Option<texting_robots::Robot>),
}

/// Fetch a URL and return (child_tasks, result)
async fn fetch_and_process_url(
    http_client: &Client,
    task: &CrawlTask,
) -> Result<(Vec<CrawlTask>, FetchResult)> {
    let CrawlTask {
        target_url,
        crawl_depth,
    } = task;

    let domain = match target_url.host_str() {
        Some(d) => d,
        None => return Err(anyhow::anyhow!("URL has no host: {}", target_url)),
    };

    let response = http_client.get(target_url.clone()).send().await?;

    // Handle robots.txt (even if 404, we return empty rules + homepage)
    if is_robots_url(target_url) {
        let content = if response.status().is_success() {
            response.text().await.unwrap_or_default()
        } else {
            String::new()
        };
        let (robot, sitemap_urls) = parse_robots(&content);

        let mut child_tasks = Vec::new();

        // Add sitemap URLs
        for url_str in sitemap_urls {
            let Ok(url) = Url::parse(&url_str) else {
                continue;
            };
            if url.host_str() != target_url.host_str() {
                continue;
            }
            let t = CrawlTask::new(url, 0);
            child_tasks.push(t);
        }

        // Always add homepage
        if let Ok(homepage) = Url::parse(&format!("https://{}/", domain)) {
            let t = CrawlTask::new(homepage, 0);
            child_tasks.push(t);
        }

        return Ok((child_tasks, FetchResult::Robots(robot)));
    }

    if !response.status().is_success() {
        return Err(anyhow::anyhow!("HTTP {}", response.status()));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Handle sitemaps (only trust URL patterns, not content-type which is too broad)
    if is_sitemap_url(target_url) {
        let bytes = response.bytes().await?;
        let (page_urls, sitemap_urls) = parse_sitemap(&bytes);

        let mut child_tasks = Vec::with_capacity(page_urls.len() + sitemap_urls.len());

        // Add page URLs at current depth
        for url_str in page_urls {
            let Ok(url) = Url::parse(&url_str) else {
                continue;
            };
            if url.host_str() != target_url.host_str() {
                continue;
            }
            child_tasks.push(CrawlTask::new(url, *crawl_depth));
        }

        // Add nested sitemaps at depth + 1
        for url_str in sitemap_urls {
            let Ok(url) = Url::parse(&url_str) else {
                continue;
            };
            if url.host_str() != target_url.host_str() {
                continue;
            }
            child_tasks.push(CrawlTask::new(url, crawl_depth + 1));
        }

        return Ok((child_tasks, FetchResult::Sitemap));
    }

    // Handle HTML pages
    if !content_type.starts_with("text/html") {
        return Err(anyhow::anyhow!("Non-HTML content type: {}", content_type));
    }

    let html_body = response.text().await?;
    let (readable_text, extracted_links, page_title) = find_main_content(html_body.as_bytes())?;
    let page_content = readable_text.nfkc().collect::<String>();

    let child_tasks = extracted_links
        .into_iter()
        .filter_map(|link| {
            let mut url = target_url.join(&link).ok()?;
            url.set_fragment(None);
            (url.host_str() == target_url.host_str())
                .then_some(CrawlTask::new(url, crawl_depth + 1))
        })
        .collect();

    let page = IndexedPage {
        page_content,
        page_title,
    };

    Ok((child_tasks, FetchResult::Page(page)))
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

        if parsed_url.host_str().is_some() {
            seed_urls.push(parsed_url);
        }
    }

    Ok(seed_urls)
}

/// Print crawl progress statistics
fn print_crawl_status(
    pages_crawled: usize,
    sitemaps_processed: usize,
    pages_failed: usize,
    task_queue_size: usize,
    active_domains_count: usize,
    db: &CrawlDb,
) {
    println!(
        "Pages: {pages_crawled}, Sitemaps: {sitemaps_processed}, Failed: {pages_failed}, Queue: {task_queue_size}, Active: {active_domains_count}"
    );

    let bloom_total = db.bloom_saved_lookups + db.bloom_positive_checks;
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
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {
    let cli_args = Args::parse();

    // Configure HTTP client with Chrome user agent and performance optimizations
    let http_client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
        .pool_idle_timeout(None) // Keep connections alive indefinitely
        .tcp_nodelay(true)        // Send small packets immediately (lower latency)
        .build()?;

    let expected_url_count = 100_000_000;
    let mut db = CrawlDb::new(expected_url_count)?;

    let seed_urls = load_seed_urls(&cli_args.input_file)?;

    // Initialize crawl state
    let (mut pages_crawled, mut sitemaps_processed, mut pages_failed) = (0usize, 0usize, 0usize);

    // Track which domains currently have active requests (one request per domain at a time)
    let mut active_request_domains: HashSet<Arc<str>, ahash::RandomState> =
        HashSet::with_hasher(ahash::RandomState::new());

    // Robots.txt cache and tracking for initialized domains
    let mut robots_cache = RobotsCache::new();
    let mut initialized_domains: HashSet<Arc<str>, ahash::RandomState> =
        HashSet::with_hasher(ahash::RandomState::new());

    // New bucketed task manager
    let mut crawl_buckets = CrawlTaskBuckets::new(cli_args.max_crawl_depth + 1);
    for url in seed_urls {
        if db.is_url_already_processed(&url, &crawl_buckets) {
            continue;
        }
        db.seen_urls_bloom.insert(url.as_str());
        crawl_buckets.insert(url, 0);
    }

    // Pool of async worker tasks
    let mut worker_pool = JoinSet::new();
    let mut last_status_report = std::time::Instant::now();

    loop {
        // Spawn new tasks up to concurrency limit
        let available_worker_slots = cli_args
            .max_concurrent_requests
            .saturating_sub(worker_pool.len());

        // Extract tasks from buckets, handling collisions first
        let tasks_to_spawn: Vec<_> = crawl_buckets.extract_if(
            |url, _depth| {
                if let Some(domain) = url.host_str() {
                    active_request_domains.insert(Arc::from(domain))
                } else {
                    false
                }
            },
            available_worker_slots,
        );

        for task in tasks_to_spawn {
            // Initialize domain: add robots.txt as a task (will be fetched first due to domain lock)
            let domain = match task.target_url.host_str() {
                Some(d) => d,
                None => continue,
            };
            if !initialized_domains.contains(domain) {
                initialized_domains.insert(Arc::from(domain));

                // Add robots.txt task for this domain
                let robots_url = Url::parse(&format!("https://{}/robots.txt", domain)).unwrap();
                let robots_task = CrawlTask::new(robots_url, 0);
                db.seen_urls_bloom.insert(robots_task.target_url.as_str());
                crawl_buckets.insert(robots_task.target_url.clone(), 0);
            }

            let http_client = http_client.clone();
            worker_pool.spawn(async move {
                let result = fetch_and_process_url(&http_client, &task).await;
                (result, task)
            });
        }

        // Exit when all work is done
        if worker_pool.is_empty() && crawl_buckets.is_empty() {
            break;
        }

        // Wait for next task to complete
        let Some(Ok(crawl_result)) = worker_pool.join_next().await else {
            continue;
        };

        // Process completed task
        let (fetch_result, completed_task) = crawl_result;

        // Mark domain as no longer in progress
        let Some(completed_domain) = completed_task.target_url.host_str() else {
            continue;
        };
        active_request_domains.remove(completed_domain);

        // Handle fetch result
        let (child_tasks, result) = match fetch_result {
            Ok(r) => r,
            Err(e) => {
                if cli_args.verbose_logging {
                    eprintln!("Error fetching {}: {:?}", completed_task.target_url, e);
                }
                pages_failed += 1;
                continue;
            }
        };

        // Add child tasks to buckets (unified for all result types)
        for child_task in child_tasks {
            if !child_task.should_skip(
                cli_args.max_crawl_depth,
                &cli_args.excluded_url_prefixes,
                &mut db,
                &crawl_buckets,
                &robots_cache,
            ) {
                db.seen_urls_bloom.insert(child_task.target_url.as_str());
                crawl_buckets.insert(child_task.target_url, child_task.crawl_depth);
            }
        }

        // Handle result-specific logic
        match result {
            FetchResult::Robots(robot) => {
                if let Some(r) = robot {
                    let domain = match completed_task.target_url.host_str() {
                        Some(d) => d,
                        None => continue,
                    };
                    robots_cache.insert(Arc::from(domain), r);
                }
            }
            FetchResult::Sitemap => {
                sitemaps_processed += 1;
            }
            FetchResult::Page(page) => match db.index_page(page, &completed_task.target_url) {
                Ok(()) => pages_crawled += 1,
                Err(e) => {
                    if cli_args.verbose_logging {
                        eprintln!("Error indexing {}: {:?}", &completed_task.target_url, e);
                    }
                    pages_failed += 1;
                }
            },
        }

        // Periodic status reporting and commit
        if last_status_report.elapsed().as_secs() >= 5 {
            print_crawl_status(
                pages_crawled,
                sitemaps_processed,
                pages_failed,
                crawl_buckets.len(),
                active_request_domains.len(),
                &db,
            );
            if let Err(e) = db.commit() {
                eprintln!("Warning: Failed to commit index: {:?}", e);
                // Continue crawling - uncommitted data will be retried on next commit
            } else if let Ok(reader) = db.index.reader() {
                println!(
                    "  Total indexed documents: {}",
                    reader.searcher().num_docs()
                );
            }
            last_status_report = std::time::Instant::now();
        }
    }
    println!(
        "Crawl finished. Docs in index: {}",
        db.index.reader()?.searcher().num_docs()
    );

    Ok(())
}

/// Bucketed crawl task manager with collision handling
struct CrawlTaskBuckets {
    buckets: Vec<Vec<usize>>, // buckets by depth, each holds hash keys
    url_map: HashMap<usize, Url, ahash::RandomState>, // map from hash key to Url
    collisions: Vec<CrawlTask>, // Tasks with hash collisions in url_map
}

impl CrawlTaskBuckets {
    fn new(max_depth: usize) -> Self {
        Self {
            buckets: vec![Vec::new(); max_depth],
            url_map: HashMap::with_hasher(ahash::RandomState::new()),
            collisions: Vec::new(),
        }
    }

    /// Check if a URL string is present in any bucket or collision
    pub fn contains(&self, url: &Url) -> bool {
        // Check url_map using hash key for O(1) lookup
        let key = Self::hash_url(url);
        if let Some(u) = self.url_map.get(&key) {
            if u == url {
                return true;
            }
        }
        // Check collisions
        if self.collisions.iter().any(|t| &t.target_url == url) {
            return true;
        }
        false
    }

    /// Insert a Url into a bucket by depth
    fn insert(&mut self, url: Url, depth: usize) {
        if depth >= self.buckets.len() {
            return;
        }
        let key = Self::hash_url(&url);
        if let Some(existing) = self.url_map.get(&key) {
            if existing != &url {
                // Hash collision with different URL: store in collisions list
                self.collisions.push(CrawlTask::new(url, depth));
            }
            // Same URL already exists - skip (no duplicates)
            return;
        }
        self.url_map.insert(key, url);
        self.buckets[depth].push(key);
    }

    /// Extract up to n tasks, handling collisions first, then buckets in order
    fn extract_if<F>(&mut self, mut filter: F, max_count: usize) -> Vec<CrawlTask>
    where
        F: FnMut(&Url, usize) -> bool,
    {
        let mut result = Vec::new();
        // Handle collisions first
        let mut i = 0;
        while result.len() < max_count && i < self.collisions.len() {
            let CrawlTask {
                target_url: url,
                crawl_depth: depth,
            } = &self.collisions[i];
            if !filter(url, *depth) {
                i += 1;
                continue;
            }
            let removed = self.collisions.swap_remove(i);
            result.push(removed);
        }
        // Then buckets from deepest to shallowest (deeper pages spawn less work)
        for depth in (0..self.buckets.len()).rev() {
            let mut j = 0;
            while j < self.buckets[depth].len() && result.len() < max_count {
                let key = self.buckets[depth][j];
                let Some(url) = self.url_map.get(&key) else {
                    // Orphan key (no URL in map) - remove stale bucket entry
                    self.buckets[depth].swap_remove(j);
                    continue;
                };
                if !filter(url, depth) {
                    j += 1;
                    continue;
                }
                let owned_url = match self.url_map.remove(&key) {
                    Some(u) => u,
                    None => {
                        j += 1;
                        continue;
                    }
                };
                let task = CrawlTask::new(owned_url, depth);
                self.buckets[depth].swap_remove(j);
                result.push(task);
            }
            if result.len() >= max_count {
                break;
            }
        }
        result
    }

    /// Check if buckets are empty
    fn is_empty(&self) -> bool {
        self.url_map.is_empty() && self.collisions.is_empty()
    }

    /// Total number of tasks
    fn len(&self) -> usize {
        self.url_map.len() + self.collisions.len()
    }

    /// Hash a Url to usize
    fn hash_url(url: &Url) -> usize {
        use std::hash::{Hash, Hasher};
        let mut hasher = ahash::AHasher::default();
        url.as_str().hash(&mut hasher);
        hasher.finish() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crawl_buckets_no_duplicate_on_reinsert() {
        let mut buckets = CrawlTaskBuckets::new(5);
        let url = Url::parse("https://example.com/page").unwrap();

        buckets.insert(url.clone(), 0);
        buckets.insert(url.clone(), 0); // Insert same URL again

        // Extract all tasks - should only get 1
        let tasks = buckets.extract_if(|_, _| true, 10);
        assert_eq!(
            tasks.len(),
            1,
            "Duplicate URL should not be extracted twice"
        );
    }

    #[test]
    fn test_crawl_buckets_contains() {
        let mut buckets = CrawlTaskBuckets::new(5);
        let url = Url::parse("https://example.com/page").unwrap();

        assert!(!buckets.contains(&url));
        buckets.insert(url.clone(), 2);
        assert!(buckets.contains(&url));
    }

    #[test]
    fn test_crawl_buckets_extract_respects_depth_order() {
        let mut buckets = CrawlTaskBuckets::new(5);

        // Insert URLs at different depths
        let shallow = Url::parse("https://example.com/shallow").unwrap();
        let deep = Url::parse("https://example.com/deep").unwrap();

        buckets.insert(shallow.clone(), 1);
        buckets.insert(deep.clone(), 4);

        // Extract all - should get deeper pages first (per comment at line 55)
        let tasks = buckets.extract_if(|_, _| true, 10);

        assert_eq!(tasks.len(), 2);
        // First task should be from deeper depth (4)
        assert_eq!(
            tasks[0].crawl_depth, 4,
            "Deeper pages should be extracted first"
        );
        assert_eq!(tasks[1].crawl_depth, 1);
    }
}
