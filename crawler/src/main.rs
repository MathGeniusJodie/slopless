use anyhow::{Context, Result};
use clap::Parser;
use fastbloom::BloomFilter;
use lol_html::{element, text, HtmlRewriter, Settings};
use reqwest::Client;
use reqwest_middleware::{ClientWithMiddleware, ClientBuilder};
use reqwest_retry::RetryTransientMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
use std::collections::{BinaryHeap, HashSet};
use std::cmp::Ordering;
use std::fs::read_to_string;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tantivy::schema::{IndexRecordOption, STORED, Schema, TEXT, TextFieldIndexing, TextOptions};
use tantivy::{doc, Index};
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
    #[arg(short = 'c', long = "concurrency", default_value_t = 500)]
    concurrency: usize,
    // verbosity flag could be added here
    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose: bool,
}

struct IndexedPage {
    page_url: String,
    page_title: String,
    page_content: String,
}

struct WebCrawler {
    http_client: ClientWithMiddleware,
    excluded_url_prefixes: Vec<String>,
    max_crawl_depth: usize,
    max_concurrent_requests: usize,
    pages_crawled: AtomicU64,
    pages_failed: AtomicU64,
    verbose_logging: bool,
}

#[derive(Eq, PartialEq)]
struct CrawlTask {
    crawl_depth: usize,
    target_url: Url,
    retry_count: usize,
}

impl Ord for CrawlTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; reverse depth comparison to get a min-heap by depth.
        other.crawl_depth.cmp(&self.crawl_depth)
    }
}

impl PartialOrd for CrawlTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl WebCrawler {
    pub async fn crawl_all_domains(self: Arc<Self>, seed_urls: Vec<Url>) -> Result<()> {
        ////////////////////////////////////////////////////////////////////////////////////////////////////////////
        // Set up Tantivy index
        ////////////////////////////////////////////////////////////////////////////////////////////////////////////
        let search_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .filter(tantivy::tokenizer::Stemmer::new(tantivy::tokenizer::Language::English))
            //.filter(  tantivy::tokenizer::SplitCompoundWords::from_dictionary(dict))
            .build();
        let body_field_indexing = TextFieldIndexing::default()
            .set_tokenizer("norm_tokenizer")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);

        let body_field_options = TextOptions::default()
            .set_indexing_options(body_field_indexing);
        let mut schema_builder = Schema::builder();
        let url_field = schema_builder.add_text_field("url", TEXT | STORED);
        let title_field = schema_builder.add_text_field("title", TEXT | STORED);
        let body_field = schema_builder.add_text_field("body", body_field_options);
        let schema = schema_builder.build();
        let search_index = if std::path::Path::new("search_db").exists() {
            Index::open_in_dir("search_db")?
        } else {
            std::fs::create_dir_all("search_db")?;
            let settings = tantivy::IndexSettings::default();
            Index::create(tantivy::directory::MmapDirectory::open("search_db")?, schema, settings)?
        };
        search_index.tokenizers()
            .register("norm_tokenizer", search_tokenizer);

        let mut index_writer = search_index.writer(1_000_000_000)?;
        ////////////////////////////////////////////////////////////////////////////////////////////////////////////
        // End set up Tantivy index
        ////////////////////////////////////////////////////////////////////////////////////////////////////////////
        
        let expected_url_count = 100_000_000;
        let mut seen_urls = BloomFilter::with_false_pos(1.0/(expected_url_count as f64))
            .hasher(ahash::RandomState::new())
            .expected_items(expected_url_count);
        let mut domains_in_progress = HashSet::with_hasher(ahash::RandomState::new());
        let mut task_queue: BinaryHeap<CrawlTask> = BinaryHeap::new();
        for url in seed_urls {
            task_queue.push(CrawlTask { target_url: url, crawl_depth: 0, retry_count: 0 });
        }

        let mut worker_pool = JoinSet::new();

