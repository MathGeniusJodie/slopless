use axum::{
    extract::{Query, State},
    response::Html,
    routing::get,
    Router,
};
use reqwest::Client;
use scraper::{Html as ScraperHtml, Selector};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(Clone)]
struct FeedEntry {
    title: String,
    url: String,
    summary: String,
}

struct AppState {
    client: Client,
    blocked: HashSet<String>,
    feed_cache: tokio::sync::Mutex<Option<(Vec<FeedEntry>, std::time::Instant)>>,
}

fn load_blocklist(paths: &[&str]) -> HashSet<String> {
    let mut set = HashSet::new();
    for path in paths {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                for line in content.lines() {
                    let line = line.trim().to_lowercase();
                    if !line.is_empty() && !line.starts_with('#') {
                        set.insert(line);
                    }
                }
                eprintln!("blocklist: loaded {path}");
            }
            Err(e) => eprintln!("blocklist: could not read {path}: {e}"),
        }
    }
    set
}

fn url_host(url: &str) -> String {
    // Strip scheme
    let s = url.find("://").map_or(url, |i| &url[i + 3..]);
    // Strip path/query/fragment
    let s = s.split('/').next().unwrap_or(s);
    let s = s.split('?').next().unwrap_or(s);
    // Strip port
    let s = match s.rfind(':') {
        Some(i) if s[i + 1..].chars().all(|c| c.is_ascii_digit()) => &s[..i],
        _ => s,
    };
    // Strip www.
    s.strip_prefix("www.").unwrap_or(s).to_lowercase()
}

fn is_blocked(url: &str, blocked: &HashSet<String>) -> bool {
    let host = url_host(url);
    blocked.contains(&host) || blocked.iter().any(|b| host.ends_with(&format!(".{b}")))
}

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
}

#[derive(Serialize, Clone)]
struct SearchResult {
    title: String,
    url: String,
    description: String,
    sources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
}

// Wiby JSON API response
#[derive(Deserialize)]
struct WibyResult {
    #[serde(rename = "Title")]
    title: Option<String>,
    #[serde(rename = "URL")]
    url: Option<String>,
    #[serde(rename = "Snippet")]
    snippet: Option<String>,
}

