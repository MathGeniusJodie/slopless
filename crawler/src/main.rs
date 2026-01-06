use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use spider::website::Website;
use std::fs::read_to_string;
use std::sync::{Arc, Mutex};
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED, TEXT,
};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index};
use unicode_normalization::UnicodeNormalization;
use url::Url;

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
    #[arg(short = 'c', long = "concurrency", default_value_t = 50)]
    max_concurrent_domains: usize,
    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose_logging: bool,
}

/// Manages Tantivy search index
struct CrawlDb {
    index: Index,
    index_writer: tantivy::IndexWriter,
    url_field: tantivy::schema::Field,
    title_field: tantivy::schema::Field,
    body_field: tantivy::schema::Field,

    // Metrics
    pages_crawled: usize,
    pages_failed: usize,
}

impl CrawlDb {
    fn new() -> Result<Self> {
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

        Ok(Self {
            index,
            index_writer,
            url_field,
            title_field,
            body_field,
            pages_crawled: 0,
            pages_failed: 0,
        })
    }

    fn index_page(&mut self, url: &str, title: &str, content: &str) -> Result<()> {
        let doc = doc!(
            self.url_field => url,
            self.title_field => title,
            self.body_field => content
        );
        self.index_writer.add_document(doc)?;
        self.pages_crawled += 1;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.index_writer.commit()?;
        Ok(())
    }

    fn print_status(&self) {
        println!(
            "Pages indexed: {}, Failed: {}",
            self.pages_crawled, self.pages_failed
        );
    }

    /// Get all indexed URLs for a domain
    fn get_urls_for_domain(&self, domain: &str) -> Vec<String> {
        use tantivy::collector::TopDocs;
        use tantivy::query::RegexQuery;
        use tantivy::schema::Value;

        // Match URLs starting with https://domain/
        let pattern = format!("https://{}/.*", regex::escape(domain));
        let Ok(query) = RegexQuery::from_pattern(&pattern, self.url_field) else {
            return Vec::new();
        };

        let Ok(reader) = self.index.reader() else {
            return Vec::new();
        };
        let searcher = reader.searcher();

        // Get up to 100k URLs for this domain
        let Ok(top_docs) = searcher.search(&query, &TopDocs::with_limit(100_000)) else {
            return Vec::new();
        };

        top_docs
            .into_iter()
            .filter_map(|(_, doc_address)| {
                let doc: tantivy::TantivyDocument = searcher.doc(doc_address).ok()?;
                doc.get_first(self.url_field)?.as_str().map(String::from)
            })
            .collect()
    }
}

/// Load domain list from file
fn load_domains(file_path: &str) -> Result<Vec<String>> {
    let file_content = read_to_string(file_path)
        .with_context(|| format!("Failed to read input file: {}", file_path))?;

    let mut domains = Vec::new();

    for line in file_content.lines() {
        let domain = line.trim();
        if domain.is_empty() {
            continue;
        }

        // Clean domain (remove http:// or https:// prefix if present)
        let clean_domain = domain
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_end_matches('/');

        // Validate by parsing as URL
        let url_str = format!("https://{}/", clean_domain);
        if Url::parse(&url_str).is_ok() {
            domains.push(clean_domain.to_string());
        } else {
            eprintln!("Skipping invalid domain: {}", domain);
        }
    }

    Ok(domains)
}

/// Crawl a single domain using spider
async fn crawl_domain(
    domain: String,
    db: Arc<Mutex<CrawlDb>>,
    max_depth: usize,
    excluded_prefixes: Arc<Vec<String>>,
    verbose: bool,
) {
    let url = format!("https://{}/", domain);

    // Get already-indexed URLs for this domain to skip
    let existing_urls: Vec<String> = {
        let db = db.lock().unwrap();
        db.get_urls_for_domain(&domain)
    };
    if verbose && !existing_urls.is_empty() {
        println!("{}: skipping {} already-indexed URLs", domain, existing_urls.len());
    }

    let mut website = Website::new(&url);

    // Configure spider for polite crawling: 1 request at a time per domain
    website.configuration.respect_robots_txt = true;
    website.configuration.subdomains = false;
    website.configuration.depth = max_depth;
    website.with_limit(1);

    // Build blacklist: existing URLs + excluded prefixes
    let blacklist: Vec<String> = existing_urls
        .into_iter()
        .chain(excluded_prefixes.iter().cloned())
        .collect();
    if !blacklist.is_empty() {
        website.with_blacklist_url(Some(blacklist.into_iter().map(Into::into).collect()));
    }

    // Subscribe to page events
    let mut rx = match website.subscribe(256) {
        Some(rx) => rx,
        None => {
            if verbose {
                eprintln!("Failed to subscribe to {} crawl events", domain);
            }
            return;
        }
    };

    // Spawn task to process received pages
    let processor = tokio::spawn({
        let db = db.clone();
        async move {
            while let Ok(page) = rx.recv().await {
                let page_url = page.get_url();

                // Get HTML content
                let html = page.get_html();
                if html.is_empty() {
                    continue;
                }

                // Extract readable content
                let (readable_text, page_title) = match find_main_content(html.as_bytes()) {
                    Ok(result) => result,
                    Err(e) => {
                        if verbose {
                            eprintln!("Error extracting content from {}: {:?}", page_url, e);
                        }
                        db.lock().unwrap().pages_failed += 1;
                        continue;
                    }
                };

                // Normalize and index
                let content: String = readable_text.nfkc().collect();
                let title: String = page_title.nfkc().collect();

                let mut db = db.lock().unwrap();
                if let Err(e) = db.index_page(page_url, &title, &content) {
                    if verbose {
                        eprintln!("Error indexing {}: {:?}", page_url, e);
                    }
                    db.pages_failed += 1;
                }
            }
        }
    });

    // Start the crawl
    website.crawl().await;

    // Wait for processor to finish
    drop(website);
    let _ = processor.await;

    if verbose {
        println!("Finished crawling {}", domain);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {
    let cli_args = Args::parse();

    let db = Arc::new(Mutex::new(CrawlDb::new()?));

    let domains = load_domains(&cli_args.input_file)?;
    println!("Loaded {} domains to crawl", domains.len());

    let excluded_prefixes = Arc::new(cli_args.excluded_url_prefixes);
    let max_depth = cli_args.max_crawl_depth;
    let verbose = cli_args.verbose_logging;
    let max_concurrent = cli_args.max_concurrent_domains;

    // Spawn background task for periodic status updates and commits
    let db_status = db.clone();
    let status_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let mut db = db_status.lock().unwrap();
            db.print_status();
            if let Err(e) = db.commit() {
                eprintln!("Warning: Failed to commit index: {:?}", e);
            } else if let Ok(reader) = db.index.reader() {
                println!("  Total indexed documents: {}", reader.searcher().num_docs());
            }
        }
    });

    // Use for_each_concurrent to crawl domains with max concurrency
    futures::stream::iter(domains)
        .for_each_concurrent(max_concurrent, |domain| {
            let db = db.clone();
            let excluded = excluded_prefixes.clone();
            async move {
                crawl_domain(domain, db, max_depth, excluded, verbose).await;
            }
        })
        .await;

    // Cancel the status task
    status_task.abort();

    // Final commit
    {
        let mut db = db.lock().unwrap();
        db.commit()?;
        println!(
            "Crawl finished. Docs in index: {}",
            db.index.reader()?.searcher().num_docs()
        );
    }

    Ok(())
}
