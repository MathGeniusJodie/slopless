use anyhow::{Context, Result};
use clap::Parser;
use dashmap::DashSet;
use lol_html::{element, text, HtmlRewriter, Settings};
use reqwest::Client;
use reqwest_middleware::{ClientWithMiddleware, ClientBuilder};
use reqwest_retry::RetryTransientMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use std::collections::BinaryHeap;
use std::cmp::Ordering;
use std::fs::read_to_string;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tantivy::schema::{IndexRecordOption, STORED, Schema, TEXT, TextFieldIndexing, TextOptions};
use tantivy::{doc, Index};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use url::Url;
#[cfg(feature = "nlprule")]
use nlprule::{Rules, Tokenizer, tokenizer_filename, rules_filename};


#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(help = "Path to text file containing domains (one per line)")]
    input_file: String,
    #[arg(short = 'x', long = "exclude")]
    exclude: Vec<String>,
    #[arg(short = 'd', long = "depth", default_value_t = 5)]
    max_depth: usize,
    #[arg(short = 'c', long = "concurrency", default_value_t = 200)]
    concurrency: usize,
    // verbosity flag could be added here
    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose: bool,
}

struct CrawledData {
    url: String,
    title: String,
    body: String,
}

/// RAII Guard to ensure a domain is marked as "inactive" when the worker finishes.
struct DomainGuard {
    host: String,
    active_domains: Arc<DashSet<String, ahash::RandomState>>,
}

impl Drop for DomainGuard {
    fn drop(&mut self) {
        self.active_domains.remove(&self.host);
    }
}

struct Spider {
    client: ClientWithMiddleware,
    visited_urls: DashSet<String, ahash::RandomState>,
    active_domains: Arc<DashSet<String, ahash::RandomState>>, // Tracks which domains are currently being requested
    excluded_prefixes: Vec<String>,
    max_depth: usize,
    concurrency_limit: usize,
    index_tx: mpsc::UnboundedSender<CrawledData>,
    // atomic counter for crawled pages and failures
    crawled_count: AtomicU64,
    failed_count: AtomicU64,
    verbose: bool,
    //stemmer: rust_stemmers::Stemmer,
    #[cfg(feature = "nlprule")]
    rules: Rules,
    #[cfg(feature = "nlprule")]
    tokenizer: Tokenizer,
}

