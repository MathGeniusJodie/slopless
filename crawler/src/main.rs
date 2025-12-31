use anyhow::{Context, Result};
use clap::Parser;
use dashmap::DashSet;
use lol_html::{element, text, HtmlRewriter, Settings};
use reqwest::Client;
use reqwest_middleware::{ClientWithMiddleware, ClientBuilder};
use reqwest_retry::RetryTransientMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use std::collections::{HashSet, VecDeque};
use std::fs::read_to_string;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tantivy::schema::{Schema, STORED, TEXT};
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

/// RAII Guard to ensure a domain is marked as "inactive" when the worker finishes.
struct DomainGuard {
    host: String,
    active_domains: Arc<DashSet<String>>,
}

impl Drop for DomainGuard {
    fn drop(&mut self) {
        self.active_domains.remove(&self.host);
    }
}

struct Spider {
    client: ClientWithMiddleware,
    visited_urls: DashSet<String>,
    active_domains: Arc<DashSet<String>>, // Tracks which domains are currently being requested
    allowed_domains: HashSet<String>,
    excluded_prefixes: Vec<String>,
    max_depth: usize,
    concurrency_limit: usize,
    index_tx: mpsc::UnboundedSender<CrawledData>,
    // atomic counter for crawled pages and failures
    crawled_count: AtomicU64,
    failed_count: AtomicU64,
    verbose: bool,
}


fn insert_sorted_depth(deque: &mut VecDeque<(Url, usize, usize)>, item: (Url, usize, usize), depth: usize) {
    let target_depth = item.1;

    // 1. Add to the end of the deque: O(1)
    deque.push_back(item);
    let mut current_idx = deque.len() - 1;

    // 2. Step through each possible depth level deeper than our target.
    // We go from 4 down to (target_depth + 1).
    for d in (target_depth + 1..depth).rev() {
        // binary_search_by can exit as soon as it finds ANY element with depth 'd'.
        // In a very long deque with only 5 levels, the 'mid' point of your 
        // search is extremely likely to hit this depth immediately.
        let search_result = deque.binary_search_by(|probe| probe.1.cmp(&d));

        match search_result {
            // Found an element with depth 'd' at 'swap_idx'
            Ok(swap_idx) => {
                if swap_idx < current_idx {
                    deque.swap(current_idx, swap_idx);
                    current_idx = swap_idx;
                }
            }
            // Depth 'd' doesn't exist. 'idx' is where it WOULD be (the boundary).
            // We can swap with this boundary to jump past all levels > d.
            Err(idx) => {
                if idx < current_idx {
                    deque.swap(current_idx, idx);
                    current_idx = idx;
                }
            }
        }
    }
}

