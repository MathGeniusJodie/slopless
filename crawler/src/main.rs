use anyhow::{Context, Result};
use clap::Parser;
use lol_html::{element, text, HtmlRewriter, Settings};
use reqwest::Client;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::fs::read_to_string;
use std::sync::Arc;
use tantivy::schema::{FAST, IndexRecordOption, STORED, STRING, Schema, TEXT, TextFieldIndexing, TextOptions, Value};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index};
use tokio::task::JoinSet;
use url::Url;

// ============================================================================
// Inlined and modified BloomFilter implementation from fastbloom
// ============================================================================

mod bloom {
    use std::cmp::max;
    use std::f64::consts::LN_2;
    use std::hash::{BuildHasher, Hash, Hasher};

    /// A bit vector partitioned into u64 blocks.
    #[derive(Debug, Clone)]
    pub struct BitVec {
        bits: Box<[u64]>,
    }

    impl BitVec {
        #[inline]
        pub fn new(num_bits: usize) -> Self {
            let num_u64s = (num_bits + 63) / 64;
            Self {
                bits: vec![0u64; num_u64s].into_boxed_slice(),
            }
        }

        #[inline]
        pub const fn len(&self) -> usize {
            self.bits.len()
        }

        #[inline]
        pub const fn num_bits(&self) -> usize {
            self.len() * 64
        }

        #[inline]
        pub fn check(&self, index: usize) -> bool {
            let (idx, bit) = coord(index);
            self.bits[idx] & bit > 0
        }

        #[inline]
        pub fn set(&mut self, index: usize) -> bool {
            let (idx, bit) = coord(index);
            let previously_contained = self.bits[idx] & bit > 0;
            self.bits[idx] |= bit;
            previously_contained
        }

        #[inline]
        pub fn clear(&mut self) {
            for i in 0..self.len() {
                self.bits[i] = 0;
            }
        }
    }

    #[inline]
    fn coord(index: usize) -> (usize, u64) {
        (index >> 6, 1u64 << (index & 0b111111))
    }

    /// Double hashing to derive multiple hash functions from a single source hash.
    /// Adapted from https://www.eecs.harvard.edu/~michaelm/postscripts/rsa2008.pdf
    #[derive(Clone, Copy)]
    pub struct DoubleHasher {
        h1: u64,
        h2: u64,
    }

    impl DoubleHasher {
        #[inline]
        pub fn new(hash: u64) -> Self {
            let h2 = hash
                .wrapping_shr(32)
                .wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
            Self { h1: hash, h2 }
        }

        #[inline]
        pub fn next(&mut self) -> u64 {
            self.h1 = self.h1.wrapping_add(self.h2).rotate_left(5);
            self.h1
        }
    }

    /// Returns the bit index for an item's hash using fast modulo reduction.
    /// https://lemire.me/blog/2016/06/27/a-fast-alternative-to-the-modulo-reduction/
    #[inline]
    pub fn index(num_bits: usize, hash: u64) -> usize {
        (((hash >> 32).wrapping_mul(num_bits as u64)) >> 32) as usize
    }

    /// Compute optimal number of hash functions for given filter size and expected items.
    #[inline]
    pub fn optimal_hashes(num_u64s: usize, num_items: usize) -> u32 {
        let num_bits = (num_u64s * 64) as f64;
        let hashes = LN_2 * num_bits / num_items as f64;
        max(hashes as u32, 1)
    }

    /// Compute optimal filter size for given expected items and false positive rate.
    #[inline]
    pub fn optimal_size(items_count: usize, fp_rate: f64) -> usize {
        let log2_2 = LN_2 * LN_2;
        let result = 8 * ((items_count as f64) * fp_rate.ln() / (-8.0 * log2_2)).ceil() as usize;
        max(result, 512)
    }

    /// A space-efficient probabilistic data structure for set membership testing.
    #[derive(Debug, Clone)]
    pub struct BloomFilter<S> {
        bits: BitVec,
        num_hashes: u32,
        hasher: S,
    }

    impl<S: BuildHasher> BloomFilter<S> {
        /// Create a new bloom filter with the specified number of bits and hasher.
        pub fn with_num_bits(num_bits: usize, hasher: S, expected_items: usize) -> Self {
            let bits = BitVec::new(num_bits);
            let num_hashes = optimal_hashes(bits.len(), expected_items);
            Self {
                bits,
                num_hashes,
                hasher,
            }
        }

