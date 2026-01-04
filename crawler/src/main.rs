use anyhow::{Context, Result};
use clap::Parser;

use reqwest::Client;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::fs::read_to_string;
use std::sync::Arc;
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED, TEXT,
};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index};
use tokio::task::JoinSet;
use url::Url;

mod bloom;
use bloom::BloomFilter;

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
    #[arg(short = 'c', long = "concurrency", default_value_t = 500)]
    max_concurrent_requests: usize,
    // verbosity flag could be added here
    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose_logging: bool,
}

struct IndexedPage {
    page_url: String,
    page_title: String,
    page_content: String,
}

#[derive(Clone, Eq, PartialEq)]
struct CrawlTask {
    crawl_depth: usize,
    target_url: Url,
    domain: Arc<str>,
}

impl Ord for CrawlTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Order by depth (DESCENDING - process deepest first), then by URL for uniqueness
        other
            .crawl_depth // Swap self/other to reverse order
            .cmp(&self.crawl_depth)
            .then_with(|| self.target_url.as_str().cmp(other.target_url.as_str()))
    }
}

impl PartialOrd for CrawlTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl CrawlTask {
    fn new(target_url: Url, crawl_depth: usize) -> Option<Self> {
        let host = match target_url.host_str() {
            Some(h) => h,
            None => return None,
        };
        Some(Self {
            domain: Arc::from(host),
            target_url,
            crawl_depth,
        })
    }
    fn should_skip(
        &self,
        max_depth: usize,
        excluded_prefixes: &[String],
        db: &mut CrawlDb,
        queued_urls: &HashSet<Arc<str>, ahash::RandomState>,
    ) -> bool {
        if self.crawl_depth > max_depth {
            return true;
        }
        if excluded_prefixes
            .iter()
            .any(|prefix| self.target_url.as_str().starts_with(prefix))
        {
            return true;
        }
        db.should_skip_url(self.target_url.as_str(), &queued_urls)
    }
}

struct SearchIndex {
    index: Index,
    url_field: tantivy::schema::Field,
    title_field: tantivy::schema::Field,
    body_field: tantivy::schema::Field,
}

struct CrawlDb {
    index: Index,
    index_writer: tantivy::IndexWriter,
    url_field: tantivy::schema::Field,
    title_field: tantivy::schema::Field,
    body_field: tantivy::schema::Field,
    seen_urls: BloomFilter<ahash::RandomState>,
    uncommitted_urls: HashSet<Box<str>, ahash::RandomState>,
    bloom_negative_count: usize,
    bloom_positive_count: usize,
}

impl CrawlDb {
    fn new(search_index: SearchIndex, expected_url_count: usize) -> Result<Self> {
        let index_writer = search_index.index.writer(50_000_000)?;
        let seen_urls = BloomFilter::with_num_bits(
            expected_url_count * 8,
            ahash::RandomState::new(),
            expected_url_count,
        );
        let uncommitted_urls = HashSet::with_hasher(ahash::RandomState::new());

        Ok(Self {
            index: search_index.index,
            index_writer,
            url_field: search_index.url_field,
            title_field: search_index.title_field,
            body_field: search_index.body_field,
            seen_urls,
            uncommitted_urls,
            bloom_negative_count: 0,
            bloom_positive_count: 0,
        })
    }

    fn searcher(&self) -> Result<tantivy::Searcher> {
        Ok(self.index.reader()?.searcher())
    }

    fn mark_seen(&mut self, url: &str) -> bool {
        self.seen_urls.insert(url)
    }

