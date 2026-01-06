use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use spider::website::Website;
use std::fs::read_to_string;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED, STRING, TEXT,
};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, Term};
use tokio::sync::{mpsc, oneshot};
use unicode_normalization::UnicodeNormalization;
use url::Url;

mod lol_readability;

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

/// Messages sent to the DB thread
enum DbMessage {
    ProcessPage {
        url: String,
        domain: String,
        html: String,
    },
    Commit {
        reply: oneshot::Sender<Result<u64>>,
    },
    Shutdown,
}

/// Handle to communicate with the DB thread
#[derive(Clone)]
struct DbHandle {
    tx: mpsc::Sender<DbMessage>,
}

impl DbHandle {
    async fn process_page(&self, url: String, domain: String, html: String) {
        self.tx
            .send(DbMessage::ProcessPage { url, domain, html })
            .await
            .ok();
    }

    async fn commit(&self) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(DbMessage::Commit { reply }).await.ok();
        rx.await?
    }

    async fn shutdown(&self) {
        self.tx.send(DbMessage::Shutdown).await.ok();
    }
}

/// Manages Tantivy search index (runs in dedicated thread)
struct CrawlDb {
    index: Index,
    index_writer: tantivy::IndexWriter,
    url_field: tantivy::schema::Field,
    domain_field: tantivy::schema::Field,
    title_field: tantivy::schema::Field,
    body_field: tantivy::schema::Field,
}

impl CrawlDb {
    fn new() -> Result<Self> {
        let search_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .filter(tantivy::tokenizer::Stemmer::new(
                tantivy::tokenizer::Language::English,
            ))
            .build();

        let (index, url_field, domain_field, title_field, body_field) =
            if std::path::Path::new("search_db").exists() {
                let index = Index::open_in_dir("search_db")?;
                let schema = index.schema();

                let url_field = schema
                    .get_field("url")
                    .context("Existing index missing 'url' field")?;
                let domain_field = schema
                    .get_field("domain")
                    .context("Existing index missing 'domain' field - delete search_db/ and re-crawl")?;
                let title_field = schema
                    .get_field("title")
                    .context("Existing index missing 'title' field")?;
                let body_field = schema
                    .get_field("body")
                    .context("Existing index missing 'body' field")?;

                (index, url_field, domain_field, title_field, body_field)
            } else {
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

                (index, url_field, domain_field, title_field, body_field)
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

    /// Returns: true = indexed, false = failed
    fn process_page(&mut self, url: &str, domain: &str, html: &str) -> bool {
        // Parse HTML to extract readable content (returns canonical URL if present)
        let (readable_text, page_title, resolved_url) =
            match lol_readability::find_main_content(html.as_bytes(), url) {
                Ok(result) => result,
                Err(_) => return false,
            };

        // Normalize unicode
        let content: String = readable_text.nfkc().collect();
        let title: String = page_title.nfkc().collect();

        // Delete any existing document with this URL (idiomatic upsert pattern)
        let url_term = Term::from_field_text(self.url_field, &resolved_url);
        self.index_writer.delete_term(url_term);

        // Index the document
        let mut doc = tantivy::TantivyDocument::new();
        doc.add_text(self.url_field, &resolved_url);
        doc.add_text(self.domain_field, domain);
        doc.add_text(self.title_field, &title);
        doc.add_text(self.body_field, &content);

        self.index_writer.add_document(doc).is_ok()
    }

    fn commit(&mut self) -> Result<u64> {
        self.index_writer.commit()?;
        let reader = self.index.reader()?;
        Ok(reader.searcher().num_docs())
    }

    fn run(mut self, mut rx: mpsc::Receiver<DbMessage>, metrics: Arc<CrawlMetrics>) {
        while let Some(msg) = rx.blocking_recv() {
            match msg {
                DbMessage::ProcessPage { url, domain, html } => {
                    if self.process_page(&url, &domain, &html) {
                        metrics.pages_crawled.fetch_add(1, Ordering::Relaxed);
                    } else {
                        metrics.pages_failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                DbMessage::Commit { reply } => {
                    reply.send(self.commit()).ok();
                }
                DbMessage::Shutdown => break,
            }
        }
    }
}

/// Spawn the DB thread and return a handle for communication
fn spawn_db_thread(metrics: Arc<CrawlMetrics>) -> Result<(DbHandle, std::thread::JoinHandle<()>)> {
    let db = CrawlDb::new()?;
    let (tx, rx) = mpsc::channel(1024);
    let handle = std::thread::spawn(move || db.run(rx, metrics));
    Ok((DbHandle { tx }, handle))
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

        let clean_domain = domain
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_end_matches('/');

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
    db: DbHandle,
    max_depth: usize,
    excluded_prefixes: Arc<Vec<String>>,
    verbose: bool,
) {
    let url = format!("https://{}/", domain);
    let mut website = Website::new(&url);

    website.configuration.respect_robots_txt = true;
    website.configuration.subdomains = false;
    website.configuration.depth = max_depth;
    website.with_limit(1);
    website.with_normalize(true);

    if !excluded_prefixes.is_empty() {
        website.with_blacklist_url(Some(
            excluded_prefixes.iter().cloned().map(Into::into).collect(),
        ));
    }

    let mut rx = match website.subscribe(256) {
        Some(rx) => rx,
        None => {
            if verbose {
                eprintln!("Failed to subscribe to {} crawl events", domain);
            }
            return;
        }
    };

    // Spawn crawl so website is dropped when done, closing the channel
    let crawl_handle = tokio::spawn(async move {
        website.crawl().await;
    });

    // Process pages as they stream in
    while let Ok(page) = rx.recv().await {
        let html = page.get_html();
        if !html.is_empty() {
            db.process_page(page.get_url().to_string(), domain.clone(), html)
                .await;
        }
    }

    // Ensure crawl task completed
    let _ = crawl_handle.await;

    if verbose {
        println!("Finished crawling {}", domain);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {
    let cli_args = Args::parse();

    let metrics = Arc::new(CrawlMetrics {
        pages_crawled: AtomicUsize::new(0),
        pages_failed: AtomicUsize::new(0),
    });
    let (db, db_thread) = spawn_db_thread(metrics.clone())?;

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

            let crawled = metrics_for_status.pages_crawled.load(Ordering::Relaxed);
            let failed = metrics_for_status.pages_failed.load(Ordering::Relaxed);
            println!("Pages indexed: {}, Failed: {}", crawled, failed);

            match db_for_status.commit().await {
                Ok(doc_count) => {
                    println!("  Total indexed documents: {}", doc_count);
                }
                Err(e) => {
                    eprintln!("Warning: Failed to commit index: {:?}", e);
                }
            }
        }
    });

    futures::stream::iter(domains)
        .for_each_concurrent(max_concurrent, |domain| {
            let db = db.clone();
            let excluded = excluded_prefixes.clone();
            async move {
                crawl_domain(domain, db, max_depth, excluded, verbose).await;
            }
        })
        .await;

    status_task.abort();

    // Final commit
    let crawled = metrics.pages_crawled.load(Ordering::Relaxed);
    let failed = metrics.pages_failed.load(Ordering::Relaxed);
    match db.commit().await {
        Ok(doc_count) => {
            println!(
                "Crawl finished. Pages indexed: {}, Failed: {}, Total docs: {}",
                crawled, failed, doc_count
            );
        }
        Err(e) => {
            eprintln!("Final commit failed: {:?}", e);
        }
    }

    db.shutdown().await;
    db_thread.join().ok();

    Ok(())
}