#[derive(Eq, PartialEq)]
struct HeapItem {
    depth: usize,
    url: Url,
    retries: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; reverse depth comparison to get a min-heap by depth.
        other.depth.cmp(&self.depth)
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Spider {
    pub async fn run(self: Arc<Self>, start_urls: Vec<Url>) -> Result<()> {
        let mut queue: BinaryHeap<HeapItem> = BinaryHeap::new();
        for url in start_urls {
            queue.push(HeapItem { url, depth: 0, retries: 0 });
        }

        let mut workers = JoinSet::new();

        let mut last_debug_time = std::time::Instant::now();
        loop {
            // 1. Try to spawn workers up to the concurrency limit
            while workers.len() < self.concurrency_limit {
                // Pop items until we find one whose domain is not active, buffering the others.
                let mut buffer: Vec<HeapItem> = Vec::new();
                let mut found: Option<HeapItem> = None;

                while let Some(item) = queue.pop() {
                    if let Some(host_str) = item.url.host_str() {
                        if !self.active_domains.contains(host_str) {
                            found = Some(item);
                            break;
                        }
                    }
                    buffer.push(item);
                }

                // Push buffered items back onto the heap
                for it in buffer.into_iter() { queue.push(it); }

                let item = match found {
                    Some(i) => i,
                    None => break, // No available item for an idle domain
                };

                let host = match item.url.host_str() {
                    Some(h) => h.to_string(),
                    None => continue,
                };

                // Mark domain as active
                self.active_domains.insert(host.clone());

                let spider = Arc::clone(&self);
                workers.spawn(async move {
                    let _guard = DomainGuard { host, active_domains: Arc::clone(&spider.active_domains) };
                    spider.process_url(item.url, item.depth, item.retries).await
                });
            }

            if workers.is_empty() && queue.is_empty() {
                break;
            }

            // 2. Wait for at least one worker to finish. 
            // This frees up a concurrency slot and potentially a domain lock.
            if let Some(worker_result) = workers.join_next().await {
                match worker_result? {
                    Ok(found_links) => {
                        for (link, depth, retries) in found_links {
                            if depth <= self.max_depth && !self.visited_urls.contains(link.as_str()) {
                                queue.push(HeapItem { url: link, depth, retries });
                            }
                        }
                    }
                    Err(e) => {
                        if self.verbose {
                            eprintln!("Worker error: {:?}", e);
                        }
                        
                        self.failed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // todo: re-queuing the URL with incremented retry count
                    }
                }
            }
            // Periodic debug output
            if last_debug_time.elapsed().as_secs() >= 5 {
                println!("Crawled: {}, Failed: {}, Queue size: {}, Active domains: {}",
                    self.crawled_count.load(std::sync::atomic::Ordering::Relaxed),
                    self.failed_count.load(std::sync::atomic::Ordering::Relaxed),
                    queue.len(),
                    self.active_domains.len(),
                );
                last_debug_time = std::time::Instant::now();
            }
        }
        Ok(())
    }

    async fn process_url(&self, url: Url, depth: usize, retries: usize) -> Result<Vec<(Url, usize, usize)>> {
        let url_str = url.to_string();
        
        // Double check visited (checked again here to prevent race conditions)
        if self.should_skip(&url_str) || !self.visited_urls.insert(url_str.clone()) {
            return Ok(vec![]);
        }

        let response = self.client.get(url.clone()).send().await?;
        if !response.status().is_success() { return Ok(vec![]); }
        
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
            
        if !content_type.starts_with("text/html") { return Ok(vec![]); }
        
        let (links, body_text, title) = self.parse_stream(response).await?;
        let _ = self.index_tx.send(CrawledData { url: url_str, body: body_text, title });

        //println!("Depth {depth}: Crawling {url_str}");
        self.crawled_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let next_depth = depth + 1;
        let normalized = links.into_iter()
            .filter_map(|l| url.join(&l).ok())
            .map(|mut u| { u.set_fragment(None); u })
            .filter(|u| {
                u.host_str().map_or(false, |host| url.host_str().map_or(false, |base_host| host == base_host))
            })
            .map(|u| (u, next_depth, retries))
            .collect();

        Ok(normalized)
    }

    async fn parse_stream(&self, response: reqwest::Response) -> Result<(Vec<String>, String, String)> {
        let response_text = response.text().await?;
        let mut links = Vec::new();
        let mut title = String::new();
        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![
                    element!("a[href]", |el| {
                        if let Some(href) = el.get_attribute("href") { links.push(href); }
                        Ok(())
                    }),
                    text!("title", |chunk| {
                        title = chunk.as_str().to_string();
                        Ok(())
                    }),
                ],
                ..Settings::default()
            },
            |_: &[u8]| {},
        );

        rewriter.write(response_text.as_bytes())?;
        rewriter.end()?;

        use dom_smoothie::{Readability, Article, Config};
        let readability_config = Config::default();
        let mut readability = Readability::new(response_text, None, Some(readability_config))?;
        let article: Article = readability.parse()?;
        let body_text = article.text_content;
        
        //unicode normalization
        use unicode_normalization::UnicodeNormalization;
        #[cfg(feature = "nlprule")]
        let mut body_text = body_text.nfkc().collect::<String>();
        #[cfg(not(feature = "nlprule"))]
        let body_text = body_text.nfkc().collect::<String>();
        
        // nlprule feature flag
        #[cfg(feature = "nlprule")]
        {
            //let body_text = self.rules.correct(&body_text, &self.tokenizer);

            // lemmaize
            let tokens = self.tokenizer.pipe(&body_text);
            let mut decomp_body_text = String::new();
            for sentence in tokens {
                for token in sentence.iter() {
                    let lemma = token.word().tags()[0].lemma().as_str();
                    decomp_body_text.push_str(lemma);
                    decomp_body_text.push(' ');
                }
            }
            body_text = decomp_body_text;
        }