// Marginalia API response
#[derive(Deserialize)]
struct MarginaliaResult {
    url: String,
    title: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct MarginaliaResponse {
    results: Option<Vec<MarginaliaResult>>,
}

async fn fetch_wiby(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!("https://wiby.me/json/?q={}", urlencoding::encode(query));
    let Ok(resp) = client.get(&url).send().await else {
        eprintln!("wiby: request failed");
        return vec![];
    };
    match resp.json::<Vec<WibyResult>>().await {
        Ok(results) => {
            let out: Vec<_> = results
                .into_iter()
                .filter_map(|r| {
                    Some(SearchResult {
                        title: r.title?,
                        url: r.url?,
                        description: r.snippet.unwrap_or_default(),
                        sources: vec!["wiby".to_string()],
                        image: None,
                    })
                })
                .collect();
            eprintln!("wiby: {} results", out.len());
            out
        }
        Err(e) => {
            eprintln!("wiby: parse error: {e}");
            vec![]
        }
    }
}

async fn fetch_brave_http_raw(client: &Client, url: &str) -> Option<String> {
    let resp = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0")
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.5")
        .header("Upgrade-Insecure-Requests", "1")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Site", "none")
        .header("Sec-Fetch-User", "?1")
        .header("DNT", "1")
        .header("Cache-Control", "max-age=0")
        .send()
        .await
        .ok()?;
    let status = resp.status();
    if !status.is_success() {
        eprintln!("brave: HTTP status {status}");
        return None;
    }
    resp.text().await.ok()
}

async fn fetch_brave_headless_raw(url: &str) -> Option<String> {
    use tokio::process::Command;
    use tokio::time::{timeout, Duration};
    use std::process::Stdio;

    let browsers = [
        ("librewolf", vec!["--headless", "--no-remote", "--profile", "/tmp/slopless-brave-profile"]),
        ("firefox",   vec!["--headless", "--no-remote", "--profile", "/tmp/slopless-brave-profile"]),
        ("chromium",  vec!["--headless=new", "--disable-gpu", "--no-sandbox"]),
        ("chromium-browser",   vec!["--headless=new", "--disable-gpu", "--no-sandbox"]),
        ("google-chrome-stable", vec!["--headless=new", "--disable-gpu", "--no-sandbox"]),
    ];

    for (bin, args) in &browsers {
        let mut cmd = Command::new(bin);
        cmd.args(args).arg("--dump-dom").arg(url)
            .stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        let child = match cmd.spawn() { Ok(c) => c, Err(_) => continue };
        match timeout(Duration::from_secs(25), child.wait_with_output()).await {
            Ok(Ok(out)) if !out.stdout.is_empty() => {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            Ok(Ok(_)) => eprintln!("brave: headless {bin} produced no output"),
            Ok(Err(e)) => eprintln!("brave: headless {bin} error: {e}"),
            Err(_) => eprintln!("brave: headless {bin} timed out"),
        }
    }
    None
}

async fn fetch_brave(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!("https://search.brave.com/search?q={}", urlencoding::encode(query));
    let html = match fetch_brave_http_raw(client, &url).await {
        Some(h) => h,
        None => {
            eprintln!("brave: HTTP failed, trying headless");
            match fetch_brave_headless_raw(&url).await {
                Some(h) => h,
                None => { eprintln!("brave: all attempts failed"); return vec![]; }
            }
        }
    };
    let results = parse_brave(&html);
    eprintln!("brave: {} results", results.len());
    results
}

fn parse_brave_typed(html: &str, data_type: &str, source_name: &str) -> Vec<SearchResult> {
    let doc = ScraperHtml::parse_document(html);
    let sel_snippet = Selector::parse(&format!("div.snippet[data-type='{data_type}']")).unwrap();
    let sel_link = Selector::parse("a.l1").unwrap();
    let sel_title = Selector::parse("div.title").unwrap();
    let sel_desc = Selector::parse("div.generic-snippet div.content").unwrap();
    let sel_img = Selector::parse("a.thumbnail img").unwrap();

    doc.select(&sel_snippet)
        .filter_map(|snippet| {
            let href = snippet.select(&sel_link).next()
                .and_then(|a| a.value().attr("href")).unwrap_or("").to_string();
            if href.is_empty() { return None; }
            let title = snippet.select(&sel_title).next()?.text().collect::<String>();
            let description = snippet.select(&sel_desc).next()
                .map(|d| d.text().collect::<String>()).unwrap_or_default();
            let image = snippet.select(&sel_img).next()
                .and_then(|img| img.value().attr("src"))
                .filter(|src| src.starts_with("http"))
                .map(|s| s.to_string());
            Some(SearchResult {
                title: title.trim().to_string(),
                url: href,
                description: description.trim().to_string(),
                sources: vec![source_name.to_string()],
                image,
            })
        })
        .collect()
}

fn parse_brave(html: &str) -> Vec<SearchResult> {
    parse_brave_typed(html, "web", "brave")
}

async fn fetch_brave_news(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!("https://search.brave.com/news?q={}", urlencoding::encode(query));
    let html = match fetch_brave_http_raw(client, &url).await {
        Some(h) => h,
        None => {
            eprintln!("brave news: HTTP failed, trying headless");
            match fetch_brave_headless_raw(&url).await {
                Some(h) => h,
                None => { eprintln!("brave news: all attempts failed"); return vec![]; }
            }
        }
    };
    let results = parse_brave_typed(&html, "news", "brave news");
    eprintln!("brave news: {} results", results.len());
    results
}

async fn fetch_marginalia(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!(
        "https://api.marginalia.nu/public/search/{}",
        urlencoding::encode(query)
    );
    let Ok(resp) = client.get(&url).send().await else {
        eprintln!("marginalia: request failed");
        return vec![];
    };
    match resp.json::<MarginaliaResponse>().await {
        Ok(data) => {
            let out: Vec<_> = data
                .results
                .unwrap_or_default()
                .into_iter()
                .map(|r| SearchResult {
                    title: r.title,
                    url: r.url,
                    description: r.description.unwrap_or_default(),
                    sources: vec!["marginalia".to_string()],
                    image: None,
                })
                .collect();
            eprintln!("marginalia: {} results", out.len());
            out
        }
        Err(e) => {
            eprintln!("marginalia: parse error: {e}");
            vec![]
        }
    }
}

async fn fetch_mwmbl(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!(
        "https://mwmbl.org/app/home/?q={}",
        urlencoding::encode(query)
    );
    let Ok(resp) = client
        .get(&url)
        .header("HX-Request", "true")
        .send()
        .await
    else {
        eprintln!("mwmbl: request failed");
        return vec![];
    };
    match resp.text().await {
        Ok(html) => {
            let out = parse_mwmbl(&html);
            eprintln!("mwmbl: {} results", out.len());
            out
        }
        Err(e) => {
            eprintln!("mwmbl: read error: {e}");
            vec![]
        }
    }
}

fn parse_mwmbl(html: &str) -> Vec<SearchResult> {
    let doc = ScraperHtml::parse_document(html);
    let sel_url = Selector::parse("input[name='url']").unwrap();
    let sel_title = Selector::parse("input[name='title']").unwrap();
    let sel_extract = Selector::parse("input[name='extract']").unwrap();

    let urls: Vec<String> = doc
        .select(&sel_url)
        .filter_map(|el| el.value().attr("value").map(str::to_string))
        .collect();
    let titles: Vec<String> = doc
        .select(&sel_title)
        .filter_map(|el| el.value().attr("value").map(str::to_string))
        .collect();
    let extracts: Vec<String> = doc
        .select(&sel_extract)
        .filter_map(|el| el.value().attr("value").map(str::to_string))
        .collect();

    urls.into_iter()
        .zip(titles)
        .zip(extracts)
        .map(|((url, title), description)| SearchResult {
            title,
            url,
            description,
            sources: vec!["mwmbl".to_string()],
            image: None,
        })
        .collect()
}

async fn fetch_searchmysite(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!(
        "https://searchmysite.net/search/?q={}",
        urlencoding::encode(query)
    );
    let Ok(resp) = client.get(&url).send().await else {
        eprintln!("searchmysite: request failed");
        return vec![];
    };
    match resp.text().await {
        Ok(html) => {
            let out = parse_searchmysite(&html);
            eprintln!("searchmysite: {} results", out.len());
            out
        }
        Err(e) => {
            eprintln!("searchmysite: read error: {e}");
            vec![]
        }
    }
}

fn parse_searchmysite(html: &str) -> Vec<SearchResult> {
    let doc = ScraperHtml::parse_document(html);
    let sel_result = Selector::parse("div.search-result").unwrap();
    let sel_link = Selector::parse("a.result-title").unwrap();
    let sel_title = Selector::parse("span.result-title-txt").unwrap();
    let sel_desc = Selector::parse("p#result-hightlight").unwrap();

    doc.select(&sel_result)
        .filter_map(|result| {
            let link = result.select(&sel_link).next()?;
            let href = link.value().attr("href")?.to_string();
            let title = result
                .select(&sel_title)
                .next()
                .map(|el| el.text().collect::<String>())
                .unwrap_or_else(|| link.text().collect());
            let description = result
                .select(&sel_desc)
                .next()
                .map(|d| d.text().collect::<String>())
                .unwrap_or_default();
            Some(SearchResult {
                title: title.trim().to_string(),
                url: href,
                description: description.split_whitespace().collect::<Vec<_>>().join(" "),
                sources: vec!["searchmysite".to_string()],
                image: None,
            })
        })
        .collect()
}

async fn fetch_britannica(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!(
        "https://www.britannica.com/search?query={}",
        urlencoding::encode(query)
    );
    let Ok(resp) = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0")
        .send()
        .await
    else {
        eprintln!("britannica: request failed");
        return vec![];
    };
    match resp.text().await {
        Ok(html) => {
            let out = parse_britannica(&html);
            eprintln!("britannica: {} results", out.len());
            out
        }
        Err(e) => {
            eprintln!("britannica: read error: {e}");
            vec![]
        }
    }
}

fn parse_britannica(html: &str) -> Vec<SearchResult> {
    let doc = ScraperHtml::parse_document(html);
    let sel_result = Selector::parse("li[class*=\"RESULT-\"]").unwrap();
    let sel_link = Selector::parse("a").unwrap();
    let sel_desc = Selector::parse("div").unwrap();

    doc.select(&sel_result)
        .filter_map(|result| {
            let link = result.select(&sel_link).next()?;
            let href = link.value().attr("href")?;
            let title = link.text().collect::<String>();
            let description = result
                .select(&sel_desc)
                .next()
                .map(|d| d.text().collect::<String>())
                .unwrap_or_default();
            Some(SearchResult {
                title: title.trim().to_string(),
                url: format!("https://www.britannica.com{href}"),
                description: description.split_whitespace().collect::<Vec<_>>().join(" "),
                sources: vec!["britannica".to_string()],
                image: None,
            })
        })
        .collect()
}

// Reddit JSON API response
#[derive(Deserialize)]
struct RedditListing {
    data: RedditListingData,
}

#[derive(Deserialize)]
struct RedditListingData {
    children: Vec<RedditPost>,
}

#[derive(Deserialize)]
struct RedditPost {
    data: RedditPostData,
}

#[derive(Deserialize)]
struct RedditPostData {
    title: String,
    permalink: String,
    selftext: Option<String>,
    thumbnail: Option<String>,
}

async fn fetch_reddit(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!(
        "https://www.reddit.com/search.json?q={}&limit=25",
        urlencoding::encode(query)
    );
    let Ok(resp) = client
        .get(&url)
        .header("User-Agent", "slopless-search/1.0")
        .send()
        .await
    else {
        eprintln!("reddit: request failed");
        return vec![];
    };
    match resp.text().await {
        Ok(body) => {
            match serde_json::from_str::<RedditListing>(&body) {
                Ok(listing) => {
                    let out: Vec<_> = listing
                        .data
                        .children
                        .into_iter()
                        .map(|post| {
                            let image = post.data.thumbnail
                                .filter(|t| t.starts_with("http"))
                                .map(|t| t.replace("&amp;", "&"));
                            SearchResult {
                                title: post.data.title,
                                url: format!("https://old.reddit.com{}", post.data.permalink),
                                description: post.data.selftext.unwrap_or_default(),
                                sources: vec!["reddit".to_string()],
                                image,
                            }
                        })
                        .collect();
                    eprintln!("reddit: {} results", out.len());
                    out
                }
                Err(e) => {
                    eprintln!("reddit: json parse error: {e}");
                    vec![]
                }
            }
        }
        Err(e) => {
            eprintln!("reddit: read error: {e}");
            vec![]
        }
    }
}

fn url_dedup_key(url: &str) -> String {
    let url = url.replace("old.reddit.com", "www.reddit.com");
    let url = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))
        .unwrap_or(&url).to_string();
    let url = url.strip_prefix("www.").unwrap_or(&url).to_string();
    url.trim_end_matches('/').to_lowercase()
}

