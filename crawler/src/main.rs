use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use parking_lot::Mutex;
use spider::website::Website;
use std::fs::read_to_string;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED, TEXT,
};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, Term};
use url::Url;

mod lol_readability;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(help = "Path to text file containing domains (one per line)")]
    input_file: String,
    #[arg(short = 'x', long = "exclude")]
    excluded_url_prefixes: Vec<String>,
    #[arg(short = 'c', long = "concurrency", default_value_t = 200)]
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

        let _ = self.index_writer.add_document(doc);
    }

    fn commit(&mut self) -> Result<u64> {
        self.index_writer.commit()?;
        let reader = self.index.reader()?;
        Ok(reader.searcher().num_docs())
    }
}

/// Shared state across all crawl tasks
struct SharedState {
    db: Mutex<CrawlDb>,
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
    if Url::parse(&url_str).is_ok() {
        Some(clean_domain.to_string())
    } else {
        None
    }
}

fn parse_domains(content: &str) -> Vec<String> {
    content.lines().filter_map(parse_domain_line).collect()
}

fn load_domains(file_path: &str) -> Result<Vec<String>> {
    let file_content = read_to_string(file_path)
        .with_context(|| format!("Failed to read input file: {}", file_path))?;

    let domains = parse_domains(&file_content);

    for line in file_content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') && parse_domain_line(line).is_none() {
            eprintln!("Skipping invalid domain: {}", trimmed);
        }
    }

    Ok(domains)
}

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// Crawl a single domain
async fn crawl_domain(
    domain: String,
    state: Arc<SharedState>,
    excluded_prefixes: Arc<Vec<String>>,
    verbose: bool,
) {
    let url = format!("https://{}/", domain);
    let mut website = Website::new(&url);

    website.configuration.respect_robots_txt = true;
    website.configuration.subdomains = true;
    website.configuration.only_html = true;
    website.configuration.delay = 5000;
    website.with_user_agent(Some(USER_AGENT));

    if !excluded_prefixes.is_empty() {
        website.with_blacklist_url(Some(
            excluded_prefixes.iter().cloned().map(Into::into).collect(),
        ));
    }

    let Some(mut rx) = website.subscribe(16384) else {
        eprintln!("Failed to subscribe to {} crawl events", domain);
        return;
    };

    let crawl_handle = tokio::spawn(async move {
        website.crawl().await;
    });

    let mut pages_received = 0usize;
    let mut pages_empty_html = 0usize;
    let mut pages_sent_to_index = 0usize;
    let mut lagged_count = 0usize;

    loop {
        let page = match rx.recv().await {
            Ok(page) => page,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                lagged_count += n as usize;
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

        pages_received += 1;

        let html = page.get_html();
        if html.is_empty() {
            pages_empty_html += 1;
            continue;
        }

        let page_url = page
            .final_redirect_destination
            .as_deref()
            .unwrap_or_else(|| page.get_url())
            .to_string();

        // Move CPU-bound work (readability + indexing) to blocking threadpool
        let state = state.clone();
        let domain_for_log = if verbose { Some(domain.clone()) } else { None };

        tokio::task::spawn_blocking(move || {
            match lol_readability::find_main_content(html.as_bytes(), &page_url) {
                Ok((content, title, resolved_url)) => {
                    let mut db = state.db.lock();
                    db.process_page(&resolved_url, &title, &content);
                    drop(db);
                    state.pages_indexed.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    state.pages_failed.fetch_add(1, Ordering::Relaxed);
                    if let Some(d) = domain_for_log {
                        eprintln!("[{}] Readability failed for {}: {}", d, page_url, e);
                    }
                }
            }
        });

        pages_sent_to_index += 1;
    }

    let _ = crawl_handle.await;

    if pages_received > 1 {
        println!(
            "Finished {} - received: {}, empty: {}, sent: {}, lagged: {}",
            domain, pages_received, pages_empty_html, pages_sent_to_index, lagged_count
        );
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {
    let cli_args = Args::parse();

    let state = Arc::new(SharedState {
        db: Mutex::new(CrawlDb::new()?),
        pages_indexed: AtomicUsize::new(0),
        pages_failed: AtomicUsize::new(0),
    });

    let domains = load_domains(&cli_args.input_file)?;
    println!("Loaded {} domains to crawl", domains.len());

    let excluded_prefixes = Arc::new(cli_args.excluded_url_prefixes);
    let max_concurrent = cli_args.max_concurrent_domains;
    let verbose = cli_args.verbose_logging;

    // Periodic status reporting and index commits
    let state_for_maint = state.clone();
    let maintenance_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            let indexed = state_for_maint.pages_indexed.load(Ordering::Relaxed);
            let failed = state_for_maint.pages_failed.load(Ordering::Relaxed);

            let mut db = state_for_maint.db.lock();
            match db.commit() {
                Ok(total_docs) => {
                    println!(
                        "Status: {} indexed, {} failed | committed, {} total docs",
                        indexed, failed, total_docs
                    );
                }
                Err(e) => {
                    println!(
                        "Status: {} indexed, {} failed | commit failed: {}",
                        indexed, failed, e
                    );
                }
            }
        }
    });

    futures::stream::iter(domains)
        .for_each_concurrent(max_concurrent, |domain| {
            let state = state.clone();
            let excluded = excluded_prefixes.clone();
            async move {
                crawl_domain(domain, state, excluded, verbose).await;
            }
        })
        .await;

    maintenance_task.abort();

    // Final commit
    let indexed = state.pages_indexed.load(Ordering::Relaxed);
    let failed = state.pages_failed.load(Ordering::Relaxed);

    let total = {
        let mut db = state.db.lock();
        db.commit()?
    };

    println!(
        "Crawl finished. Pages indexed: {}, Failed: {}, Total docs: {}",
        indexed, failed, total
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_domain_line_simple() {
        assert_eq!(
            parse_domain_line("example.com"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_with_subdomain() {
        assert_eq!(
            parse_domain_line("www.example.com"),
            Some("www.example.com".to_string())
        );
        assert_eq!(
            parse_domain_line("blog.example.com"),
            Some("blog.example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_strips_https() {
        assert_eq!(
            parse_domain_line("https://example.com"),
            Some("example.com".to_string())
        );
        assert_eq!(
            parse_domain_line("https://example.com/"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_strips_http() {
        assert_eq!(
            parse_domain_line("http://example.com"),
            Some("example.com".to_string())
        );
        assert_eq!(
            parse_domain_line("http://example.com/"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_strips_trailing_slash() {
        assert_eq!(
            parse_domain_line("example.com/"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_preserves_path() {
        assert_eq!(
            parse_domain_line("example.com/path"),
            Some("example.com/path".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_trims_whitespace() {
        assert_eq!(
            parse_domain_line("  example.com  "),
            Some("example.com".to_string())
        );
        assert_eq!(
            parse_domain_line("\texample.com\n"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_empty() {
        assert_eq!(parse_domain_line(""), None);
        assert_eq!(parse_domain_line("   "), None);
        assert_eq!(parse_domain_line("\t\n"), None);
    }

    #[test]
    fn test_parse_domain_line_comment() {
        assert_eq!(parse_domain_line("# this is a comment"), None);
        assert_eq!(parse_domain_line("  # indented comment"), None);
    }

    #[test]
    fn test_parse_domain_line_invalid() {
        assert_eq!(parse_domain_line("not a valid domain!@#$"), None);
        assert_eq!(parse_domain_line("http://"), None);
        assert_eq!(parse_domain_line("://missing-scheme"), None);
    }

    #[test]
    fn test_parse_domain_line_with_port() {
        assert_eq!(
            parse_domain_line("example.com:8080"),
            Some("example.com:8080".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_international() {
        assert_eq!(
            parse_domain_line("xn--n3h.com"),
            Some("xn--n3h.com".to_string())
        );
    }

    #[test]
    fn test_parse_domains_single() {
        let content = "example.com";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com"]);
    }

    #[test]
    fn test_parse_domains_multiple() {
        let content = "example.com\ntest.org\nfoo.bar";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org", "foo.bar"]);
    }

    #[test]
    fn test_parse_domains_with_empty_lines() {
        let content = "example.com\n\ntest.org\n\n\nfoo.bar\n";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org", "foo.bar"]);
    }

    #[test]
    fn test_parse_domains_with_comments() {
        let content = "# Domain list\nexample.com\n# Another comment\ntest.org";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org"]);
    }

    #[test]
    fn test_parse_domains_mixed_protocols() {
        let content = "https://example.com\nhttp://test.org/\nfoo.bar";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org", "foo.bar"]);
    }

    #[test]
    fn test_parse_domains_filters_invalid() {
        let content = "example.com\ninvalid!@#\ntest.org";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org"]);
    }

    #[test]
    fn test_parse_domains_empty_content() {
        let content = "";
        let domains = parse_domains(content);
        assert!(domains.is_empty());
    }

    #[test]
    fn test_parse_domains_only_comments_and_empty() {
        let content = "# Comment 1\n\n# Comment 2\n   \n";
        let domains = parse_domains(content);
        assert!(domains.is_empty());
    }

    #[test]
    fn test_parse_domains_windows_line_endings() {
        let content = "example.com\r\ntest.org\r\n";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org"]);
    }

    #[test]
    fn test_parse_domains_preserves_order() {
        let content = "zebra.com\nalpha.org\nmiddle.net";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["zebra.com", "alpha.org", "middle.net"]);
    }

    #[test]
    fn test_parse_domains_deduplication_not_applied() {
        let content = "example.com\nexample.com";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "example.com"]);
    }
}
