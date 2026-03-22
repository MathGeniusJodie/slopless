use axum::{
    extract::{Query, State},
    response::{Html, Json},
    routing::get,
    Router,
};
use reqwest::Client;
use scraper::{Html as ScraperHtml, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::net::TcpListener;

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

#[derive(Serialize)]
struct SearchResponse {
    smol: Vec<SearchResult>,
    big: Vec<SearchResult>,
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
        Ok(results) => results
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
            .collect(),
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
    let Ok(resp) = client
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
        )
        .header("Accept-Language", "en-US,en;q=0.5")
        .send()
        .await
    else {
        eprintln!("brave: request failed");
        return vec![];
    };
    match resp.text().await {
        Ok(html) => parse_brave(&html),
        Err(e) => {
            eprintln!("brave: read error: {e}");
            vec![]
        }
    }
}

fn parse_brave(html: &str) -> Vec<SearchResult> {
    let doc = ScraperHtml::parse_document(html);
    let sel_snippet = Selector::parse("div.snippet[data-type='web']").unwrap();
    let sel_link = Selector::parse("a.l1").unwrap();
    let sel_title = Selector::parse("div.title").unwrap();
    let sel_desc = Selector::parse("div.generic-snippet div.content").unwrap();

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
            Some(SearchResult {
                title: title.trim().to_string(),
                url: href,
                description: description.trim().to_string(),
                source: "brave".to_string(),
                image: None,
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
        Ok(data) => data
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
            .collect(),
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
        Ok(html) => parse_mwmbl(&html),
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
        Ok(html) => parse_searchmysite(&html),
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
        Ok(html) => parse_britannica(&html),
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
                Ok(listing) => listing
                    .data
                    .children
                    .into_iter()
                    .map(|post| {
                        let image = post.data.thumbnail.filter(|t| t.starts_with("http"));
                        SearchResult {
                            title: post.data.title,
                            url: format!("https://old.reddit.com{}", post.data.permalink),
                            description: post.data.selftext.unwrap_or_default(),
                            source: "reddit".to_string(),
                            image,
                        }
                    })
                    .collect(),
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

fn interleave(sources: &[Vec<SearchResult>], seen: &mut HashSet<String>) -> Vec<SearchResult> {
    let max_len = sources.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut out = Vec::new();
    for i in 0..max_len {
        for source in sources {
            if let Some(r) = source.get(i) {
                if seen.insert(reddit_normalize(&r.url)) {
                    out.push(r.clone());
                }
            }
        }
    }
    out
}

async fn search(
    Query(params): Query<SearchParams>,
    State(client): State<Arc<Client>>,
) -> Json<SearchResponse> {
    let Some(query) = params.q.filter(|q| !q.is_empty()) else {
        return Json(SearchResponse { smol: vec![], big: vec![] });
    };

    let (wiby, brave, marginalia, mwmbl, searchmysite, britannica, reddit) = tokio::join!(
        fetch_wiby(&client, &query),
        fetch_brave(&client, &query),
        fetch_marginalia(&client, &query),
        fetch_mwmbl(&client, &query),
        fetch_searchmysite(&client, &query),
        fetch_britannica(&client, &query),
        fetch_reddit(&client, &query),
    );

    // Big column: reddit first (priority), then brave, then mwmbl
    let mut seen = HashSet::new();
    let big = interleave(&[reddit, brave, mwmbl], &mut seen);

    // Smol column: wiby + marginalia + searchmysite + britannica, skip URLs already in big
    let smol = interleave(&[wiby, marginalia, searchmysite, britannica], &mut seen);

    Json(SearchResponse { smol, big })
}

const INDEX_HTML: &str = include_str!("index.html");

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

#[tokio::main]
async fn main() {
    let client = Arc::new(
        Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client"),
    );

    let app = Router::new()
        .route("/", get(index))
        .route("/search", get(search))
        .with_state(client);

    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