    fn should_skip_url(
        &mut self,
        url: &str,
        queued_urls: &HashSet<Arc<str>, ahash::RandomState>,
    ) -> bool {
        // Check uncommitted URLs first (already added to tantivy but not committed)
        if self.uncommitted_urls.contains(url) || queued_urls.contains(url) {
            return true;
        }

        if !self.seen_urls.contains(url) {
            self.bloom_negative_count += 1;
            return false;
        } else {
            self.bloom_positive_count += 1;
        }

        // possible collision: must verify db
        use tantivy::collector::DocSetCollector;
        use tantivy::query::TermQuery;

        let term = tantivy::Term::from_field_text(self.url_field, url);
        let query = TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        let searcher = match self.searcher() {
            Ok(s) => s,
            Err(_) => return false,
        };
        let Ok(doc_addresses) = searcher.search(&query, &DocSetCollector) else {
            return false;
        };

        !doc_addresses.is_empty()
    }

    fn index_page(&mut self, page: IndexedPage) -> Result<()> {
        let doc = doc!(
            self.url_field => &*page.page_url,
            self.title_field => &*page.page_title,
            self.body_field => &*page.page_content
        );
        self.index_writer.add_document(doc)?;
        self.uncommitted_urls.insert(page.page_url.into());
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.index_writer.commit()?;
        self.uncommitted_urls.clear();
        Ok(())
    }

    fn num_docs(&self) -> Result<u64> {
        Ok(self.index.reader()?.searcher().num_docs())
    }
}

fn setup_search_index() -> Result<SearchIndex> {
    let search_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(tantivy::tokenizer::Stemmer::new(
            tantivy::tokenizer::Language::English,
        ))
        .build();
    let body_field_indexing = TextFieldIndexing::default()
        .set_tokenizer("norm_tokenizer")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let body_field_options = TextOptions::default().set_indexing_options(body_field_indexing);
    let mut schema_builder = Schema::builder();
    let url_field = schema_builder.add_text_field("url", TEXT | STORED | FAST);
    let title_field = schema_builder.add_text_field("title", TEXT | STORED);
    let body_field = schema_builder.add_text_field("body", body_field_options);
    let schema = schema_builder.build();
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
    Ok(SearchIndex {
        index,
        url_field,
        title_field,
        body_field,
    })
}

fn extract_page_content(html_body: String) -> Result<(Vec<String>, String, String)> {
    let (readable_text, extracted_links, page_title) = find_main_content(html_body.as_bytes())?;
    use unicode_normalization::UnicodeNormalization;
    let normalized_text = readable_text.nfkc().collect::<String>();
    println!("Extracted Text: {}", &normalized_text);
    Ok((extracted_links, normalized_text, page_title))
}