        /// Compute the source hash for a value.
        #[inline]
        pub fn source_hash(&self, val: &(impl Hash + ?Sized)) -> u64 {
            let mut state = self.hasher.build_hasher();
            val.hash(&mut state);
            state.finish()
        }

        /// Get the bit positions that would be set/checked for a given source hash.
        /// This is the key method that fastbloom doesn't expose!
        #[inline]
        pub fn bit_positions_hash(&self, source_hash: u64) -> u64 {
            let mut hasher = DoubleHasher::new(source_hash);
            let num_bits = self.bits.num_bits();
            let mut all_positions = 0;
            for _ in 0..self.num_hashes {
                let h = hasher.next();
                let position = index(num_bits, h);
                let mut state = self.hasher.build_hasher();
                position.hash(&mut state);
                all_positions ^= state.finish();
            }
            all_positions
        }

        /// Check if a value is possibly in the filter.
        #[inline]
        pub fn contains(&self, val: &(impl Hash + ?Sized)) -> bool {
            let source_hash = self.source_hash(val);
            self.contains_hash(source_hash)
        }

        /// Check if a source hash is possibly in the filter.
        #[inline]
        pub fn contains_hash(&self, source_hash: u64) -> bool {
            let mut hasher = DoubleHasher::new(source_hash);
            let num_bits = self.bits.num_bits();
            (0..self.num_hashes).all(|_| {
                let h = hasher.next();
                self.bits.check(index(num_bits, h))
            })
        }

        /// Insert a value into the filter. Returns true if the value may have been present.
        #[inline]
        pub fn insert(&mut self, val: &(impl Hash + ?Sized)) -> bool {
            let source_hash = self.source_hash(val);
            self.insert_hash(source_hash)
        }

        /// Insert a source hash into the filter. Returns true if it may have been present.
        #[inline]
        pub fn insert_hash(&mut self, source_hash: u64) -> bool {
            let mut hasher = DoubleHasher::new(source_hash);
            let num_bits = self.bits.num_bits();
            let mut previously_contained = true;
            for _ in 0..self.num_hashes {
                let h = hasher.next();
                previously_contained &= self.bits.set(index(num_bits, h));
            }
            previously_contained
        }

        /// Returns the number of hash functions used.
        #[inline]
        pub fn num_hashes(&self) -> u32 {
            self.num_hashes
        }

        /// Returns the total number of bits in the filter.
        #[inline]
        pub fn num_bits(&self) -> usize {
            self.bits.num_bits()
        }

        /// Clear all bits in the filter.
        #[inline]
        pub fn clear(&mut self) {
            self.bits.clear();
        }
        #[inline]
        pub fn get_set_bits(&self) -> usize {
            self.bits.bits.iter().map(|b| b.count_ones() as usize).sum()
        }
    }
}

use bloom::BloomFilter;

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

struct IndexedPage {
    page_url: String,
    page_title: String,
    page_content: String,
}

struct WebCrawler {
    http_client: Client,
}

#[derive(Clone, Eq, PartialEq)]
struct CrawlTask {
    crawl_depth: usize,
    target_url: Url,
    retry_count: usize,
}

impl Ord for CrawlTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Order by depth (DESCENDING - process deepest first), then by URL for uniqueness
        other.crawl_depth  // Swap self/other to reverse order
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
    fn should_skip(
        &self,
        max_depth: usize,
        excluded_prefixes: &[String],
        db: &CrawlDb,
        searcher: &tantivy::Searcher,
        queued_urls: &HashSet<String, ahash::RandomState>,
    ) -> bool {
        if self.crawl_depth > max_depth {
            return true;
        }
        if excluded_prefixes
            .iter()
            .any(|prefix| self.target_url.as_str().starts_with(prefix)) {
            return true;
        }
        let url_str = self.target_url.to_string();
        queued_urls.contains(&url_str) || db.should_skip_url(&url_str, searcher)   
    }
}

struct SearchIndex {
    index: Index,
    url_field: tantivy::schema::Field,
    url_hash_field: tantivy::schema::Field,
    title_field: tantivy::schema::Field,
    body_field: tantivy::schema::Field,
}

