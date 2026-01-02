use anyhow::{Context, Result};
use clap::Parser;
use fastbloom::BloomFilter;
use futures::FutureExt;
use lol_html::{element, text, HtmlRewriter, Settings};
use reqwest::Client;
use reqwest_middleware::{ClientWithMiddleware, ClientBuilder};
use reqwest_retry::RetryTransientMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use std::collections::{BinaryHeap, HashSet};
use std::cmp::Ordering;
use std::fs::read_to_string;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tantivy::schema::{IndexRecordOption, STORED, Schema, TEXT, TextFieldIndexing, TextOptions};
use tantivy::{doc, Index};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use url::Url;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(help = "Path to text file containing domains (one per line)")]
    input_file: String,
    #[arg(short = 'x', long = "exclude")]
    exclude: Vec<String>,
    #[arg(short = 'd', long = "depth", default_value_t = 5)]
    max_depth: usize,
    #[arg(short = 'c', long = "concurrency", default_value_t = 500)]
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

struct Spider {
    client: ClientWithMiddleware,
    excluded_prefixes: Vec<String>,
    max_depth: usize,
    concurrency_limit: usize,
    crawled_count: AtomicU64,
    failed_count: AtomicU64,
    verbose: bool,
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
        ////////////////////////////////////////////////////////////////////////////////////////////////////////////
        // Set up Tantivy index
        ////////////////////////////////////////////////////////////////////////////////////////////////////////////
        let tantivy_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .filter(tantivy::tokenizer::Stemmer::new(tantivy::tokenizer::Language::English))
            //.filter(  tantivy::tokenizer::SplitCompoundWords::from_dictionary(dict))
            .build();
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
        ////////////////////////////////////////////////////////////////////////////////////////////////////////////
        // End set up Tantivy index
        ////////////////////////////////////////////////////////////////////////////////////////////////////////////
        
        let expected_items = 100_000_000;
        let mut visited_urls = BloomFilter::with_false_pos(1.0/(expected_items as f64))
            .hasher(ahash::RandomState::new())
            .expected_items(expected_items);
        let mut active_domains = HashSet::with_hasher(ahash::RandomState::new());
        let mut queue: BinaryHeap<HeapItem> = BinaryHeap::new();
        for url in start_urls {
            queue.push(HeapItem { url, depth: 0, retries: 0 });
        }

        let mut workers = JoinSet::new();

        let mut last_debug_time = std::time::Instant::now();
        loop {
            while workers.len() <= self.concurrency_limit {
                // Pop items until we find one whose domain is not active, buffering the others.
                let mut buffer: Vec<HeapItem> = Vec::new();
                let mut found: Option<HeapItem> = None;

                while let Some(item) = queue.pop() {
                    if let Some(host_str) = item.url.host_str() {
                        if !active_domains.contains(host_str) {
                            found = Some(item);
                            break;
                        }
                    }
                    if visited_urls.contains(&item.url.to_string()) {
                        // already visited, skip
                        continue;
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

                let spider = Arc::clone(&self);
                let bind_url = item.url.to_string();
                visited_urls.insert(&bind_url);
                active_domains.insert(host.clone());
                workers.spawn(async move {
                    (host,spider.process_url(item).await)
                });
            }

            if workers.is_empty() && queue.is_empty() {
                break;
            }

            if let Some(res) = workers.join_next().await {
                match res{
                    Ok((host,ret)) => {
                        active_domains.remove(&host);
                        let (links, data) = ret;
                        if let Some(crawled) = data {
                            match writer.add_document(doc!(url_field => crawled.url, body_field => crawled.body, title_field => crawled.title)) {
                                Ok(_opstamp) => {},
                                Err(e) => {
                                    eprintln!("Indexing error: {:?}", e);
                                }
                            }
                            writer.commit().expect("Failed to commit index");
                        }
                        for item in links {
                            let was_visited = visited_urls.contains(&item.url.to_string());
                            if item.depth <= self.max_depth && !was_visited && !self.should_skip(&item.url.to_string()) {
                                queue.push(item);
                            }
                        }
                    },
                    Err(e) => {
                        if self.verbose {
                            eprintln!("Worker task failed: {:?}", e);
                        }
                    }
                }
            }
            // Periodic debug output
            if last_debug_time.elapsed().as_secs() >= 5 {
                println!("Crawled: {}, Failed: {}, Queue size: {}, Active domains: {}",
                    self.crawled_count.load(std::sync::atomic::Ordering::Relaxed),
                    self.failed_count.load(std::sync::atomic::Ordering::Relaxed),
                    queue.len(),
                    active_domains.len(),
                );
                last_debug_time = std::time::Instant::now();
            }
        }
        println!("Crawl finished. Docs in index: {}", index.reader()?.searcher().num_docs());
        Ok(())
    }

    async fn process_url(&self, item : HeapItem) -> (Vec<HeapItem>,Option<CrawledData>) {
        let HeapItem { url, depth, retries } = item;
        
        let response = match self.client.get(url.clone()).send().await{
            Ok(resp) => resp,
            Err(e) => {
                if self.verbose {
                    eprintln!("Network error: {:?}", e);
                }        
                self.failed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return (vec![], None);
            }
        };
        if !response.status().is_success() { return (vec![], None); }
        
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
            
        if !content_type.starts_with("text/html") { return (vec![], None); }
        
        let (links, body_text, title) = match self.parse_stream(response).await {
            Ok(res) => res,
            Err(e) => {
                if self.verbose {
                    eprintln!("Parse error for {}: {:?}", url, e);
                }        
                self.failed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return (vec![], None);
            }
        };

        //println!("Depth {depth}: Crawling {url_str}");
        self.crawled_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let next_depth = depth + 1;
        let normalized = links.into_iter()
            .filter_map(|l| url.join(&l).ok())
            .map(|mut u| { u.set_fragment(None); u })
            .filter(|u| {
                u.host_str().map_or(false, |host| url.host_str().map_or(false, |base_host| host == base_host))
            })
            .map(|url| HeapItem{url, depth:next_depth, retries})
            .collect();

        (normalized, Some(CrawledData { url: url.to_string(), body: body_text, title }))
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
        use unicode_normalization::UnicodeNormalization;
        let body_text = body_text.nfkc().collect::<String>();
        Ok((links, body_text, title))
    }

    fn should_skip(&self, url: &str) -> bool {
        self.excluded_prefixes.iter().any(|p| url.starts_with(p))
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {

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
            start_urls.push(start_url);
        }
    }

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
        excluded_prefixes: args.exclude,
        max_depth: args.max_depth,
        concurrency_limit: args.concurrency,
        crawled_count: AtomicU64::new(0),
        failed_count: AtomicU64::new(0),
        verbose: args.verbose,
    });

    spider.run(start_urls).await?;

    Ok(())
}