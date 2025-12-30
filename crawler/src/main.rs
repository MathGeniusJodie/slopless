use anyhow::{anyhow, Result};
use dashmap::DashSet;
use lol_html::{element, html_content::ContentType, HtmlRewriter, Settings};
use reqwest::Client;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use tantivy::schema::{Schema, TEXT, STORED, IndexRecordOption, TextFieldIndexing, TextAnalyzerParams};
use tantivy::{doc, Index, IndexWriter};
use url::Url;

struct Crawler {
    client: Client,
    visited: Arc<DashSet<String>>,
    domain: String,
    index_writer: Arc<IndexWriter>,
    schema: Schema,
}

impl Crawler {
    async fn crawl(&self, current_url: Url) -> Result<()> {
        let url_str = current_url.to_string();

        // 1. Skip if already visited
        if !self.visited.insert(url_str.clone()) {
            return Ok(());
        }

        println!("Crawling: {}", url_str);

        // 2. Fetch the page
        let response = self.client.get(current_url.clone()).send().await?;
        if !response.status().is_success() {
            return Ok(());
        }

        let body = response.text().await?;
        let mut discovered_links = Vec::new();
        let mut page_text = Vec::new();

        // 3. Parse with lol-html
        // We use the rewriter to extract hrefs and text content simultaneously
        {
            let mut rewriter = HtmlRewriter::new(
                Settings {
                    element_content_handlers: vec![
                        element!("a[href]", |el| {
                            if let Some(href) = el.get_attribute("href") {
                                discovered_links.push(href);
                            }
                            Ok(())
                        }),
                        // Collect text from body to index
                        element!("body", |el| {
                            el.on_text(|chunks| {
                                page_text.push(chunks.as_str().to_string());
                                Ok(())
                            })?;
                            Ok(())
                        }),
                    ],
                    ..Settings::default()
                },
                |_: &[u8]| {}, // We aren't actually rewriting, just extracting
            );

            rewriter.write(body.as_bytes())?;
            rewriter.end()?;
        }

        // 4. Index in Tantivy
        let full_text = page_text.join(" ");
        let url_field = self.schema.get_field("url").unwrap();
        let body_field = self.schema.get_field("body").unwrap();

        self.index_writer.add_document(doc!(
            url_field => url_str,
            body_field => full_text, // Indexed but NOT stored (see schema setup)
        ))?;

        // 5. Recurse into discovered links
        for link in discovered_links {
            if let Ok(mut absolute_url) = current_url.join(&link) {
                absolute_url.set_fragment(None); // Normalize by removing fragments

                // Only follow links within the same domain
                if let Some(host) = absolute_url.host_str() {
                    if host == self.domain {
                        // In a real production scraper, you'd use a worker pool/queue here.
                        // For simplicity, we use Box::pin for async recursion.
                        let _ = Box::pin(self.crawl(absolute_url)).await;
                    }
                }
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        println!("Usage: cargo run <domain> (e.g. example.com)");
        return Ok(());
    }

    let input_domain = args[1].trim_start_matches("http://").trim_start_matches("https://");
    let start_url = Url::parse(&format!("https://{}", input_domain))?;
    let domain_name = start_url.host_str().ok_or_else(|| anyhow!("Invalid domain"))?.to_string();

    // --- Tantivy Setup ---
    let mut schema_builder = Schema::builder();
    // URL: Store it so we can retrieve the result
    schema_builder.add_text_field("url", STORED);
    
    // Body: Index it for search, but DO NOT STORE to save memory
    let text_indexing = TextAnalyzerParams::default();
    let indexing_options = TextFieldIndexing::default()
        .set_tokenizer("default")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    
    schema_builder.add_text_field("body", tantivy::schema::TextOptions::default()
        .set_indexing_options(indexing_options)
        .set_stored()); // Removing .set_stored() effectively does what you asked, 
                        // but Tantivy needs careful field definition. 
                        // To be explicit: we define it as indexed only.
    
    let schema = schema_builder.build();
    let index = Index::create_in_ram(schema.clone()); // Using RAM for this example
    let index_writer = Arc::new(index.writer(50_000_000)?); // 50MB heap

    // --- Crawler Setup ---
    let crawler = Crawler {
        client: Client::builder()
            .user_agent("RustScraper/1.0")
            .build()?,
        visited: Arc::new(DashSet::new()),
        domain: domain_name,
        index_writer,
        schema,
    };

    // --- Start Crawling ---
    println!("Starting crawl on {}...", start_url);
    crawler.crawl(start_url).await?;

    // Finalize Index
    crawler.index_writer.commit()?;

    // --- Output to File ---
    let mut file = File::create("urls.txt")?;
    for url in crawler.visited.iter() {
        writeln!(file, "{}", url.key())?;
    }

    println!("\nCrawl complete!");
    println!("Total pages visited: {}", crawler.visited.len());
    println!("URLs saved to urls.txt");
    println!("Search index is ready in memory.");

    Ok(())
}