fn rank_and_dedup(
    sources: &[Vec<SearchResult>],
    seen: &mut HashSet<String>,
    rank_adjust: impl Fn(&SearchResult, f64) -> f64,
) -> Vec<SearchResult> {
    // For each URL, accumulate sum of 1-based ranks and count across all sources
    let mut entries: HashMap<String, (SearchResult, f64, usize)> = HashMap::new();
    for source in sources {
        for (i, result) in source.iter().enumerate() {
            let key = url_dedup_key(&result.url);
            if seen.contains(&key) {
                continue;
            }
            let rank = rank_adjust(result, (i + 1) as f64);
            let new_source = result.sources[0].clone();
            entries.entry(key)
                .and_modify(|(r, total, count)| {
                    *total += rank;
                    *count += 1;
                    if !r.sources.contains(&new_source) {
                        r.sources.push(new_source.clone());
                    }
                })
                .or_insert_with(|| (result.clone(), rank, 1));
        }
    }

    // Sort by average rank ascending (lower = better)
    let mut sorted: Vec<(String, SearchResult, f64)> = entries
        .into_iter()
        .map(|(key, (result, total, count))| (key, result, total / count as f64))
        .collect();
    sorted.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

    sorted.into_iter().map(|(key, result, _)| { seen.insert(key); result }).collect()
}

