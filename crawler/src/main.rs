use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use reqwest::Client;
use spider::compact_str::CompactString;
use spider::website::Website;
use std::fs::read_to_string;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED, TEXT,
};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, Term};
use tokio::sync::mpsc;
use url::Url;

mod lol_readability;

#[cfg(test)]
mod lol_readability_tests;

#[cfg(test)]
mod main_tests;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(help = "Path to text file containing domains (one per line)")]
    input_file: String,
    #[arg(short = 'x', long = "exclude")]
    excluded_url_prefixes: Vec<String>,
    #[arg(short = 'c', long = "concurrency", default_value_t = 100)]
    max_concurrent_domains: usize,
}

/// Manages Tantivy search index
struct CrawlDb {
    index: Index,
    index_writer: tantivy::IndexWriter,
    url_field: tantivy::schema::Field,
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

        let (index, url_field, title_field, body_field) =
            if std::path::Path::new("search_db").exists() {
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

                (index, url_field, title_field, body_field)
            } else {
                let body_field_indexing = TextFieldIndexing::default()
                    .set_tokenizer("norm_tokenizer")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions);
                let body_field_options =
                    TextOptions::default().set_indexing_options(body_field_indexing);

                let mut schema_builder = Schema::builder();
                let url_field = schema_builder.add_text_field("url", TEXT | STORED | FAST);
                let title_field = schema_builder.add_text_field("title", TEXT | STORED);
                let body_field = schema_builder.add_text_field("body", body_field_options);
                let schema = schema_builder.build();

                std::fs::create_dir_all("search_db")?;
                let index = Index::create(
                    tantivy::directory::MmapDirectory::open("search_db")?,
                    schema,
                    tantivy::IndexSettings::default(),
                )?;

                (index, url_field, title_field, body_field)
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
        })
    }

    fn process_page(&mut self, url: &str, title: &str, content: &str) {
        let url_term = Term::from_field_text(self.url_field, url);
        self.index_writer.delete_term(url_term);

        let mut doc = tantivy::TantivyDocument::new();
        doc.add_text(self.url_field, url);
        doc.add_text(self.title_field, title);
        doc.add_text(self.body_field, content);

        if let Err(e) = self.index_writer.add_document(doc) {
            eprintln!("Error adding document {}: {}", url, e);
        }
    }

    fn commit(&mut self) -> Result<u64> {
        self.index_writer.commit()?;
        let reader = self.index.reader()?;
        Ok(reader.searcher().num_docs())
    }
}

/// Page data sent to the writer thread
struct PageData {
    url: String,
    title: String,
    content: String,
}

/// Shared state across all crawl tasks
struct SharedState {
    tx: mpsc::UnboundedSender<PageData>,
    pages_indexed: AtomicUsize,
    pages_failed: AtomicUsize,
}

/// Parse a single domain line, returning the cleaned domain if valid
fn parse_domain_line(line: &str) -> Option<String> {
    let domain = line.trim();

    if domain.is_empty() || domain.starts_with('#') {
        return None;
    }

    let clean_domain = domain
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');

    let url_str = format!("https://{}/", clean_domain);
    Url::parse(&url_str).ok()?;
    Some(clean_domain.to_string())
}

fn parse_domains(content: &str) -> Vec<String> {
    content.lines().filter_map(parse_domain_line).collect()
}