        let mut last_status_report = std::time::Instant::now();
        loop {
            while worker_pool.len() <= self.max_concurrent_requests {
                // Pop items until we find one whose domain is not active, buffering the others.
                let mut deferred_tasks: Vec<CrawlTask> = Vec::new();
                let mut available_task: Option<CrawlTask> = None;

                while let Some(task) = task_queue.pop() {
                    if let Some(domain) = task.target_url.host_str() {
                        if !domains_in_progress.contains(domain) {
                            available_task = Some(task);
                            break;
                        }
                    }
                    if seen_urls.contains(&task.target_url.to_string()) {
                        // already visited, skip
                        continue;
                    }
                    deferred_tasks.push(task);
                }

                // Push buffered items back onto the heap
                for deferred in deferred_tasks.into_iter() { task_queue.push(deferred); }

                let current_task = match available_task {
                    Some(task) => task,
                    None => break, // No available item for an idle domain
                };

                let domain = match current_task.target_url.host_str() {
                    Some(host) => host.to_string(),
                    None => continue,
                };

                let crawler = Arc::clone(&self);
                let url_string = current_task.target_url.to_string();
                seen_urls.insert(&url_string);
                domains_in_progress.insert(domain.clone());
                worker_pool.spawn(async move {
                    (domain, crawler.fetch_and_process_page(current_task).await)
                });
            }

            if worker_pool.is_empty() && task_queue.is_empty() {
                break;
            }

            if let Some(join_result) = worker_pool.join_next().await {
                match join_result {
                    Ok((completed_domain, crawl_result)) => {
                        domains_in_progress.remove(&completed_domain);
                        let (discovered_links, page_data) = crawl_result;
                        if let Some(indexed_page) = page_data {
                            match index_writer.add_document(doc!(url_field => indexed_page.page_url, body_field => indexed_page.page_content, title_field => indexed_page.page_title)) {
                                Ok(_opstamp) => {},
                                Err(err) => {
                                    eprintln!("Indexing error: {:?}", err);
                                }
                            }
                            index_writer.commit().expect("Failed to commit index");
                        }
                        for new_task in discovered_links {
                            let already_seen = seen_urls.contains(&new_task.target_url.to_string());
                            if new_task.crawl_depth <= self.max_crawl_depth && !already_seen && !self.is_excluded_url(&new_task.target_url.to_string()) {
                                task_queue.push(new_task);
                            }
                        }
                    },
                    Err(join_error) => {
                        if self.verbose_logging {
                            eprintln!("Worker task failed: {:?}", join_error);
                        }
                    }
                }
            }
            // Periodic debug output
            if last_status_report.elapsed().as_secs() >= 5 {
                println!("Crawled: {}, Failed: {}, Queue size: {}, Active domains: {}",
                    self.pages_crawled.load(std::sync::atomic::Ordering::Relaxed),
                    self.pages_failed.load(std::sync::atomic::Ordering::Relaxed),
                    task_queue.len(),
                    domains_in_progress.len(),
                );
                last_status_report = std::time::Instant::now();
            }
        }
        println!("Crawl finished. Docs in index: {}", search_index.reader()?.searcher().num_docs());
        Ok(())
    }

    async fn fetch_and_process_page(&self, task: CrawlTask) -> (Vec<CrawlTask>, Option<IndexedPage>) {
        let CrawlTask { target_url, crawl_depth, retry_count } = task;
        
        let response = match self.http_client.get(target_url.clone()).send().await {
            Ok(resp) => resp,
            Err(network_error) => {
                if self.verbose_logging {
                    eprintln!("Network error: {:?}", network_error);
                }        
                self.pages_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return (vec![], None);
            }
        };
        if !response.status().is_success() { return (vec![], None); }
        
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|header_value| header_value.to_str().ok())
            .unwrap_or("");
            
        if !content_type.starts_with("text/html") { return (vec![], None); }
        
        let (extracted_links, page_content, page_title) = match self.extract_page_content(response).await {
            Ok(result) => result,
            Err(parse_error) => {
                if self.verbose_logging {
                    eprintln!("Parse error for {}: {:?}", target_url, parse_error);
                }        
                self.pages_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return (vec![], None);
            }
        };

        //println!("Depth {crawl_depth}: Crawling {url_str}");
        self.pages_crawled.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let next_depth = crawl_depth + 1;
        let child_tasks = extracted_links.into_iter()
            .filter_map(|link| target_url.join(&link).ok())
            .map(|mut resolved_url| { resolved_url.set_fragment(None); resolved_url })
            .filter(|resolved_url| {
                resolved_url.host_str().map_or(false, |link_host| target_url.host_str().map_or(false, |base_host| link_host == base_host))
            })
            .map(|resolved_url| CrawlTask { target_url: resolved_url, crawl_depth: next_depth, retry_count })
            .collect();

        (child_tasks, Some(IndexedPage { page_url: target_url.to_string(), page_content, page_title }))
    }

    async fn extract_page_content(&self, response: reqwest::Response) -> Result<(Vec<String>, String, String)> {
        let html_body = response.text().await?;
        let mut extracted_links = Vec::new();
        let mut page_title = String::new();
        let mut html_rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![
                    element!("a[href]", |anchor| {
                        if let Some(href) = anchor.get_attribute("href") { extracted_links.push(href); }
                        Ok(())
                    }),
                    text!("title", |title_text| {
                        page_title = title_text.as_str().to_string();
                        Ok(())
                    }),
                ],
                ..Settings::default()
            },
            |_: &[u8]| {},
        );

        html_rewriter.write(html_body.as_bytes())?;
        html_rewriter.end()?;

        use dom_smoothie::{Readability, Article, Config};
        let readability_config = Config::default();
        let mut readability_parser = Readability::new(html_body, None, Some(readability_config))?;
        let parsed_article: Article = readability_parser.parse()?;
        let readable_text = parsed_article.text_content;
        use unicode_normalization::UnicodeNormalization;
        let normalized_text = readable_text.nfkc().collect::<String>();
        Ok((extracted_links, normalized_text, page_title))
    }

    fn is_excluded_url(&self, url: &str) -> bool {
        self.excluded_url_prefixes.iter().any(|prefix| url.starts_with(prefix))
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 20)]
async fn main() -> Result<()> {

    let cli_args = Args::parse();

    let file_content = read_to_string(&cli_args.input_file)
        .with_context(|| format!("Failed to read input file: {}", cli_args.input_file))?;
    
    let mut seed_urls = Vec::new();

    for line in file_content.lines() {
        let domain = line.trim();
        if domain.is_empty() { continue; }
        let clean_domain = domain.trim_start_matches("http://").trim_start_matches("https://");
        let parsed_url = match Url::parse(&format!("https://{clean_domain}"))
            .context("Invalid domain") {
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

    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(4);
    let crawler = Arc::new(WebCrawler {
        http_client: ClientBuilder::new(
            Client::builder()
            // chrome
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
            // max concurrency for multiple hosts
            .pool_max_idle_per_host(0)
            .pool_idle_timeout(None)
            //.timeout(std::time::Duration::from_secs(5))
            .tcp_nodelay(true)
            .build()?
        )
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build(),
        excluded_url_prefixes: cli_args.exclude,
        max_crawl_depth: cli_args.max_depth,
        max_concurrent_requests: cli_args.concurrency,
        pages_crawled: AtomicU64::new(0),
        pages_failed: AtomicU64::new(0),
        verbose_logging: cli_args.verbose,
    });

    crawler.crawl_all_domains(seed_urls).await?;

    Ok(())
}