async fn fetch_apnews(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!("https://apnews.com/search?q={}", urlencoding::encode(query));
    let Ok(resp) = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0")
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .send()
        .await
    else {
        eprintln!("apnews: request failed");
        return vec![];
    };
    match resp.text().await {
        Ok(html) => {
            let out = parse_apnews(&html);
            eprintln!("apnews: {} results", out.len());
            out
        }
        Err(e) => { eprintln!("apnews: read error: {e}"); vec![] }
    }
}

fn parse_apnews(html: &str) -> Vec<SearchResult> {
    let doc = ScraperHtml::parse_document(html);
    let sel_result = Selector::parse("div.PagePromo").unwrap();
    let sel_title_link = Selector::parse("div.PagePromo-title a.Link").unwrap();
    let sel_title_text = Selector::parse("div.PagePromo-title span.PagePromoContentIcons-text").unwrap();
    let sel_desc = Selector::parse("div.PagePromo-description span.PagePromoContentIcons-text").unwrap();

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let one_month_ago = now_secs.saturating_sub(7 * 24 * 3600);

    doc.select(&sel_result)
        .filter_map(|result| {
            let ts_secs = result.value()
                .attr("data-posted-date-timestamp")
                .and_then(|t| t.parse::<u64>().ok())
                .map(|ms| ms / 1000)
                .unwrap_or(0);
            if ts_secs < one_month_ago {
                return None;
            }
            let url = result.select(&sel_title_link).next()
                .and_then(|a| a.value().attr("href"))?.to_string();
            let title = result.select(&sel_title_text).next()
                .map(|el| el.text().collect::<String>())
                .unwrap_or_default();
            let description = result.select(&sel_desc).next()
                .map(|el| el.text().collect::<String>())
                .unwrap_or_default();
            Some(SearchResult {
                title: title.trim().to_string(),
                url,
                description: description.trim().to_string(),
                sources: vec!["apnews".to_string()],
                image: None,
            })
        })
        .collect()
}