        Ok((links, body_text, title))
    }

    fn should_skip(&self, url: &str) -> bool {
        self.excluded_prefixes.iter().any(|p| url.starts_with(p))
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {
    #[cfg(feature = "nlprule")]
    let mut tokenizer_bytes: &'static [u8] = include_bytes!(concat!(env!("OUT_DIR"),"/",tokenizer_filename!("en")));
    #[cfg(feature = "nlprule")]
    let mut rules_bytes: &'static [u8] = include_bytes!(concat!(env!("OUT_DIR"),"/",rules_filename!("en")));
    #[cfg(feature = "nlprule")]
    let tokenizer = Tokenizer::from_reader(&mut tokenizer_bytes).expect("tokenizer binary is valid");
    #[cfg(feature = "nlprule")]
    let rules = Rules::from_reader(&mut rules_bytes).expect("rules binary is valid");

    let tantivy_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        //.filter(tantivy::tokenizer::AsciiFoldingFilter)
        .filter(tantivy::tokenizer::Stemmer::new(tantivy::tokenizer::Language::English))
        //.filter(  tantivy::tokenizer::SplitCompoundWords::from_dictionary(dict))
        .build();

    let args = Args::parse();

    let content = read_to_string(&args.input_file)
        .with_context(|| format!("Failed to read input file: {}", args.input_file))?;
    
    let mut start_urls = Vec::new();

    for line in content.lines() {
        let domain = line.trim();
        if domain.is_empty() { continue; }
        let clean_domain = domain.trim_start_matches("http://").trim_start_matches("https://");
        let start_url = match Url::parse(&format!("https://{clean_domain}"))
            .context("Invalid domain") {
                Ok(url) => url,
                Err(e) => {
                    eprintln!("Skipping invalid domain '{}': {:?}", domain, e);
                    continue;
                }
            };
        
        if let Some(_host) = start_url.host_str() {
            //allowed_domains.insert(host.to_string());
            start_urls.push(start_url);
        }
    }

    let text_field_indexing = TextFieldIndexing::default()
    .set_tokenizer("norm_tokenizer")
    .set_index_option(IndexRecordOption::WithFreqsAndPositions);

    let text_options = TextOptions::default()
    .set_indexing_options(text_field_indexing);

    let mut schema_builder = Schema::builder();
    let url_field = schema_builder.add_text_field("url", TEXT | STORED);
    let title_field = schema_builder.add_text_field("title", TEXT | STORED);
    let body_field = schema_builder.add_text_field("body", text_options);
    let schema = schema_builder.build();
    
    let index = if std::path::Path::new("search_db").exists() {
        Index::open_in_dir("search_db")?
    } else {
        std::fs::create_dir_all("search_db")?;
        let settings = tantivy::IndexSettings::default();
        Index::create(tantivy::directory::MmapDirectory::open("search_db")?, schema, settings)?
    };
    index.tokenizers()
        .register("norm_tokenizer", tantivy_tokenizer);

    let mut writer = index.writer(1_000_000_000)?;
    let (index_tx, mut index_rx) = mpsc::unbounded_channel::<CrawledData>();
    
    let indexer_handle = tokio::spawn(async move {
        while let Some(data) = index_rx.recv().await {
            let _ = writer.add_document(doc!(url_field => data.url, body_field => data.body, title_field => data.title));
        }
        writer.commit().expect("Failed to commit index");
    });

    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4);

    let spider = Arc::new(Spider {
        client: ClientBuilder::new(
            Client::builder()
            // chrome
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
            // max concurency for multiple hosts
            .pool_max_idle_per_host(0)
            .pool_idle_timeout(None)
            //.timeout(std::time::Duration::from_secs(5))
            .tcp_nodelay(true)
            .build()?
        )
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build(),
        index_tx,
        visited_urls: DashSet::with_hasher(ahash::RandomState::new()),
        active_domains: Arc::new(DashSet::with_hasher(ahash::RandomState::new())),
        excluded_prefixes: args.exclude,
        max_depth: args.max_depth,
        concurrency_limit: args.concurrency,
        crawled_count: AtomicU64::new(0),
        failed_count: AtomicU64::new(0),
        verbose: args.verbose,
        #[cfg(feature = "nlprule")]
        rules,
        #[cfg(feature = "nlprule")]
        tokenizer,
    });

    spider.run(start_urls).await?;
    
    indexer_handle.await.context("Indexer task panicked")?;
    println!("Crawl finished. Docs in index: {}", index.reader()?.searcher().num_docs());

    Ok(())
}