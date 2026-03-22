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

struct AppState {
    client: Client,
    blocked: HashSet<String>,
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
    source: String,
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
                        source: "wiby".to_string(),
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

async fn fetch_brave(client: &Client, query: &str) -> Vec<SearchResult> {
    let url = format!(
        "https://search.brave.com/search?q={}",
        urlencoding::encode(query)
    );

    // Attempt 1: emulate Firefox as closely as possible with headers
    if let Some(results) = fetch_brave_http(client, &url).await {
        return results;
    }

    // Attempt 2: headless browser (real TLS fingerprint, handles JS challenges)
    eprintln!("brave: HTTP failed, trying headless browser");
    fetch_brave_headless(&url).await
}

async fn fetch_brave_http(client: &Client, url: &str) -> Option<Vec<SearchResult>> {
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

    let html = resp.text().await.ok()?;
    let results = parse_brave(&html);
    eprintln!("brave: {} results (HTTP)", results.len());
    if results.is_empty() { None } else { Some(results) }
}

async fn fetch_brave_headless(url: &str) -> Vec<SearchResult> {
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
        cmd.args(args)
            .arg("--dump-dom")
            .arg(url)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => continue,
        };

        match timeout(Duration::from_secs(25), child.wait_with_output()).await {
            Ok(Ok(out)) if !out.stdout.is_empty() => {
                let html = String::from_utf8_lossy(&out.stdout);
                let results = parse_brave(&html);
                eprintln!("brave: {} results (headless {bin})", results.len());
                if !results.is_empty() {
                    return results;
                }
                eprintln!("brave: headless {bin} returned empty DOM");
            }
            Ok(Ok(_)) => eprintln!("brave: headless {bin} produced no output"),
            Ok(Err(e)) => eprintln!("brave: headless {bin} error: {e}"),
            Err(_) => eprintln!("brave: headless {bin} timed out"),
        }
    }

    eprintln!("brave: all attempts failed, 0 results");
    vec![]
}

fn parse_brave(html: &str) -> Vec<SearchResult> {
    let doc = ScraperHtml::parse_document(html);
    let sel_snippet = Selector::parse("div.snippet[data-type='web']").unwrap();
    let sel_link = Selector::parse("a.l1").unwrap();
    let sel_title = Selector::parse("div.title").unwrap();
    let sel_desc = Selector::parse("div.generic-snippet div.content").unwrap();
    let sel_img = Selector::parse("a.thumbnail img").unwrap();

    doc.select(&sel_snippet)
        .filter_map(|snippet| {
            let href = snippet
                .select(&sel_link)
                .next()
                .and_then(|a| a.value().attr("href"))
                .unwrap_or("")
                .to_string();
            if href.is_empty() {
                return None;
            }
            let title = snippet
                .select(&sel_title)
                .next()?
                .text()
                .collect::<String>();
            let description = snippet
                .select(&sel_desc)
                .next()
                .map(|d| d.text().collect::<String>())
                .unwrap_or_default();
            let image = snippet
                .select(&sel_img)
                .next()
                .and_then(|img| img.value().attr("src"))
                .filter(|src| src.starts_with("http"))
                .map(|s| s.to_string());
            Some(SearchResult {
                title: title.trim().to_string(),
                url: href,
                description: description.trim().to_string(),
                source: "brave".to_string(),
                image,
            })
        })
        .collect()
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
                    source: "marginalia".to_string(),
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
            source: "mwmbl".to_string(),
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
                source: "searchmysite".to_string(),
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
                source: "britannica".to_string(),
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
                                source: "reddit".to_string(),
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

fn reddit_normalize(url: &str) -> String {
    url.replace("old.reddit.com", "www.reddit.com")
}

fn rank_and_dedup(sources: &[Vec<SearchResult>], seen: &mut HashSet<String>) -> Vec<SearchResult> {
    // For each URL, accumulate sum of 1-based ranks and count across all sources
    let mut entries: HashMap<String, (SearchResult, f64, usize)> = HashMap::new();
    for source in sources {
        for (i, result) in source.iter().enumerate() {
            let key = reddit_normalize(&result.url);
            if seen.contains(&key) {
                continue;
            }
            let rank = (i + 1) as f64;
            entries.entry(key)
                .and_modify(|(_, total, count)| { *total += rank; *count += 1; })
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

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}

fn render_result(r: &SearchResult) -> String {
    let img_class = if r.source == "brave" { "result-img result-img-square" } else { "result-img" };
    let img_html = match &r.image {
        Some(src) => format!(
            "<img class=\"{img_class}\" src=\"{}\" alt=\"\" loading=\"lazy\" referrerpolicy=\"no-referrer\">",
            html_escape(src)
        ),
        None => String::new(),
    };
    format!(
        "<div class=\"result\">\
{img}\
<div class=\"result-source source-{source}\">{source}</div>\
<div class=\"result-title\"><a href=\"{url}\">{title}</a></div>\
<div class=\"result-url\">{url_text}</div>\
<div class=\"result-desc\">{desc}</div>\
</div>",
        source = html_escape(&r.source),
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
            let (wiby, brave, marginalia, mwmbl, searchmysite, britannica, reddit) = tokio::join!(
                fetch_wiby(&state.client, q),
                fetch_brave(&state.client, q),
                fetch_marginalia(&state.client, q),
                fetch_mwmbl(&state.client, q),
                fetch_searchmysite(&state.client, q),
                fetch_britannica(&state.client, q),
                fetch_reddit(&state.client, q),
            );

            let filter = |mut results: Vec<SearchResult>| -> Vec<SearchResult> {
                results.retain(|r| !is_blocked(&r.url, &state.blocked));
                results
            };

            let mut seen = HashSet::new();
            let big = rank_and_dedup(&[filter(reddit), filter(brave), filter(mwmbl)], &mut seen);
            let smol = rank_and_dedup(&[filter(wiby), filter(marginalia), filter(searchmysite), filter(britannica)], &mut seen);

            let total = smol.len() + big.len();
            let images_url = format!("https://search.brave.com/images?q={}", urlencoding::encode(q));
            let status = format!(
                "{total} results &nbsp;&middot;&nbsp; <a href=\"{}\">images</a>",
                html_escape(&images_url)
            );

            let smol_html: String = smol.iter().map(render_result).collect();
            let big_html: String = big.iter().map(render_result).collect();
            let results_html = format!(
                "<div id=\"results\">\
<div class=\"column\"><div class=\"column-label\">Discovery</div>{smol_html}</div>\
<div class=\"column-big\"><div class=\"column-label\">Web</div>{big_html}</div>\
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

    let state = Arc::new(AppState { client, blocked });

    let app = Router::new()
        .route("/", get(index))

        .with_state(state);

    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