struct CrawlDb {
    index: Index,
    index_writer: tantivy::IndexWriter,
    url_field: tantivy::schema::Field,
    url_hash_field: tantivy::schema::Field,
    title_field: tantivy::schema::Field,
    body_field: tantivy::schema::Field,
    seen_urls: BloomFilter<ahash::RandomState>,
    collisions: BloomFilter<ahash::RandomState>,
    uncommitted_hashes: HashSet<u64, ahash::RandomState>,
    uncommitted_urls: HashSet<String, ahash::RandomState>,
}

impl CrawlDb {
    fn new(search_index: SearchIndex, expected_url_count: usize) -> Result<Self> {
        let index_writer = search_index.index.writer(50_000_000)?;
        let seen_urls = BloomFilter::with_num_bits(
            expected_url_count * 8,
            ahash::RandomState::new(),
            expected_url_count,
        );
        let collisions = BloomFilter::with_num_bits(
            expected_url_count * 2,
            ahash::RandomState::new(),
            expected_url_count,
        );
        // 15% need to double check and 0.1% false positive rate w 600 million bits
        // 2% need to double check and no false positives w 1 billion bits
        let uncommitted_hashes = HashSet::with_hasher(ahash::RandomState::new());
        let uncommitted_urls = HashSet::with_hasher(ahash::RandomState::new());
        
        Ok(Self {
            index: search_index.index,
            index_writer,
            url_field: search_index.url_field,
            url_hash_field: search_index.url_hash_field,
            title_field: search_index.title_field,
            body_field: search_index.body_field,
            seen_urls,
            collisions,
            uncommitted_hashes,
            uncommitted_urls,
        })
    }

    fn searcher(&self) -> Result<tantivy::Searcher> {
        Ok(self.index.reader()?.searcher())
    }

    fn mark_seen(&mut self, url: &str) {
        let source_hash = self.seen_urls.source_hash(url);
        let bit_pos_hash = self.seen_urls.bit_positions_hash(source_hash);
        let collides = self.seen_urls.contains_hash(source_hash);
        if collides {
            self.collisions.insert(&bit_pos_hash);
        } else {
            self.seen_urls.insert_hash(source_hash);
        }
    }

