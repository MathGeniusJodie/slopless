use anyhow::{anyhow, Result};
use clap::Parser;
use dashmap::DashSet;
use lol_html::{element, text, HtmlRewriter, Settings};
use reqwest::Client;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tantivy::schema::{STORED, Schema, TEXT};
use tantivy::{doc, Index, IndexSettings, IndexWriter};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use url::Url;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(help = "Domain to crawl (e.g., example.com)")]
    domain: String,

    #[arg(short = 'x', long = "exclude")]
    exclude: Vec<String>,

    #[arg(short = 'd', long = "depth", default_value_t = 5)]
    max_depth: usize,

    #[arg(short = 'c', long = "concurrency", default_value_t = 50)]
    concurrency: usize,
}

struct Crawler {
    client: Client,
    visited: Arc<DashSet<String>>,
    domain: String,
    index_writer: Arc<Mutex<IndexWriter>>,
    schema: Schema,
    concurrency_limit: Arc<Semaphore>,
    excluded_prefixes: Vec<String>,
    max_depth: usize,
}

impl Crawler {
    /// The main loop: manages the queue and orchestrates tasks
    pub async fn run(self: Arc<Self>, start_url: Url) -> Result<()> {
        let mut queue = VecDeque::new();
        let mut join_set = JoinSet::new();

        if self.is_excluded(&start_url.to_string()) {
            return Err(anyhow!("Start URL is in the exclusion list"));
        }

        // BFS initialization
        queue.push_back((start_url, 0));

        loop {
            // 1. Fill the JoinSet with tasks from the queue up to the concurrency limit
            while !queue.is_empty() && join_set.len() < self.concurrency_limit.available_permits() {
                if let Some((url, depth)) = queue.pop_front() {
                    let crawler_clone = Arc::clone(&self);
                    join_set.spawn(async move {
                        crawler_clone.crawl(url, depth).await
                    });
                }
            }

            // 2. If no tasks are running and the queue is empty, we are finished
            if join_set.is_empty() {
                break;
            }

            // 3. Wait for the next task to complete and process discovered links
            let res = match join_set.join_next().await {
                Some(r) => r,
                None => continue,
            };

            let new_links = match res {
                Ok(Ok(links)) => links,
                Ok(Err(e)) => {
                    eprintln!("Crawl error: {}", e);
                    continue;
                }
                Err(e) => {
                    eprintln!("Task join error: {}", e);
                    continue;
                }
            };

            for (link, depth) in new_links {
                if depth > self.max_depth || self.visited.contains(link.as_str()) {
                    continue;
                }
                queue.push_back((link, depth));
            }
        }

        Ok(())
    }

    fn is_excluded(&self, url: &str) -> bool {
        self.excluded_prefixes.iter().any(|prefix| url.starts_with(prefix))
    }

    async fn crawl(self: Arc<Self>, current_url: Url, current_depth: usize) -> Result<Vec<(Url, usize)>> {
        let url_str = current_url.to_string();

        // Double-check visited/excluded inside the task for thread safety
        if self.is_excluded(&url_str) || !self.visited.insert(url_str.clone()) {
            return Ok(vec![]);
        }

        // Rate limiting / Concurrency control
        let _permit = self.concurrency_limit.acquire().await?;
        println!("Depth {}: Crawling {}", current_depth, url_str);

        let mut response = self.client.get(current_url.clone()).send().await?;
        if !response.status().is_success() {
            return Ok(vec![]);
        }

        // Stream body to the parser
        let (tx, mut rx) = mpsc::unbounded_channel::<bytes::Bytes>();
        let parse_handle = tokio::task::spawn_blocking(move || -> Result<(Vec<String>, String)> {
            let mut discovered_links = Vec::new();
            let mut page_text = Vec::new();

            let mut rewriter = HtmlRewriter::new(
                Settings {
                    element_content_handlers: vec![
                        element!("a[href]", |el| {
                            if let Some(href) = el.get_attribute("href") {
                                discovered_links.push(href);
                            }
                            Ok(())
                        }),
                        text!("body", |chunk| {
                            page_text.push(chunk.as_str().to_string());
                            Ok(())
                        }),
                    ],
                    ..Settings::default()
                },
                |_: &[u8]| {},
            );

            while let Some(chunk) = rx.blocking_recv() {
                rewriter.write(&chunk)?;
            }
            rewriter.end()?;

            Ok((discovered_links, page_text.join(" ")))
        });

        while let Some(chunk) = response.chunk().await? {
            let _ = tx.send(chunk);
        }
        drop(tx);

        let (raw_links, full_text) = parse_handle.await??;

        // --- INDEXING ---
        let url_field = self.schema.get_field("url").unwrap();
        let body_field = self.schema.get_field("body").unwrap();
        {
            let writer = self.index_writer.lock().unwrap();
            writer.add_document(doc!(
                url_field => url_str,
                body_field => full_text,
            ))?;
        }

        // --- LINK FILTERING ---
        let mut next_urls = Vec::new();
        let next_depth = current_depth + 1;

        if next_depth > self.max_depth {
            return Ok(next_urls);
        }

        for link in raw_links {
            let absolute_url = match current_url.join(&link) {
            Ok(mut url) => {
                url.set_fragment(None);
                url
            }
            Err(_) => continue,
            };

            let abs_url_str = absolute_url.as_str();

            if absolute_url.host_str() != Some(&self.domain) {
            continue;
            }
            if self.is_excluded(abs_url_str) {
            continue;
            }

            next_urls.push((absolute_url, next_depth));
        }

        Ok(next_urls)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let input_domain = args.domain.trim_start_matches("http://").trim_start_matches("https://");
    let start_url = Url::parse(&format!("https://{}", input_domain))?;
    let domain_name = start_url.host_str().ok_or_else(|| anyhow!("Invalid domain"))?.to_string();

    let mut schema_builder = Schema::builder();
    schema_builder.add_text_field("url", STORED);
    schema_builder.add_text_field("body", TEXT | STORED);
    let schema = schema_builder.build();
    
    // Ensure the directory exists
    std::fs::create_dir_all("database")?;
    let index = Index::create(
        tantivy::directory::MmapDirectory::open("database")?,
        schema.clone(),
        IndexSettings::default(),
    )?;
    let index_writer = Arc::new(Mutex::new(index.writer(100_000_000)?));

    let crawler = Arc::new(Crawler {
        client: Client::builder()
            .user_agent("RustScraper/1.0")
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
        visited: Arc::new(DashSet::new()),
        domain: domain_name,
        index_writer: index_writer.clone(),
        schema,
        concurrency_limit: Arc::new(Semaphore::new(args.concurrency)),
        excluded_prefixes: args.exclude,
        max_depth: args.max_depth,
    });

    crawler.run(start_url).await?;
    
    // Explicitly commit the index
    index_writer.lock().unwrap().commit()?;

    let reader = index.reader()?;
    let searcher = reader.searcher();
    println!("Finished! Index contains {} pages.", searcher.num_docs());

    Ok(())
}