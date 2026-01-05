use sitemap::reader::{SiteMapEntity, SiteMapReader};
use std::collections::HashMap;
use std::sync::Arc;
use texting_robots::Robot;
use url::Url;

/// Cache of robots.txt rules per domain
pub struct RobotsCache {
    cache: HashMap<Arc<str>, Robot, ahash::RandomState>,
}

impl RobotsCache {
    pub fn new() -> Self {
        Self {
            cache: HashMap::with_hasher(ahash::RandomState::new()),
        }
    }

    /// Insert parsed robots.txt rules for a domain
    pub fn insert(&mut self, domain: Arc<str>, robot: Robot) {
        self.cache.insert(domain, robot);
    }

    /// Check if a URL is allowed (returns true if no rules cached yet)
    pub fn is_url_allowed(&self, url: &Url, domain: &str) -> bool {
        self.cache
            .get(domain)
            .map_or(true, |r| r.allowed(url.path()))
    }
}

/// Check if a URL is a robots.txt file
pub fn is_robots_url(url: &Url) -> bool {
    url.path() == "/robots.txt"
}

/// User agent string for robots.txt parsing (matches the Chrome UA used by the crawler)
const ROBOTS_USER_AGENT: &str = "Mozilla/5.0 (compatible; Crawler)";

/// Parse robots.txt content, returns (Robot, sitemap_urls)
pub fn parse_robots(content: &str) -> (Option<Robot>, Vec<String>) {
    let robot = Robot::new(ROBOTS_USER_AGENT, content.as_bytes()).ok();
    let sitemaps: Vec<String> = robot
        .as_ref()
        .map(|r| r.sitemaps.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    (robot, sitemaps)
}

/// Check if a URL looks like a sitemap (any .xml or .xml.gz file)
pub fn is_sitemap_url(url: &Url) -> bool {
    let path = url.path();
    let len = path.len();
    // Case-insensitive check without allocation
    (len >= 4 && path[len - 4..].eq_ignore_ascii_case(".xml"))
        || (len >= 7 && path[len - 7..].eq_ignore_ascii_case(".xml.gz"))
}

/// Parse sitemap content and return (page_urls, nested_sitemap_urls)
pub fn parse_sitemap(content: &[u8]) -> (Vec<String>, Vec<String>) {
    let mut pages = Vec::new();
    let mut sitemaps = Vec::new();

    for entity in SiteMapReader::new(content) {
        match entity {
            SiteMapEntity::Url(e) => {
                if let Some(url) = e.loc.get_url() {
                    pages.push(url.to_string());
                }
            }
            SiteMapEntity::SiteMap(e) => {
                if let Some(url) = e.loc.get_url() {
                    sitemaps.push(url.to_string());
                }
            }
            SiteMapEntity::Err(_) => {}
        }
    }
    (pages, sitemaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_robots_cache_allows_when_empty() {
        let cache = RobotsCache::new();
        let url = Url::parse("https://example.com/anything").unwrap();
        let domain: Arc<str> = "example.com".into();
        assert!(cache.is_url_allowed(&url, &domain));
    }

    #[test]
    fn test_robots_cache_with_rules() {
        let mut cache = RobotsCache::new();
        let domain: Arc<str> = "example.com".into();
        let robot =
            Robot::new(super::ROBOTS_USER_AGENT, b"User-agent: *\nDisallow: /admin/\n").unwrap();
        cache.insert(domain.clone(), robot);

        let allowed = Url::parse("https://example.com/public").unwrap();
        let blocked = Url::parse("https://example.com/admin/settings").unwrap();

        assert!(cache.is_url_allowed(&allowed, &domain));
        assert!(!cache.is_url_allowed(&blocked, &domain));
    }

    #[test]
    fn test_parse_robots_extracts_sitemaps() {
        let content =
            "User-agent: *\nDisallow: /private/\nSitemap: https://example.com/sitemap.xml\n";
        let (robot, sitemaps) = parse_robots(content);
        assert!(robot.is_some());
        assert_eq!(sitemaps, vec!["https://example.com/sitemap.xml"]);
    }
}