async fn fetch_and_process_page(
    http_client: &Client,
    task: &CrawlTask,
) -> Result<(Vec<CrawlTask>, IndexedPage)> {
    let CrawlTask {
        target_url,
        crawl_depth,
        ..
    } = task;

    let response = http_client.get(target_url.clone()).send().await?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to fetch URL: {} with status: {}",
            target_url,
            response.status()
        ));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|header_value| header_value.to_str().ok())
        .unwrap_or("");

    if !content_type.starts_with("text/html") {
        return Err(anyhow::anyhow!(
            "Non-HTML content type: {} for URL: {}",
            content_type,
            target_url
        ));
    }
    let html_body = response.text().await?;
    let (extracted_links, page_content, page_title) = extract_page_content(html_body)?;

    let child_tasks = extracted_links
        .into_iter()
        .filter_map(|link| {
            let mut url = target_url.join(&link).ok()?;
            url.set_fragment(None);
            (url.host_str() == target_url.host_str())
                .then_some(CrawlTask::new(url, crawl_depth + 1)?)
        })
        .collect();
    Ok((
        child_tasks,
        IndexedPage {
            page_url: target_url.to_string(),
            page_content,
            page_title,
        },
    ))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {
    let cli_args = Args::parse();

    let file_content = read_to_string(&cli_args.input_file)
        .with_context(|| format!("Failed to read input file: {}", cli_args.input_file))?;

    let mut seed_urls = Vec::new();

    for line in file_content.lines() {
        let domain = line.trim();
        if domain.is_empty() {
            continue;
        }
        let clean_domain = domain
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let parsed_url =
            match Url::parse(&format!("https://{clean_domain}")).context("Invalid domain") {
                Ok(url) => url,
                Err(parse_error) => {
                    eprintln!("Skipping invalid domain '{}': {:?}", domain, parse_error);
                    continue;
                }
            };

        if let Some(_host) = parsed_url.host_str() {
            seed_urls.push(parsed_url);
        }
    }

    let http_client = Client::builder()
        // chrome
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
        .pool_idle_timeout(None)
        .tcp_nodelay(true)
        .build()?;

    let search_index = setup_search_index()?;
    let expected_url_count = 100_000_000;
    let mut db = CrawlDb::new(search_index, expected_url_count)?;

    let (mut pages_crawled, mut pages_failed) = (0usize, 0usize);
    let mut domains_in_progress: HashSet<Arc<str>, ahash::RandomState> =
        HashSet::with_hasher(ahash::RandomState::new());
    let mut queued_urls: HashSet<Arc<str>, ahash::RandomState> =
        HashSet::with_hasher(ahash::RandomState::new());
    let mut task_queue: BTreeSet<CrawlTask> = seed_urls
        .into_iter()
        .filter_map(|url| {
            db.mark_seen(url.as_str());
            queued_urls.insert(url.as_str().into());
            CrawlTask::new(url, 0)
        })
        .collect();
    let mut worker_pool = JoinSet::new();
    let mut last_status_report = std::time::Instant::now();

    loop {
        let to_spawn = cli_args
            .max_concurrent_requests
            .saturating_sub(worker_pool.len());
        for task in task_queue
            .extract_if(.., |task| {
                if !domains_in_progress.insert(task.domain.clone()) {
                    return false;
                }
                true
            })
            .take(to_spawn)
        {
            queued_urls.remove(task.target_url.as_str());
            let http_client = http_client.clone(); // http_client is Rc internally
            worker_pool.spawn(async move {
                //let ret = fetch_and_process_page(&http_client,&task).await;
                (fetch_and_process_page(&http_client, &task).await, task)
            });
        }

        if worker_pool.is_empty() && task_queue.is_empty() {
            break;
        }

        // Process completed task
        let Some(Ok(crawl_result)) = worker_pool.join_next().await else {
            continue;
        };

        let (fetch_result, task) = crawl_result;
        domains_in_progress.remove(&task.domain);
        let (discovered_links, page_data) = match fetch_result {
            Ok(res) => res,
            Err(e) => {
                if cli_args.verbose_logging {
                    eprintln!("Error fetching {}: {:?}", task.target_url, e);
                }
                pages_failed += 1;
                continue;
            }
        };

        for task in discovered_links {
            if !task.should_skip(
                cli_args.max_crawl_depth,
                &cli_args.excluded_url_prefixes,
                &mut db,
                &queued_urls,
            ) {
                let url_str = task.target_url.as_str();
                db.mark_seen(url_str);
                queued_urls.insert(url_str.into());
                task_queue.insert(task);
            }
        }

        // Index the page if we got content
        let _ = db.index_page(page_data);

        pages_crawled += 1;

        // Periodic status report
        if last_status_report.elapsed().as_secs() >= 5 {
            println!(
                "Crawled: {pages_crawled}, Failed: {pages_failed}, Queue: {}, Active: {}",
                task_queue.len(),
                domains_in_progress.len()
            );
            println!(
                " Uncommitted: URLs {}, Bloom positive ratio {:.2}%",
                db.uncommitted_urls.len(),
                (db.bloom_positive_count as f64
                    / (db.bloom_positive_count + db.bloom_negative_count) as f64)
                    * 100.0
            );
            last_status_report = std::time::Instant::now();
            db.commit().expect("Failed to commit index");
        }
    }
    println!("Crawl finished. Docs in index: {}", db.num_docs()?);

    Ok(())
}
