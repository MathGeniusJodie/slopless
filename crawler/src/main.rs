use anyhow::{anyhow, Result};
use dashmap::DashSet;
use lol_html::{element, text, HtmlRewriter, Settings};
use reqwest::Client;
use std::sync::{Arc, Mutex};
use tantivy::schema::{IndexRecordOption, STORED, Schema, TEXT, TextFieldIndexing};
use tantivy::{doc, Index, IndexWriter};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use url::Url;

struct Crawler {
    client: Client,
    visited: Arc<DashSet<String>>,
    domain: String,
    index_writer: Arc<Mutex<IndexWriter>>,
    schema: Schema,
    concurrency_limit: Arc<Semaphore>,
}

impl Crawler {
    pub async fn run(self: Arc<Self>, start_url: Url) -> Result<()> {
        let mut join_set = JoinSet::new();
        join_set.spawn(Arc::clone(&self).crawl(start_url));

        while let Some(res) = join_set.join_next().await {
            match res {
                Ok(Ok(new_links)) => {
                    for link in new_links {
                        join_set.spawn(Arc::clone(&self).crawl(link));
                    }
                }
                Ok(Err(e)) => eprintln!("Crawl error: {}", e),
                Err(e) => eprintln!("Join error: {}", e),
            }
        }
        Ok(())
    }

    async fn crawl(self: Arc<Self>, current_url: Url) -> Result<Vec<Url>> {
        let url_str = current_url.to_string();
        if !self.visited.insert(url_str.clone()) {
            return Ok(vec![]);
        }

        let _permit = self.concurrency_limit.acquire().await?;
        println!("Crawling: {}", url_str);

        let mut response = self.client.get(current_url.clone()).send().await?;
        if !response.status().is_success() {
            return Ok(vec![]);
        }

        // --- THE STREAMING BRIDGE ---
        // Create a channel to pipe bytes from the Async network to the Sync parser
        let (tx, mut rx) = mpsc::unbounded_channel::<bytes::Bytes>();

        // Spawn a blocking task for lol-html (it's not Send, so it must stay on one thread)
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

            // Synchronously pull from the channel until it's closed
            while let Some(chunk) = rx.blocking_recv() {
                rewriter.write(&chunk)?;
            }
            rewriter.end()?;

            Ok((discovered_links, page_text.join(" ")))
        });

        // Loop to pull from network and send to the parser task
        while let Some(chunk) = response.chunk().await? {
            let _ = tx.send(chunk.into());
        }
        drop(tx); // Close channel so the parser knows we're done

        // Wait for the parsing thread to finish
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
        for link in raw_links {
            if let Ok(mut absolute_url) = current_url.join(&link) {
                absolute_url.set_fragment(None);
                if absolute_url.host_str() == Some(&self.domain) {
                    next_urls.push(absolute_url);
                }
            }
        }

        Ok(next_urls)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // (Same setup as before, but ensure Crawler::run is called)
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { return Err(anyhow!("Usage: crawler <domain>")); }
    
    let input_domain = args[1].trim_start_matches("http://").trim_start_matches("https://");
    let start_url = Url::parse(&format!("https://{}", input_domain))?;
    let domain_name = start_url.host_str().unwrap().to_string();

    let mut schema_builder = Schema::builder();
    schema_builder.add_text_field("url", STORED);
    schema_builder.add_text_field("body", TEXT | STORED);
    let schema = schema_builder.build();
    let index = Index::create_in_ram(schema.clone());
    let index_writer = Arc::new(Mutex::new(index.writer(50_000_000)?));

    let crawler = Arc::new(Crawler {
        client: Client::builder().user_agent("RustScraper/1.0").build()?,
        visited: Arc::new(DashSet::new()),
        domain: domain_name,
        index_writer: index_writer.clone(),
        schema,
        concurrency_limit: Arc::new(Semaphore::new(20)),
    });

    crawler.run(start_url).await?;
    index_writer.lock().unwrap().commit()?;

    // Create a reader and get the number of documents
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let num_docs = searcher.num_docs();

    println!("Finished! Visited {} pages.", num_docs);
    Ok(())
}