fn strip_html(html: &str) -> String {
    let doc = ScraperHtml::parse_fragment(html);
    doc.root_element()
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_atom_feed(xml: &str) -> Vec<FeedEntry> {
    let doc = match roxmltree::Document::parse(xml) {
        Ok(d) => d,
        Err(e) => { eprintln!("smallweb: XML parse error: {e}"); return vec![]; }
    };
    doc.descendants()
        .filter(|n| n.tag_name().name() == "entry")
        .filter_map(|entry| {
            let title = entry.descendants()
                .find(|n| n.tag_name().name() == "title")
                .and_then(|n| n.text())
                .unwrap_or("").trim().to_string();
            let url = entry.descendants()
                .find(|n| n.tag_name().name() == "link")
                .and_then(|n| n.attribute("href"))
                .unwrap_or("").to_string();
            if url.is_empty() { return None; }
            let summary_html = entry.descendants()
                .find(|n| n.tag_name().name() == "summary" || n.tag_name().name() == "content")
                .and_then(|n| n.text())
                .unwrap_or("").to_string();
            let summary = strip_html(&summary_html);
            Some(FeedEntry { title, url, summary })
        })
        .collect()
}

fn search_feed(entries: &[FeedEntry], query: &str) -> Vec<SearchResult> {
    let words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
    if words.is_empty() { return vec![]; }
    let mut scored: Vec<(usize, &FeedEntry)> = entries.iter()
        .filter_map(|entry| {
            let text = format!("{} {}", entry.title, entry.summary).to_lowercase();
            let text_words: HashSet<&str> = text
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .collect();
            let score = words.iter().filter(|w| text_words.contains(w.as_str())).count();
            if score > 0 { Some((score, entry)) } else { None }
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter()
        .map(|(_, e)| SearchResult {
            title: e.title.clone(),
            url: e.url.clone(),
            description: e.summary.clone(),
            sources: vec!["smallweb news".to_string()],
            image: None,
        })
        .collect()
}

async fn fetch_smallweb(state: &AppState, query: &str) -> Vec<SearchResult> {
    use std::time::{Duration, Instant};
    // Return cached entries if fresh
    {
        let cache = state.feed_cache.lock().await;
        if let Some((entries, cached_at)) = &*cache {
            if cached_at.elapsed() < Duration::from_secs(3600) {
                let results = search_feed(entries, query);
                eprintln!("smallweb: {} results (cached)", results.len());
                return results;
            }
        }
    }
    // Fetch fresh feed
    let fresh = match state.client
        .get("https://kagi.com/api/v1/smallweb/feed/")
        .header("User-Agent", "slopless-search/1.0")
        .send()
        .await
    {
        Ok(resp) => match resp.text().await {
            Ok(xml) => {
                let entries = parse_atom_feed(&xml);
                eprintln!("smallweb: fetched {} feed entries", entries.len());
                Some(entries)
            }
            Err(e) => { eprintln!("smallweb: read error: {e}"); None }
        },
        Err(e) => { eprintln!("smallweb: request failed: {e}"); None }
    };
    let mut cache = state.feed_cache.lock().await;
    let entries = if let Some(entries) = fresh {
        *cache = Some((entries.clone(), Instant::now()));
        entries
    } else {
        cache.as_ref().map(|(e, _)| e.clone()).unwrap_or_default()
    };
    let results = search_feed(&entries, query);
    eprintln!("smallweb: {} results", results.len());
    results
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}

fn render_result(r: &SearchResult) -> String {
    let img_class = if r.sources.contains(&"brave".to_string()) { "result-img result-img-square" } else { "result-img" };
    let img_html = match &r.image {
        Some(src) => format!(
            "<img class=\"{img_class}\" src=\"{}\" alt=\"\" loading=\"lazy\" referrerpolicy=\"no-referrer\">",
            html_escape(src)
        ),
        None => String::new(),
    };
    let sources_html: String = r.sources.iter()
        .map(|s| format!("<span class=\"source-{}\">{}</span>", html_escape(&s.replace(' ', "-")), html_escape(s)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "<div class=\"result\">\
{img}\
<div class=\"result-source\">{sources_html}</div>\
<div class=\"result-title\"><a href=\"{url}\">{title}</a></div>\
<div class=\"result-url\">{url_text}</div>\
<div class=\"result-desc\">{desc}</div>\
</div>",
        url = html_escape(&r.url),
        title = html_escape(&r.title),
        url_text = html_escape(&r.url),
        img = img_html,
        desc = html_escape(&r.description),
    )
}

const INDEX_HTML: &str = include_str!("index.html");

async fn index(
    Query(params): Query<SearchParams>,
    State(state): State<Arc<AppState>>,
) -> Html<String> {
    let query = params.q.filter(|q| !q.is_empty());

    let (status, results_html) = match query.as_deref() {
        None => (String::new(), String::new()),
        Some(q) => {
            let (wiby, brave, brave_news, marginalia, mwmbl, searchmysite, britannica, reddit, smallweb, apnews) = tokio::join!(
                fetch_wiby(&state.client, q),
                fetch_brave(&state.client, q),
                fetch_brave_news(&state.client, q),
                fetch_marginalia(&state.client, q),
                fetch_mwmbl(&state.client, q),
                fetch_searchmysite(&state.client, q),
                fetch_britannica(&state.client, q),
                fetch_reddit(&state.client, q),
                fetch_smallweb(&state, q),
                fetch_apnews(&state.client, q),
            );

            let filter = |mut results: Vec<SearchResult>| -> Vec<SearchResult> {
                results.retain(|r| !is_blocked(&r.url, &state.blocked));
                results
            };

            let mut seen = HashSet::new();
            let news_sources = ["apnews", "brave news"];
            let query_words: Vec<String> = q.split_whitespace()
                .map(|w| w.to_lowercase()).collect();
            let relevance_penalty = |r: &SearchResult, rank: f64| -> f64 {
                if r.sources.iter().any(|s| s == "smallweb news") {
                    return rank;
                }
                let text = format!("{} {}", r.title, r.description).to_lowercase();
                let all_present = query_words.iter().all(|w| text.contains(w.as_str()));
                if all_present { rank } else { rank * 2.0 + 6.0 }
            };

            let kw_cull = |mut results: Vec<SearchResult>| -> Vec<SearchResult> {
                if query_words.is_empty() { return results; }
                results.retain(|r| {
                    let text = format!("{} {} {}", r.title, r.description, r.url).to_lowercase();
                    query_words.iter().any(|w| text.contains(w.as_str()))
                });
                results
            };

            // Pre-filter smol sources so we can augment big results with their source names
            let smol_vecs = [
                kw_cull(filter(reddit)), kw_cull(filter(wiby)), kw_cull(filter(marginalia)),
                kw_cull(filter(searchmysite)), kw_cull(filter(smallweb)),
            ];

            let mut big = rank_and_dedup(
                &[filter(brave), kw_cull(filter(brave_news)), kw_cull(filter(mwmbl)), kw_cull(filter(britannica)), kw_cull(filter(apnews))],
                &mut seen,
                |r, rank| {
                    let rank = relevance_penalty(r, rank);
                    if r.sources.iter().any(|s| news_sources.contains(&s.as_str())) {
                        rank * 2.0 + 6.0
                    } else {
                        rank
                    }
                },
            );

            // For any smol result whose URL is already in big, add its source name to the big result
            let big_key_to_idx: HashMap<String, usize> = big.iter().enumerate()
                .map(|(i, r)| (url_dedup_key(&r.url), i))
                .collect();
            for source_vec in &smol_vecs {
                for result in source_vec {
                    let key = url_dedup_key(&result.url);
                    if let Some(&idx) = big_key_to_idx.get(&key) {
                        let new_src = &result.sources[0];
                        if !big[idx].sources.contains(new_src) {
                            big[idx].sources.push(new_src.clone());
                        }
                        if new_src == "reddit" {
                            big[idx].url = result.url.clone();
                        }
                    }
                }
            }

            let smol = rank_and_dedup(
                &smol_vecs,
                &mut seen,
                |r, rank| relevance_penalty(r, rank),
            );

            let total = smol.len() + big.len();
            let images_url = format!("https://search.brave.com/images?q={}", urlencoding::encode(q));
            let status = format!(
                "{total} results &nbsp;&middot;&nbsp; <a href=\"{}\">images</a>",
                html_escape(&images_url)
            );

            let smol_html: String = smol.iter().map(render_result).collect();
            let big_col1: String = big.iter().step_by(2).map(render_result).collect();
            let big_col2: String = big.iter().skip(1).step_by(2).map(render_result).collect();
            let results_html = format!(
                "<div id=\"results\">\
<div class=\"column\"><div class=\"column-label\">Discovery</div>{smol_html}</div>\
<div class=\"column-big\"><div class=\"column-label\">Web</div>\
<div class=\"column\">{big_col1}</div>\
<div class=\"column\">{big_col2}</div>\
</div>\
</div>"
            );

            (status, results_html)
        }
    };

    let page = INDEX_HTML
        .replace("{{QUERY}}", &html_escape(query.as_deref().unwrap_or("")))
        .replace("{{STATUS}}", &status)
        .replace("{{RESULTS}}", &results_html);

    Html(page)
}

#[tokio::main]
async fn main() {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client");

    let blocked = load_blocklist(&[
        "../handpicked-fakenews.txt",
        "../handpicked-blocklist.txt",
    ]);
    eprintln!("blocklist: {} domains total", blocked.len());

    let state = Arc::new(AppState { client, blocked, feed_cache: tokio::sync::Mutex::new(None) });

    let app = Router::new()
        .route("/", get(index))

        .with_state(state);

    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