fn load_domains(file_path: &str) -> Result<Vec<String>> {
    let file_content = read_to_string(file_path)
        .with_context(|| format!("Failed to read input file: {}", file_path))?;
    Ok(parse_domains(&file_content))
}

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// Crawl a single domain
async fn crawl_domain(
    domain: String,
    state: Arc<SharedState>,
    excluded_prefixes: Arc<Vec<String>>,
) {
    let url = format!("https://{}/", domain);
    let mut website = Website::new(&url);

    website.configuration.respect_robots_txt = true;
    website.configuration.subdomains = true;
    website.configuration.only_html = true;
    website.configuration.delay = 4000;
    website.with_user_agent(Some(USER_AGENT));

    // Use rustls instead of native-tls/OpenSSL to avoid pthread rwlock contention
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .use_rustls_tls()
        .build()
        .expect("Failed to build HTTP client with rustls");
    website.set_http_client(client);

    website.with_blacklist_url(
        (!excluded_prefixes.is_empty())
            .then(|| excluded_prefixes.iter().map(CompactString::new).collect()),
    );

    let Some(mut rx) = website.subscribe(16384) else {
        eprintln!("Failed to subscribe to {} crawl events", domain);
        return;
    };

    let crawl_handle = tokio::spawn(async move {
        website.crawl().await;
    });

    let mut pages_sent_to_index = 0usize;
    let mut lagged_count = 0usize;

    loop {
        let page = match rx.recv().await {
            Ok(page) => page,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                // Lagged pages are dropped by the broadcast channel - count as failed
                let n = n as usize;
                lagged_count += n;
                state.pages_failed.fetch_add(n, Ordering::Relaxed);
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

        let html = page.get_html();
        if html.is_empty() {
            continue;
        }

        let page_url = page
            .final_redirect_destination
            .as_deref()
            .unwrap_or_else(|| page.get_url());

        match lol_readability::find_main_content(html.as_bytes(), page_url) {
            Ok((content, title, url)) => {
                // Send to writer thread - if channel is closed (writer crashed), count as failed
                if state
                    .tx
                    .send(PageData {
                        url,
                        title,
                        content,
                    })
                    .is_ok()
                {
                    pages_sent_to_index += 1;
                } else {
                    eprintln!("Writer channel closed, page lost: {}", page_url);
                    state.pages_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(_e) => {
                state.pages_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let _ = crawl_handle.await;

    if pages_sent_to_index > 0 || lagged_count > 0 {
        println!(
            "Finished {} Sent: {}, Lagged: {}",
            domain, pages_sent_to_index, lagged_count
        );
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {
    let cli_args = Args::parse();

    let (tx, mut rx) = mpsc::unbounded_channel::<PageData>();

    let state = Arc::new(SharedState {
        tx,
        pages_indexed: AtomicUsize::new(0),
        pages_failed: AtomicUsize::new(0),
    });

    let domains = load_domains(&cli_args.input_file)?;
    println!("Loaded {} domains to crawl", domains.len());

    let excluded_prefixes = Arc::new(cli_args.excluded_url_prefixes);
    let max_concurrent = cli_args.max_concurrent_domains;

    // Dedicated writer thread - owns CrawlDb exclusively, no locks needed.
    // Note: If this thread panics entirely (e.g., CrawlDb::new fails), crawlers will
    // detect the closed channel and count pages as failed. Per-page panics are caught
    // and recovered from below.
    let state_for_writer = state.clone();
    let writer_handle = std::thread::spawn(move || -> Result<u64> {
        let mut db = CrawlDb::new()?;
        let mut last_status = std::time::Instant::now();
        while let Some(page) = rx.blocking_recv() {
            // Catch panics from individual page processing to avoid killing the writer
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                db.process_page(&page.url, &page.title, &page.content);
            }));

            if result.is_err() {
                eprintln!("Panic while indexing page: {}", page.url);
                state_for_writer
                    .pages_failed
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // Increment after successful write (not when sent to channel)
            state_for_writer
                .pages_indexed
                .fetch_add(1, Ordering::Relaxed);

            if last_status.elapsed().as_secs() < 10 {
                continue;
            }
            last_status = std::time::Instant::now();

            if let Err(e) = db.commit() {
                eprintln!("Error committing to index: {}", e);
                continue;
            }
            println!(
                "Status: {} indexed, {} failed",
                state_for_writer.pages_indexed.load(Ordering::Relaxed),
                state_for_writer.pages_failed.load(Ordering::Relaxed),
            );
        }
        db.commit()
    });

    // Crawl all domains concurrently
    futures::stream::iter(domains)
        .for_each_concurrent(max_concurrent, |domain| {
            let state = state.clone();
            let excluded = excluded_prefixes.clone();
            async move {
                crawl_domain(domain, state, excluded).await;
            }
        })
        .await;

    // Drop the sender to signal writer thread to finish
    drop(state);
    // Wait for writer to complete
    let total = writer_handle.join().expect("Writer thread panicked")?;
    println!("Crawl finished. Total docs: {}", total);
    Ok(())
}