impl Spider {
    pub async fn run(self: Arc<Self>, start_urls: Vec<Url>) -> Result<()> {
        let mut queue = VecDeque::new();
        for url in start_urls {
            queue.push_back((url, 0, 0));
        }

        let mut workers = JoinSet::new();

        let mut last_debug_time = std::time::Instant::now();
        loop {
            // 1. Try to spawn workers up to the concurrency limit
            while workers.len() < self.concurrency_limit {
                // Find the URL with the smallest depth whose domain is NOT currently being processed
                let next_idx = queue.iter().position(|(url, _, _)| {
                    if let Some(host) = url.host_str() {
                        !self.active_domains.contains(host)
                    } else {
                        false
                    }
                });

                if let Some(idx) = next_idx {
                    //let (url, depth) = queue.remove(idx).expect("Index must exist");
                    // swap remove to avoid shifting elements
                    let (url, depth, retries) = {
                        queue.swap(idx, 0);
                        queue.pop_front().expect("Index must exist")
                    };
                    let host = match url.host_str() {
                        Some(h) => h.to_string(),
                        None => continue, // Skip URLs without a valid host
                    };
                    
                    // Mark domain as active
                    self.active_domains.insert(host.clone());

                    let spider = Arc::clone(&self);
                    workers.spawn(async move { 
                        // The guard is created inside the task to release the domain when finished
                        let _guard = DomainGuard { host, active_domains: Arc::clone(&spider.active_domains) };
                        spider.process_url(url, depth, retries).await 
                    });
                } else {
                    // No URLs available for idle domains, stop trying to spawn for now
                    break;
                }
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
                                insert_sorted_depth(&mut queue, (link, depth, retries), self.max_depth);
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
            if last_debug_time.elapsed().as_secs() >= 10 {
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

        //let _permit = self.concurrency_limit.acquire().await?;
        
        let response = self.client.get(url.clone()).send().await?;
        if !response.status().is_success() { return Ok(vec![]); }
        
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
            
        if !content_type.starts_with("text/html") { return Ok(vec![]); }
        
        

        let (links, body_text, title) = self.parse_stream(response,url.clone()).await?;
        let _ = self.index_tx.send(CrawledData { url: url_str, body: body_text, title });

        //println!("Depth {depth}: Crawling {url_str}");
        self.crawled_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let next_depth = depth + 1;
        let normalized = links.into_iter()
            .filter_map(|l| url.join(&l).ok())
            .map(|mut u| { u.set_fragment(None); u })
            .filter(|u| {
                u.host_str().map_or(false, |h| self.allowed_domains.contains(h))
            })
            .map(|u| (u, next_depth, retries))
            .collect();

        Ok(normalized)
    }

    async fn parse_stream(&self, mut response: reqwest::Response, url:Url) -> Result<(Vec<String>, String, String)> {
        let (tx, mut rx) = mpsc::unbounded_channel::<bytes::Bytes>();
        let parse_handle = tokio::task::spawn_blocking(move || -> Result<(Vec<String>, String)> {
            let mut links = Vec::new();
            let mut titles = Vec::new();
            let mut rewriter = HtmlRewriter::new(
                Settings {
                    element_content_handlers: vec![
                        element!("a[href]", |el| {
                            if let Some(href) = el.get_attribute("href") { links.push(href); }
                            Ok(())
                        }),
                        text!("title", |chunk| {
                            titles.push(chunk.as_str().to_string());
                            Ok(())
                        }),
                    ],
                    ..Settings::default()
                },
                |_: &[u8]| {},
            );
            while let Some(chunk) = rx.blocking_recv() { rewriter.write(&chunk)?; }
            rewriter.end()?;

            Ok((links, titles.join(" ")))
        });

        let body_text = readability::extractor::scrape(&url.to_string())?.text;
        let body_text = rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::English)
            .stem(&body_text)
            .to_string();
        // truncate body text to 10k characters
        let body_text = if body_text.len() > 10000 {
            body_text[..10000].to_string()
        } else {
            body_text
        };

        while let Some(chunk) = response.chunk().await? { let _ = tx.send(chunk); }
        drop(tx);
        let meta = parse_handle.await??;
        Ok((meta.0, body_text, meta.1))
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
    let mut allowed_domains = HashSet::new();

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
        
        if let Some(host) = start_url.host_str() {
            allowed_domains.insert(host.to_string());
            start_urls.push(start_url);
        }
    }

    let mut schema_builder = Schema::builder();
    let url_field = schema_builder.add_text_field("url", TEXT | STORED);
    let title_field = schema_builder.add_text_field("title", TEXT | STORED);
    let body_field = schema_builder.add_text_field("body", TEXT);
    let schema = schema_builder.build();
    
    let index = if std::path::Path::new("search_db").exists() {
        Index::open_in_dir("search_db")?
    } else {
        std::fs::create_dir_all("search_db")?;
        let settings = tantivy::IndexSettings::default();
        Index::create(tantivy::directory::MmapDirectory::open("search_db")?, schema, settings)?
    };

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
        visited_urls: DashSet::new(),
        active_domains: Arc::new(DashSet::new()),
        allowed_domains,
        excluded_prefixes: args.exclude,
        max_depth: args.max_depth,
        concurrency_limit: args.concurrency,
        crawled_count: AtomicU64::new(0),
        failed_count: AtomicU64::new(0),
        verbose: args.verbose,
    });

    spider.run(start_urls).await?;
    
    indexer_handle.await.context("Indexer task panicked")?;
    println!("Crawl finished. Docs in index: {}", index.reader()?.searcher().num_docs());

    Ok(())
}