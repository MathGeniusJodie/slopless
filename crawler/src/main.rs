use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use spider::website::Website;
use std::fs::read_to_string;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED, STRING, TEXT,
};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, Term};
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

/// Metrics tracked with atomic counters (no lock needed)
struct CrawlMetrics {
    pages_crawled: AtomicUsize,
    pages_failed: AtomicUsize,
}

/// Manages Tantivy search index
struct CrawlDb {
    index: Index,
    index_writer: tantivy::IndexWriter,
    url_field: tantivy::schema::Field,
    domain_field: Option<tantivy::schema::Field>, // Optional for backwards compatibility
    title_field: tantivy::schema::Field,
    body_field: tantivy::schema::Field,
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

        // Open existing index or create new one
        let (index, url_field, domain_field, title_field, body_field) =
            if std::path::Path::new("search_db").exists() {
                // Open existing index and use its schema
                let index = Index::open_in_dir("search_db")?;
                let schema = index.schema();

                let url_field = schema
                    .get_field("url")
                    .context("Existing index missing 'url' field")?;
                let title_field = schema
                    .get_field("title")
                    .context("Existing index missing 'title' field")?;
                let body_field = schema
                    .get_field("body")
                    .context("Existing index missing 'body' field")?;
                // domain field may not exist in old indexes
                let domain_field = schema.get_field("domain").ok();

                if domain_field.is_none() {
                    eprintln!(
                        "Warning: Existing index lacks 'domain' field. \
                         URL lookups will use slower regex queries. \
                         Delete search_db/ and re-crawl for better performance."
                    );
                }

                (index, url_field, domain_field, title_field, body_field)
            } else {
                // Build new schema with domain field
                let body_field_indexing = TextFieldIndexing::default()
                    .set_tokenizer("norm_tokenizer")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions);
                let body_field_options =
                    TextOptions::default().set_indexing_options(body_field_indexing);

                let mut schema_builder = Schema::builder();
                let url_field = schema_builder.add_text_field("url", TEXT | STORED | FAST);
                let domain_field = schema_builder.add_text_field("domain", STRING | FAST);
                let title_field = schema_builder.add_text_field("title", TEXT | STORED);
                let body_field = schema_builder.add_text_field("body", body_field_options);
                let schema = schema_builder.build();

                std::fs::create_dir_all("search_db")?;
                let index = Index::create(
                    tantivy::directory::MmapDirectory::open("search_db")?,
                    schema,
                    tantivy::IndexSettings::default(),
                )?;

                (index, url_field, Some(domain_field), title_field, body_field)
            };

        index
            .tokenizers()
            .register("norm_tokenizer", search_tokenizer);

        let index_writer = index.writer(50_000_000)?;

        Ok(Self {
            index,
            index_writer,
            url_field,
            domain_field,
            title_field,
            body_field,
        })
    }

    fn index_page(&mut self, url: &str, domain: &str, title: &str, content: &str) -> Result<()> {
        let mut doc = tantivy::TantivyDocument::new();
        doc.add_text(self.url_field, url);
        if let Some(domain_field) = self.domain_field {
            doc.add_text(domain_field, domain);
        }
        doc.add_text(self.title_field, title);
        doc.add_text(self.body_field, content);
        self.index_writer.add_document(doc)?;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.index_writer.commit()?;
        Ok(())
    }

    /// Get all indexed URLs for a domain
    fn get_urls_for_domain(&self, domain: &str) -> Vec<String> {
        use tantivy::collector::TopDocs;
        use tantivy::schema::Value;

        let Ok(reader) = self.index.reader() else {
            return Vec::new();
        };
        let searcher = reader.searcher();

        // Use efficient term query if domain field exists, otherwise fall back to regex
        let top_docs = if let Some(domain_field) = self.domain_field {
            use tantivy::query::TermQuery;
            let term = Term::from_field_text(domain_field, domain);
            let query = TermQuery::new(term, IndexRecordOption::Basic);
            searcher.search(&query, &TopDocs::with_limit(100_000))
        } else {
            use tantivy::query::RegexQuery;
            let pattern = format!("https://{}/.*", regex::escape(domain));
            let Ok(query) = RegexQuery::from_pattern(&pattern, self.url_field) else {
                return Vec::new();
            };
            searcher.search(&query, &TopDocs::with_limit(100_000))
        };

        let Ok(top_docs) = top_docs else {
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
    metrics: Arc<CrawlMetrics>,
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
        let metrics = metrics.clone();
        let domain = domain.clone();
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
                        metrics.pages_failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };

                // Normalize and index
                let content: String = readable_text.nfkc().collect();
                let title: String = page_title.nfkc().collect();

                {
                    let mut db = db.lock().unwrap();
                    if let Err(e) = db.index_page(page_url, &domain, &title, &content) {
                        if verbose {
                            eprintln!("Error indexing {}: {:?}", page_url, e);
                        }
                        metrics.pages_failed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        metrics.pages_crawled.fetch_add(1, Ordering::Relaxed);
                    }
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
    let metrics = Arc::new(CrawlMetrics {
        pages_crawled: AtomicUsize::new(0),
        pages_failed: AtomicUsize::new(0),
    });

    let domains = load_domains(&cli_args.input_file)?;
    println!("Loaded {} domains to crawl", domains.len());

    let excluded_prefixes = Arc::new(cli_args.excluded_url_prefixes);
    let max_depth = cli_args.max_crawl_depth;
    let verbose = cli_args.verbose_logging;
    let max_concurrent = cli_args.max_concurrent_domains;

    // Spawn background task for periodic status updates and commits
    let db_for_status = db.clone();
    let metrics_for_status = metrics.clone();
    let status_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;

            // Read metrics without holding the lock
            let crawled = metrics_for_status.pages_crawled.load(Ordering::Relaxed);
            let failed = metrics_for_status.pages_failed.load(Ordering::Relaxed);
            println!("Pages indexed: {}, Failed: {}", crawled, failed);

            // Acquire lock only for commit operation
            let commit_result = {
                let mut db = db_for_status.lock().unwrap();
                db.commit()
            };

            if let Err(e) = commit_result {
                eprintln!("Warning: Failed to commit index: {:?}", e);
            } else {
                // Read doc count without holding the mutex
                let db = db_for_status.lock().unwrap();
                if let Ok(reader) = db.index.reader() {
                    println!("  Total indexed documents: {}", reader.searcher().num_docs());
                }
            }
        }
    });

    // Use for_each_concurrent to crawl domains with max concurrency
    futures::stream::iter(domains)
        .for_each_concurrent(max_concurrent, |domain| {
            let db = db.clone();
            let metrics = metrics.clone();
            let excluded = excluded_prefixes.clone();
            async move {
                crawl_domain(domain, db, metrics, max_depth, excluded, verbose).await;
            }
        })
        .await;

    // Cancel the status task
    status_task.abort();

    // Final commit
    {
        let mut db = db.lock().unwrap();
        db.commit()?;
        let crawled = metrics.pages_crawled.load(Ordering::Relaxed);
        let failed = metrics.pages_failed.load(Ordering::Relaxed);
        println!(
            "Crawl finished. Pages indexed: {}, Failed: {}, Total docs: {}",
            crawled,
            failed,
            db.index.reader()?.searcher().num_docs()
        );
    }

    Ok(())
}
