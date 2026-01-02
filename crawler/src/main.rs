use anyhow::{Context, Result};
use clap::Parser;
use fastbloom::BloomFilter;
use lol_html::{element, text, HtmlRewriter, Settings};
use reqwest::Client;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::fs::read_to_string;
use std::sync::Arc;
use tantivy::schema::{IndexRecordOption, Schema, TextFieldIndexing, TextOptions, STORED, TEXT};
use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
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
    verbose_logging: bool,
}

#[derive(Clone, Eq, PartialEq)]
struct CrawlTask {
    crawl_depth: usize,
    target_url: Url,
    retry_count: usize,
}

impl Ord for CrawlTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Order by depth (ascending), then by URL for uniqueness in BTreeSet
        self.crawl_depth
            .cmp(&other.crawl_depth)
            .then_with(|| self.target_url.as_str().cmp(other.target_url.as_str()))
    }
}

impl PartialOrd for CrawlTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct SearchIndex {
    index: Index,
    url_field: tantivy::schema::Field,
    title_field: tantivy::schema::Field,
    body_field: tantivy::schema::Field,
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
    let url_field = schema_builder.add_text_field("url", TEXT | STORED);
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

fn pop_available_task(
    task_queue: &mut BTreeSet<CrawlTask>,
    domains_in_progress: &HashSet<String, ahash::RandomState>,
) -> Option<CrawlTask> {
    let task = task_queue
        .iter()
        .find(|task| {
            task.target_url
                .host_str()
                .map_or(false, |d| !domains_in_progress.contains(d))
        })
        .cloned()?;
    task_queue.take(&task)
}

fn extract_page_content(html_body: String) -> Result<(Vec<String>, String, String)> {
    let mut extracted_links = Vec::new();
    let mut page_title = String::new();
    let mut html_rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("a[href]", |anchor| {
                    if let Some(href) = anchor.get_attribute("href") {
                        extracted_links.push(href);
                    }
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

    use dom_smoothie::{Article, Config, Readability};
    let readability_config = Config::default();
    let mut readability_parser = Readability::new(html_body, None, Some(readability_config))?;
    let parsed_article: Article = readability_parser.parse()?;
    let readable_text = parsed_article.text_content;
    use unicode_normalization::UnicodeNormalization;
    let normalized_text = readable_text.nfkc().collect::<String>();
    Ok((extracted_links, normalized_text, page_title))
}

impl WebCrawler {
    pub async fn crawl_all_domains(self: Arc<Self>, seed_urls: Vec<Url>) -> Result<()> {
        let SearchIndex {
            index,
            url_field,
            title_field,
            body_field,
        } = setup_search_index()?;
        let mut index_writer = index.writer(1_000_000_000)?;

        let (mut pages_crawled, mut pages_failed) = (0usize, 0usize);
        let expected_url_count = 100_000_000;
        let mut seen_urls = BloomFilter::with_false_pos(1.0 / (expected_url_count as f64))
            .hasher(ahash::RandomState::new())
            .expected_items(expected_url_count);
        let mut domains_in_progress = HashSet::with_hasher(ahash::RandomState::new());
        let mut task_queue: BTreeSet<CrawlTask> = seed_urls
            .into_iter()
            .map(|url| CrawlTask {
                target_url: url,
                crawl_depth: 0,
                retry_count: 0,
            })
            .collect();
        let mut worker_pool = JoinSet::new();
        let mut last_status_report = std::time::Instant::now();

        loop {
            while worker_pool.len() <= self.max_concurrent_requests {
                let Some(task) = pop_available_task(&mut task_queue, &domains_in_progress) else {
                    break;
                };
                let Some(domain) = task.target_url.host_str().map(String::from) else {
                    continue;
                };
                seen_urls.insert(&task.target_url.to_string());
                domains_in_progress.insert(domain.clone());
                let crawler = Arc::clone(&self);
                worker_pool
                    .spawn(async move { (domain, crawler.fetch_and_process_page(task).await) });
            }

            if worker_pool.is_empty() && task_queue.is_empty() {
                break;
            }

            let Some(join_result) = worker_pool.join_next().await else {
                continue;
            };
            let (domain, crawl_result) = match join_result {
                Ok(r) => r,
                Err(e) => {
                    if self.verbose_logging {
                        eprintln!("Worker task failed: {:?}", e);
                    }
                    continue;
                }
            };
            domains_in_progress.remove(&domain);

            let (discovered_links, page_data) = match crawl_result {
                Ok(r) => r,
                Err(e) => {
                    pages_failed += 1;
                    if self.verbose_logging {
                        eprintln!("Error crawling {}: {:?}", domain, e);
                    }
                    continue;
                }
            };

            if let Some(p) = page_data {
                if let Err(e) = index_writer.add_document(doc!(url_field => p.page_url, body_field => p.page_content, title_field => p.page_title)) {
                    eprintln!("Indexing error: {:?}", e);
                }
                index_writer.commit().expect("Failed to commit index");
            }
            for task in discovered_links {
                let dominated = task.crawl_depth > self.max_crawl_depth
                    || seen_urls.contains(&task.target_url.to_string())
                    || self.is_excluded_url(&task.target_url.to_string());
                if !dominated {
                    task_queue.insert(task);
                }
            }
            pages_crawled += 1;

            if last_status_report.elapsed().as_secs() >= 5 {
                println!(
                    "Crawled: {}, Failed: {}, Queue: {}, Active: {}",
                    pages_crawled,
                    pages_failed,
                    task_queue.len(),
                    domains_in_progress.len()
                );
                last_status_report = std::time::Instant::now();
            }
        }
        println!(
            "Crawl finished. Docs in index: {}",
            index.reader()?.searcher().num_docs()
        );
        Ok(())
    }

    async fn fetch_and_process_page(
        &self,
        task: CrawlTask,
    ) -> Result<(Vec<CrawlTask>, Option<IndexedPage>)> {
        let CrawlTask {
            target_url,
            crawl_depth,
            retry_count,
        } = task;

        let response = self.http_client.get(target_url.clone()).send().await?;
        if !response.status().is_success() {
            return Ok((vec![], None));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|header_value| header_value.to_str().ok())
            .unwrap_or("");

        if !content_type.starts_with("text/html") {
            return Ok((vec![], None));
        }
        let html_body = response.text().await?;
        let (extracted_links, page_content, page_title) = extract_page_content(html_body)?;

        let next_depth = crawl_depth + 1;
        let child_tasks = extracted_links
            .into_iter()
            .filter_map(|link| target_url.join(&link).ok())
            .map(|mut resolved_url| {
                resolved_url.set_fragment(None);
                resolved_url
            })
            .filter(|resolved_url| {
                resolved_url.host_str().map_or(false, |link_host| {
                    target_url
                        .host_str()
                        .map_or(false, |base_host| link_host == base_host)
                })
            })
            .map(|resolved_url| CrawlTask {
                target_url: resolved_url,
                crawl_depth: next_depth,
                retry_count,
            })
            .collect();

        Ok((
            child_tasks,
            Some(IndexedPage {
                page_url: target_url.to_string(),
                page_content,
                page_title,
            }),
        ))
    }

    fn is_excluded_url(&self, url: &str) -> bool {
        self.excluded_url_prefixes
            .iter()
            .any(|prefix| url.starts_with(prefix))
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
        verbose_logging: cli_args.verbose,
    });

    crawler.crawl_all_domains(seed_urls).await?;

    Ok(())
}
