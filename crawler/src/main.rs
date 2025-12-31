use anyhow::{Context, Result};
use clap::Parser;
use dashmap::DashSet;
use lol_html::{element, text, HtmlRewriter, Settings};
use reqwest::Client;
use std::collections::{HashSet, VecDeque};
use std::fs::read_to_string;
use std::sync::Arc;
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{doc, Index, IndexSettings};
use tokio::sync::{mpsc, Semaphore};
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
    #[arg(short = 'c', long = "concurrency", default_value_t = 1000)]
    concurrency: usize,
}

struct CrawledData {
    url: String,
    body: String,
}

struct Spider {
    client: Client,
    visited_urls: DashSet<String>,
    // Changed from String to HashSet for multiple domain support
    allowed_domains: HashSet<String>,
    excluded_prefixes: Vec<String>,
    max_depth: usize,
    concurrency_limit: Arc<Semaphore>,
    index_tx: mpsc::UnboundedSender<CrawledData>,
}

impl Spider {
    // Modified to take a Vec of starting URLs
    pub async fn run(self: Arc<Self>, start_urls: Vec<Url>) -> Result<()> {
        let mut queue = VecDeque::new();
        for url in start_urls {
            queue.push_back((url, 0));
        }

        let mut workers = JoinSet::new();

        loop {
            while !queue.is_empty() && workers.len() < self.concurrency_limit.available_permits() {
                if let Some((url, depth)) = queue.pop_front() {
                    let spider = Arc::clone(&self);
                    workers.spawn(async move { spider.process_url(url, depth).await });
                }
            }

            if workers.is_empty() { break; }

            if let Some(worker_result) = workers.join_next().await {
                match worker_result? {
                    Ok(found_links) => {
                        for (link, depth) in found_links {
                            if depth <= self.max_depth && !self.visited_urls.contains(link.as_str()) {
                                queue.push_back((link, depth));
                            }
                        }
                    }
                    Err(e) => eprintln!("Crawl error: {e}"),
                }
            }
        }
        Ok(())
    }

    async fn process_url(&self, url: Url, depth: usize) -> Result<Vec<(Url, usize)>> {
        let url_str = url.to_string();
        if self.should_skip(&url_str) || !self.visited_urls.insert(url_str.clone()) {
            return Ok(vec![]);
        }

        let _permit = self.concurrency_limit.acquire().await?;
        println!("Depth {depth}: Crawling {url_str}");
        
        let response = self.client.get(url.clone()).send().await?;
        if !response.status().is_success() { return Ok(vec![]); }

        let (links, body_text) = self.parse_stream(response).await?;

        let _ = self.index_tx.send(CrawledData { url: url_str, body: body_text });

        let next_depth = depth + 1;
        let normalized = links.into_iter()
            .filter_map(|l| url.join(&l).ok())
            .map(|mut u| { u.set_fragment(None); u })
            // Check if the host is in our allowed list
            .filter(|u| {
                if let Some(host) = u.host_str() {
                    self.allowed_domains.contains(host)
                } else {
                    false
                }
            })
            .map(|u| (u, next_depth))
            .collect();

        Ok(normalized)
    }

    async fn parse_stream(&self, mut response: reqwest::Response) -> Result<(Vec<String>, String)> {
        let (tx, mut rx) = mpsc::unbounded_channel::<bytes::Bytes>();
        let parse_handle = tokio::task::spawn_blocking(move || -> Result<(Vec<String>, String)> {
            let mut links = Vec::new();
            let mut texts = Vec::new();
            let mut rewriter = HtmlRewriter::new(
                Settings {
                    element_content_handlers: vec![
                        element!("a[href]", |el| {
                            if let Some(href) = el.get_attribute("href") { links.push(href); }
                            Ok(())
                        }),
                        text!("body", |chunk| {
                            texts.push(chunk.as_str().to_string());
                            Ok(())
                        }),
                    ],
                    ..Settings::default()
                },
                |_: &[u8]| {},
            );
            while let Some(chunk) = rx.blocking_recv() { rewriter.write(&chunk)?; }
            rewriter.end()?;
            Ok((links, texts.join(" ")))
        });

        while let Some(chunk) = response.chunk().await? { let _ = tx.send(chunk); }
        drop(tx);
        parse_handle.await?
    }

    fn should_skip(&self, url: &str) -> bool {
        self.excluded_prefixes.iter().any(|p| url.starts_with(p))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 1. Read domains from file
    let content = read_to_string(&args.input_file)
        .with_context(|| format!("Failed to read input file: {}", args.input_file))?;
    
    let mut start_urls = Vec::new();
    let mut allowed_domains = HashSet::new();

    for line in content.lines() {
        let domain = line.trim();
        if domain.is_empty() { continue; }

        // Clean domain and create start URL
        let clean_domain = domain.trim_start_matches("http://").trim_start_matches("https://");
        let start_url = match Url::parse(&format!("https://{clean_domain}")) {
            Ok(url) => url,
            Err(_) => {
            eprintln!("Skipping invalid domain in file: {domain}");
            continue;
            }
        };
        
        if let Some(host) = start_url.host_str() {
            allowed_domains.insert(host.to_string());
            start_urls.push(start_url);
        }
    }

    if start_urls.is_empty() {
        anyhow::bail!("No valid domains found in input file.");
    }

    // 2. Tantivy Setup
    let mut schema_builder = Schema::builder();
    let url_field = schema_builder.add_text_field("url", STORED);
    let body_field = schema_builder.add_text_field("body", TEXT | STORED);
    let schema = schema_builder.build();
    
    //std::fs::create_dir_all("search_db")?;
    //let index = Index::create(tantivy::directory::MmapDirectory::open("search_db")?, schema, IndexSettings::default())?;
    // keep old index if exists
    let index = if std::path::Path::new("search_db").exists() {
        Index::open_in_dir("search_db")?
    } else {
        std::fs::create_dir_all("search_db")?;
        Index::create(tantivy::directory::MmapDirectory::open("search_db")?, schema, IndexSettings::default())?
    };

    let mut writer = index.writer(1000_000_000)?;

    // 3. Indexer Task
    let (index_tx, mut index_rx) = mpsc::unbounded_channel::<CrawledData>();
    let indexer_handle = tokio::spawn(async move {
        let mut count = 0;
        while let Some(data) = index_rx.recv().await {
            let _ = writer.add_document(doc!(url_field => data.url, body_field => data.body));
            count += 1;
        }
        println!("Indexer received shutdown signal. Committing {count} docs...");
        writer.commit().expect("Failed to commit index");
    });

    // 4. Run Spider
    {
        let spider = Arc::new(Spider {
            client: Client::builder().user_agent("RustSpider/1.0").build()?,
            index_tx,
            visited_urls: DashSet::new(),
            allowed_domains,
            excluded_prefixes: args.exclude,
            max_depth: args.max_depth,
            concurrency_limit: Arc::new(Semaphore::new(args.concurrency)),
        });

        spider.run(start_urls).await?;
        println!("Crawl loop finished.");
    } 

    // 5. Wait for Indexer
    indexer_handle.await.context("Indexer task panicked")?;
    
    // 6. Verify
    let reader = index.reader()?;
    println!("Verified docs in index: {}", reader.searcher().num_docs());

    Ok(())
}