    fn should_skip_url(&self, url: &str, searcher: &tantivy::Searcher) -> bool {
        // Check uncommitted URLs first (already added to tantivy but not committed)
        if self.uncommitted_urls.contains(url) {
            return true;
        }
        
        let maybe_in_index = self.seen_urls.contains(url);
        let source_hash = self.seen_urls.source_hash(url);
        let bit_pos_hash = self.seen_urls.bit_positions_hash(source_hash);
        if maybe_in_index {
            if !self.collisions.contains(&bit_pos_hash) {
                return true;
            }
        }
        
        // possible collision: must verify db
        use tantivy::collector::DocSetCollector;
        use tantivy::query::TermQuery;
        
        let term = tantivy::Term::from_field_u64(self.url_hash_field, bit_pos_hash);
        let query = TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        
        let Ok(doc_addresses) = searcher.search(&query, &DocSetCollector) else {
            return false;
        };

        // cannot be a false positive if there's only one document with this hash
        if doc_addresses.len() == 1 {
            return true;
        }
        
        // multiple documents with this hash - check actual URLs
        for doc_address in doc_addresses {
            if let Ok(doc) = searcher.doc::<tantivy::TantivyDocument>(doc_address) {
                if let Some(stored_url) = doc.get_first(self.url_field).and_then(|v| v.as_str()) {
                    if stored_url == url {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn index_page(&mut self, page: IndexedPage, bit_pos_hash: u64, searcher: &tantivy::Searcher) -> Result<()> {
        self.uncommitted_hashes.insert(bit_pos_hash);
        self.uncommitted_urls.insert(page.page_url.clone());
        
        let doc = doc!(
            self.url_field => page.page_url,
            self.url_hash_field => bit_pos_hash,
            self.title_field => page.page_title,
            self.body_field => page.page_content
        );
        self.index_writer.add_document(doc)?;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.index_writer.commit()?;
        self.uncommitted_hashes.clear();
        self.uncommitted_urls.clear();
        Ok(())
    }

    fn num_docs(&self) -> Result<u64> {
        Ok(self.index.reader()?.searcher().num_docs())
    }
}

fn setup_search_index() -> Result<SearchIndex> {
    let search_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(tantivy::tokenizer::Stemmer::new(
            tantivy::tokenizer::Language::English,
        ))
        .build();
    let body_field_indexing = TextFieldIndexing::default()
        .set_tokenizer("norm_tokenizer")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let body_field_options = TextOptions::default().set_indexing_options(body_field_indexing);
    let mut schema_builder = Schema::builder();
    let url_field = schema_builder.add_text_field("url", STRING | STORED | FAST);
    let url_hash_field = schema_builder.add_u64_field("url_hash", tantivy::schema::INDEXED | FAST);
    let title_field = schema_builder.add_text_field("title", TEXT | STORED);
    let body_field = schema_builder.add_text_field("body", body_field_options);
    let schema = schema_builder.build();
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
    Ok(SearchIndex {
        index,
        url_field,
        url_hash_field,
        title_field,
        body_field,
    })
}

fn pop_available_tasks(
    task_queue: &mut BTreeSet<CrawlTask>,
    domains_in_progress: &HashSet<String, ahash::RandomState>,
    max_tasks: usize,
) -> Vec<CrawlTask> {
    let mut result = Vec::with_capacity(max_tasks);
    let mut domains_claimed: HashSet<&str, ahash::RandomState> =
        HashSet::with_hasher(ahash::RandomState::new());

    // Single scan: collect tasks for domains not in progress and not yet claimed this batch
    let tasks_to_take: Vec<CrawlTask> = task_queue
        .iter()
        .filter_map(|task| {
            let domain = task.target_url.host_str()?;
            if !domains_in_progress.contains(domain) && !domains_claimed.contains(domain) {
                domains_claimed.insert(domain);
                Some(task.clone())
            } else {
                None
            }
        })
        .take(max_tasks)
        .collect();

    for task in tasks_to_take {
        if let Some(t) = task_queue.take(&task) {
            result.push(t);
        }
    }
    result
}

fn extract_page_content(html_body: String) -> Result<(Vec<String>, String, String)> {
    let mut extracted_links = Vec::new();
    let mut page_title = String::new();
    let mut html_rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("a[href]", |anchor| {
                    if let Some(href) = anchor.get_attribute("href") {
                        extracted_links.push(href);
                    }
                    Ok(())
                }),
                text!("title", |title_text| {
                    page_title = title_text.as_str().to_string();
                    Ok(())
                }),
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );

    html_rewriter.write(html_body.as_bytes())?;
    html_rewriter.end()?;

    use dom_smoothie::{Article, Config, Readability};
    let readability_config = Config::default();
    let mut readability_parser = Readability::new(html_body, None, Some(readability_config))?;
    let parsed_article: Article = readability_parser.parse()?;
    let readable_text = parsed_article.text_content;
    use unicode_normalization::UnicodeNormalization;
    let normalized_text = readable_text.nfkc().collect::<String>();
    Ok((extracted_links, normalized_text, page_title))
}

impl WebCrawler {
    async fn fetch_and_process_page(
        &self,
        task: CrawlTask,
    ) -> Result<(Vec<CrawlTask>, Option<IndexedPage>)> {
        let CrawlTask {
            target_url,
            crawl_depth,
            retry_count,
        } = task;

        let response = self.http_client.get(target_url.clone()).send().await?;
        if !response.status().is_success() {
            return Ok((vec![], None));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|header_value| header_value.to_str().ok())
            .unwrap_or("");

        if !content_type.starts_with("text/html") {
            return Ok((vec![], None));
        }
        let html_body = response.text().await?;
        let (extracted_links, page_content, page_title) = extract_page_content(html_body)?;

        let next_depth = crawl_depth + 1;
        let base_host = target_url.host_str();
        let child_tasks = extracted_links
            .into_iter()
            .filter_map(|link| {
                let mut url = target_url.join(&link).ok()?;
                url.set_fragment(None);
                (url.host_str() == base_host).then_some(CrawlTask {
                    target_url: url,
                    crawl_depth: next_depth,
                    retry_count,
                })
            })
            .collect();
        Ok((
            child_tasks,
            Some(IndexedPage {
                page_url: target_url.to_string(),
                page_content,
                page_title,
            }),
        ))
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {
    let cli_args = Args::parse();

    let file_content = read_to_string(&cli_args.input_file)
        .with_context(|| format!("Failed to read input file: {}", cli_args.input_file))?;

    let mut seed_urls = Vec::new();

    for line in file_content.lines() {
        let domain = line.trim();
        if domain.is_empty() {
            continue;
        }
        let clean_domain = domain
            .trim_start_matches("http://")
            .trim_start_matches("https://");
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

    let crawler = Arc::new(WebCrawler {
        http_client: Client::builder()
            // chrome
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
            // max concurrency for multiple hosts
            .pool_max_idle_per_host(0)
            .pool_idle_timeout(None)
            //.timeout(std::time::Duration::from_secs(5))
            .tcp_nodelay(true)
            .build()?
    });

    let search_index = setup_search_index()?;
    let expected_url_count = 100_000_000;
    let mut db = CrawlDb::new(search_index, expected_url_count)?;

    let (mut pages_crawled, mut pages_failed) = (0usize, 0usize);
    let mut domains_in_progress = HashSet::with_hasher(ahash::RandomState::new());
    let mut queued_urls: HashSet<String, ahash::RandomState> = HashSet::with_hasher(ahash::RandomState::new());
    let mut task_queue: BTreeSet<CrawlTask> = seed_urls
        .into_iter()
        .map(|url| {
            db.mark_seen(url.as_str());
            queued_urls.insert(url.to_string());
            CrawlTask {
                target_url: url,
                crawl_depth: 0,
                retry_count: 0,
            }
        })
        .collect();
    let mut worker_pool = JoinSet::new();
    let mut last_status_report = std::time::Instant::now();

    loop {
        // Spawn new tasks up to concurrency limit
        let slots_available = cli_args.max_concurrent_requests.saturating_sub(worker_pool.len());
        for task in pop_available_tasks(&mut task_queue, &domains_in_progress, slots_available) {
            let Some(domain) = task.target_url.host_str().map(String::from) else {
                continue;
            };
            let url_str = task.target_url.to_string();
            queued_urls.remove(&url_str);
            let source_hash = db.seen_urls.source_hash(&url_str);
            let url_hash = db.seen_urls.bit_positions_hash(source_hash);
            domains_in_progress.insert(domain.clone());
            let crawler = Arc::clone(&crawler);
            worker_pool.spawn(async move { (domain, url_hash, crawler.fetch_and_process_page(task).await) });
        }

        if worker_pool.is_empty() && task_queue.is_empty() {
            break;
        }

        // Process completed task
        let Some(Ok((domain, url_hash, crawl_result))) = worker_pool.join_next().await else {
            continue;
        };
        domains_in_progress.remove(&domain);

        let (discovered_links, page_data) = match crawl_result {
            Ok(result) => result,
            Err(e) => {
                pages_failed += 1;
                if cli_args.verbose_logging {
                    eprintln!("Error crawling {domain}: {e:?}");
                }
                continue;
            }
        };
        let searcher = db.searcher()?;
        for task in discovered_links {
            if !task.should_skip(
                cli_args.max_crawl_depth,
                &cli_args.excluded_url_prefixes,
                &db,
                &searcher,
                &queued_urls,
            ) {
                let url_str = task.target_url.to_string();
                db.mark_seen(&url_str);
                queued_urls.insert(url_str);
                task_queue.insert(task);
            }
        }

        // Index the page if we got content
        if let Some(page) = page_data {
            if let Err(e) = db.index_page(page, url_hash, &searcher) {
                eprintln!("Indexing error: {e:?}");
            }
        }

        pages_crawled += 1;

        // Periodic status report
        if last_status_report.elapsed().as_secs() >= 5 {
            println!(
                "Crawled: {pages_crawled}, Failed: {pages_failed}, Queue: {}, Active: {}",
                task_queue.len(),
                domains_in_progress.len()
            );
            println!(" Uncommitted: URLs {}, Hashes {}, Collisions {}",
                db.uncommitted_urls.len(),
                db.uncommitted_hashes.len(),
                db.collisions.get_set_bits(),
            );
            last_status_report = std::time::Instant::now();
            db.commit().expect("Failed to commit index");
        }
    }
    println!(
        "Crawl finished. Docs in index: {}",
        db.num_docs()?
    );

    Ok(())
}
