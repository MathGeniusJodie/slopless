use lol_html::{element, text, EndTagHandler, HtmlRewriter, Settings};
use regex::Regex;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;

// -----------------------------------------------------------------------------
// Static Configuration (Compiled Once)
// -----------------------------------------------------------------------------
static RE_UNLIKELY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)banner|combx|comment|community|disqus|extra|foot|header|menu|related|remark|rss|share|shoutbox|sidebar|skyscraper|sponsor|ad-break|agegate|pagination|popup"
    ).unwrap()
});
static RE_MAYBE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)and|article|body|column|main|shadow").unwrap());
static RE_POSITIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)article|body|content|entry|hentry|main|page|pagination|post|text|blog|story")
        .unwrap()
});
static RE_NEGATIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)hidden|banner|combx|comment|com-|contact|foot|footer|footnote|masthead|media|meta|outbrain|promo|related|scroll|shoutbox|sidebar|sponsor|shopping|tags|tool|widget|nav|namespace|action|catlinks|toc|printfooter|jump-to|siteSub|contentSub"
    ).unwrap()
});

// -----------------------------------------------------------------------------
// Data Structures
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagType {
    Div,
    P,
    A,
    Header,  // h1-h6
    List,    // ul, ol, dl
    Article, // article, main, section - semantic content tags
    Other,
}

/// A stack frame representing an open element.
#[derive(Debug, Default)]
struct Frame {
    tag: Option<TagType>,
    score: f32,
    text_len: u32,
    link_text_len: u32,
    commas: u32,
    text: String, // Accumulated text content
}

impl Frame {
    fn new(name: &str, id: Option<&str>, class: Option<&str>) -> Self {
        let tag = match name {
            "div" => TagType::Div,
            "p" => TagType::P,
            "a" => TagType::A,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => TagType::Header,
            "ul" | "ol" | "dl" => TagType::List,
            "article" | "main" | "section" => TagType::Article,
            _ => TagType::Other,
        };

        // Base Score Heuristics
        let mut score = match tag {
            TagType::Article => 30.0, // Semantic content tags get high base score
            TagType::Div => 5.0,
            TagType::P => 10.0,
            TagType::Header => -5.0,
            TagType::List => -3.0,
            _ => 0.0,
        };

        let check_weight = |val: &str| -> f32 {
            if RE_UNLIKELY.is_match(val) && !RE_MAYBE.is_match(val) {
                return -50.0;
            }
            if RE_POSITIVE.is_match(val) {
                return 25.0;
            }
            if RE_NEGATIVE.is_match(val) {
                return -25.0;
            }
            0.0
        };

        if let Some(s) = id {
            score += check_weight(s);
        }
        if let Some(s) = class {
            score += check_weight(s);
        }

        Self {
            tag: Some(tag),
            score,
            text_len: 0,
            link_text_len: 0,
            commas: 0,
            text: String::new(),
        }
    }

    fn append_text(&mut self, s: &str) {
        let mut iter = s.split_whitespace().peekable();
        if iter.peek().is_none() {
            return;
        }
        if !self.text.is_empty() && !self.text.ends_with(' ') {
            self.text.push(' ');
        }
        if let Some(first) = iter.next() {
            self.text.push_str(first);
            for word in iter {
                self.text.push(' ');
                self.text.push_str(word);
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Helper Functions
// -----------------------------------------------------------------------------

/// Container tags whose entire content should be ignored
fn is_non_content_container(tag: &str) -> bool {
    matches!(
        tag,
        "head"
            | "script"
            | "style"
            | "noscript"
            | "template"
            | "svg"
            | "math"
            | "canvas"
            | "iframe"
            | "object"
            | "embed"
            | "applet"
            | "audio"
            | "video"
            | "form"
    )
}

/// Void/self-closing tags
fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Non-content leaf tags
fn is_non_content_leaf(tag: &str) -> bool {
    matches!(
        tag,
        "title"
            | "base"
            | "input"
            | "textarea"
            | "select"
            | "option"
            | "optgroup"
            | "button"
            | "area"
            | "map"
            | "param"
            | "datalist"
            | "output"
            | "progress"
            | "meter"
            | "picture"
    )
}

// -----------------------------------------------------------------------------
// Main Function (Single Pass)
// -----------------------------------------------------------------------------

/// Find and extract the main content as plain text.
/// Returns: (content_text, urls, title)
/// Returns an error if no suitable content element was found.
pub fn find_main_content(html: &[u8]) -> anyhow::Result<(String, Vec<String>, String)> {
    struct Context {
        stack: Vec<Frame>,
        best_text: Option<String>,
        best_score: f32,
        skip_depth: u32,
        urls: Vec<String>,
        title: String,
        in_title: bool,
    }

    let ctx = Rc::new(RefCell::new(Context {
        stack: Vec::with_capacity(64),
        best_text: None,
        best_score: 0.0,
        skip_depth: 0,
        urls: Vec::new(),
        title: String::new(),
        in_title: false,
    }));

    let ctx_open = ctx.clone();
    let ctx_text = ctx.clone();
    let ctx_close = ctx.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("*", move |el| {
                    let tag_name = el.tag_name();
                    let is_container_skip = is_non_content_container(&tag_name);
                    let is_leaf_skip = is_non_content_leaf(&tag_name);
                    let is_void = is_void_element(&tag_name);

                    // Collect URLs from anchor tags
                    if tag_name == "a" {
                        if let Some(href) = el.get_attribute("href") {
                            if !href.is_empty() {
                                ctx_open.borrow_mut().urls.push(href);
                            }
                        }
                    }

                    // Track title element (don't push a frame, just track state)
                    let is_title = tag_name == "title";
                    if is_title {
                        ctx_open.borrow_mut().in_title = true;
                    }

                    let (pushed_frame, incremented_skip) = {
                        let mut c = ctx_open.borrow_mut();
                        if is_title {
                            // Title element: don't skip, don't push frame, just track
                            // This needs to come before skip_depth check since title is in <head>
                            (false, false)
                        } else if c.skip_depth > 0 && !is_void {
                            c.skip_depth += 1;
                            (false, true)
                        } else if c.skip_depth > 0 && is_void {
                            (false, false)
                        } else if is_container_skip {
                            c.skip_depth += 1;
                            (false, true)
                        } else if is_leaf_skip || is_void {
                            (false, false)
                        } else {
                            let id = el.get_attribute("id");
                            let class = el.get_attribute("class");
                            let frame = Frame::new(&tag_name, id.as_deref(), class.as_deref());
                            c.stack.push(frame);
                            (true, false)
                        }
                    };

                    if let Some(handlers) = el.end_tag_handlers() {
                        let ctx_close = ctx_close.clone();
                        let handler: EndTagHandler<'static> = Box::new(move |_| {
                            let mut c = ctx_close.borrow_mut();

                            // Handle title closing
                            if is_title {
                                c.in_title = false;
                                return Ok(());
                            }

                            if incremented_skip {
                                c.skip_depth -= 1;
                                return Ok(());
                            }

                            if !pushed_frame {
                                return Ok(());
                            }

                            if let Some(mut frame) = c.stack.pop() {
                                // Weight text length more heavily - this is the key signal
                                let text_score = (frame.text_len as f32).sqrt();
                                let mut final_score =
                                    frame.score + text_score + frame.commas as f32;

                                if frame.text_len > 0 {
                                    let density =
                                        frame.link_text_len as f32 / frame.text_len as f32;
                                    if density > 0.5 {
                                        final_score *= 1.0 - density;
                                    }
                                }

                                // Check if this is the best candidate
                                // Any element with enough quality text can compete
                                let tag_is_candidate = matches!(
                                    frame.tag,
                                    Some(
                                        TagType::Article
                                            | TagType::Div
                                            | TagType::P
                                            | TagType::Other
                                    )
                                );

                                // Minimum text length to filter out tiny navigation snippets
                                let min_text_len = 100;
                                let has_enough_text = frame.text_len >= min_text_len;

                                // Check for garbage content like "a a a b b b c c c"
                                // Real content has average word length > 2
                                let word_count = frame.text.split_whitespace().count();
                                let avg_word_len = if word_count > 0 {
                                    frame.text.len() / word_count
                                } else {
                                    0
                                };
                                let has_real_words = avg_word_len > 2;

                                let is_candidate = tag_is_candidate
                                    && has_enough_text
                                    && has_real_words
                                    && final_score > c.best_score
                                    && !frame.text.is_empty();

                                // Always bubble stats and text to parent so parents can compete
                                if let Some(parent) = c.stack.last_mut() {
                                    // Bubble raw stats - parent calculates its own score
                                    parent.text_len += frame.text_len;
                                    parent.link_text_len += frame.link_text_len;
                                    parent.commas += frame.commas;
                                    parent.append_text(&frame.text);
                                }

                                if is_candidate {
                                    c.best_score = final_score;
                                    c.best_text = Some(frame.text);
                                }
                            }
                            Ok(())
                        });
                        handlers.push(handler);
                    }

                    Ok(())
                }),
                text!("*", move |chunk| {
                    let text = chunk.as_str();
                    if text.trim().is_empty() {
                        return Ok(());
                    }

                    let mut c = ctx_text.borrow_mut();

                    // Capture title text
                    if c.in_title {
                        if !c.title.is_empty() {
                            c.title.push(' ');
                        }
                        c.title.push_str(text.trim());
                        return Ok(());
                    }

                    if c.skip_depth > 0 {
                        return Ok(());
                    }

                    if let Some(frame) = c.stack.last_mut() {
                        let len = text.len() as u32;
                        let commas = text.bytes().filter(|&b| b == b',').count() as u32;

                        frame.text_len += len;
                        frame.commas += commas;
                        frame.append_text(text);

                        if let Some(TagType::A) = frame.tag {
                            frame.link_text_len += len;
                        }
                    }
                    Ok(())
                }),
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );

    rewriter.write(html)?;
    rewriter.end()?;

    let final_ctx = Rc::into_inner(ctx)
        .expect("Rc should have single owner")
        .into_inner();

    match final_ctx.best_text {
        Some(text) => Ok((text, final_ctx.urls, final_ctx.title)),
        None => anyhow::bail!("No main content found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_finds_article_by_id() {
        let html = r#"
            <html>
                <body>
                    <header>Navigation stuff</header>
                    <div id="sidebar">Links, ads, widgets</div>
                    <article id="main-content">
                        <p>This is the main article content with lots of text.
                        It has multiple sentences, and commas too.
                        The article discusses important topics that readers care about.
                        More content here to make this the clear winner.</p>
                        <p>Another paragraph with even more valuable content for readers.</p>
                    </article>
                    <footer>Copyright 2025</footer>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok(), "Should find a main content element");
        let (text, _urls, _title) = result.unwrap();
        // Should extract the article text
        assert!(
            text.contains("main article content") && text.contains("multiple sentences"),
            "Expected article text, got: {}",
            text
        );
    }

    #[test]
    fn test_finds_content_by_class() {
        let html = r#"
            <html>
                <body>
                    <nav class="menu">Home | About | Contact</nav>
                    <div class="content article-body">
                        <p>This is the real content of the page. It contains
                        important information that readers want to see.
                        Multiple sentences with commas, periods, and details.</p>
                        <p>More paragraphs with substantial text content here.</p>
                    </div>
                    <aside class="sidebar">Related links</aside>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok(), "Should find content element");
        let (text, _urls, _title) = result.unwrap();
        assert!(
            text.contains("real content of the page"),
            "Expected content text, got: {}",
            text
        );
    }

    #[test]
    fn test_penalizes_link_heavy_content() {
        let html = r#"
            <html>
                <body>
                    <div id="navigation">
                        <a href="/1">Link 1</a>
                        <a href="/2">Link 2</a>
                        <a href="/3">Link 3</a>
                        <a href="/4">Link 4</a>
                    </div>
                    <div id="article">
                        <p>This is actual article text without many links.
                        It contains substantial content that readers want to read.
                        Multiple paragraphs of real content here.</p>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (text, _urls, _title) = result.unwrap();
        // Should prefer article over navigation due to link density penalty
        assert!(
            text.contains("actual article text"),
            "Should prefer article over nav, got: {}",
            text
        );
    }

    #[test]
    fn test_negative_class_penalty() {
        let html = r#"
            <html>
                <body>
                    <div class="sidebar widget">
                        <p>Some sidebar content with text.</p>
                    </div>
                    <div class="post entry">
                        <p>This is the main post content with more text,
                        longer sentences, and better structure overall.</p>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (text, _urls, _title) = result.unwrap();
        // Should prefer post/entry over sidebar/widget
        assert!(
            text.contains("main post content"),
            "Should prefer post over sidebar, got: {}",
            text
        );
    }

    #[test]
    fn test_empty_html() {
        let html = b"<html><body></body></html>";
        let result = find_main_content(html);
        // Empty page should return an error
        assert!(result.is_err());
    }

    #[test]
    fn test_minimal_content() {
        // Minimal content without id/class won't be identified as main content
        let html = b"<html><body><p>Hello world</p></body></html>";
        let result = find_main_content(html);
        assert!(result.is_err());
    }

    #[test]
    fn test_head_content_ignored() {
        let html = r#"
            <html>
                <head>
                    <title>Page Title With Keywords</title>
                    <meta name="description" content="This has lots of text that should not count">
                    <style>body { content: "lots of text here too, and commas, everywhere"; }</style>
                    <script>var text = "JavaScript code with commas, and text, everywhere";</script>
                </head>
                <body>
                    <div id="content">
                        <p>This is the actual body content that should be found.
                        It has multiple sentences with commas, periods, and details.
                        The content discusses important topics that readers care about.
                        More substantial text here to make this clearly the main content area.</p>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok(), "Should find content element");
        let (text, _urls, _title) = result.unwrap();
        // Should find the body content, not include head content
        assert!(
            text.contains("actual body content") && !text.contains("Page Title"),
            "Should identify body content without head, got: {}",
            text
        );
    }

    #[test]
    fn test_script_style_outside_head_ignored() {
        // Some pages have script/style in body (bad practice but common)
        let html = r#"
            <html>
                <body>
                    <script>var x = "lots of text, commas, and content here";</script>
                    <style>.foo { content: "more text, with commas"; }</style>
                    <div id="article">
                        <p>Real article content here with substantial text that matters.
                        This has multiple sentences with commas, periods, and details.
                        The content discusses important topics that readers want to read about.</p>
                    </div>
                    <noscript>JavaScript is disabled, lots of text here too</noscript>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (text, _urls, _title) = result.unwrap();
        assert!(
            text.contains("Real article content") && !text.contains("var x"),
            "Should find article text without script content, got: {}",
            text
        );
    }

    #[test]
    fn test_link_heavy_article_like_wikipedia() {
        // Wikipedia-style article: lots of links but also substantial prose
        let html = r#"
            <html>
                <body>
                    <nav id="navigation">
                        <a href="/">Home</a>
                        <a href="/about">About</a>
                        <a href="/contact">Contact</a>
                    </nav>
                    <div id="article">
                        <p>
                            <a href="/wiki/Rust">Rust</a> is a 
                            <a href="/wiki/Programming_language">programming language</a> 
                            designed for performance and safety, especially safe 
                            <a href="/wiki/Concurrency">concurrency</a>. Rust is 
                            syntactically similar to C++, but provides memory safety 
                            without using garbage collection.
                        </p>
                        <p>
                            The Rust Foundation was established in 2021 to support 
                            the development of the language and its ecosystem.
                        </p>
                    </div>
                    <div id="sidebar">
                        <a href="/related1">Related</a>
                        <a href="/related2">Links</a>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok(), "Should find the article content");
        let (text, _urls, _title) = result.unwrap();
        // Should extract the article prose
        assert!(
            text.contains("programming language") && text.contains("Rust Foundation"),
            "Should extract article text, got: {}",
            text
        );
    }

    #[test]
    fn test_form_elements_ignored() {
        let html = r#"
            <html>
                <body>
                    <form id="search-form">
                        <input type="text" placeholder="Search with lots of text here">
                        <select>
                            <option>Option one with text</option>
                            <option>Option two with text</option>
                            <option>Option three with text</option>
                        </select>
                        <button>Submit button text</button>
                    </form>
                    <div id="content">
                        <p>This is the actual page content that matters.
                        It contains substantial text with multiple sentences, commas, and details.
                        The article discusses important topics that readers want to read about.
                        More content here to establish this as the main readable area.</p>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (text, _urls, _title) = result.unwrap();
        // Should NOT contain form content
        assert!(
            text.contains("actual page content") && !text.contains("Option one"),
            "Should find content without form text, got: {}",
            text
        );
    }

    #[test]
    fn test_extracts_all_urls() {
        let html = r#"
            <html>
                <body>
                    <div id="content">
                        <p>Check out <a href="https://example.com">this site</a> and
                        <a href="https://another.com/page">another page</a> for more information.
                        This article contains substantial text with multiple sentences.
                        The content discusses important topics that readers care about.</p>
                        <a href="/relative/path">Relative link</a>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (_text, urls, _title) = result.unwrap();
        assert_eq!(urls.len(), 3, "Should find 3 URLs, got: {:?}", urls);
        assert!(urls.contains(&"https://example.com".to_string()));
        assert!(urls.contains(&"https://another.com/page".to_string()));
        assert!(urls.contains(&"/relative/path".to_string()));
    }

    #[test]
    fn test_extracts_urls_preserves_order() {
        let html = r#"
            <html>
                <body>
                    <div id="content">
                        <p>This article has multiple links that should be extracted in order.
                        It contains substantial text content with commas, periods, and details.</p>
                        <a href="/first">First</a>
                        <a href="/second">Second</a>
                        <a href="/third">Third</a>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (_text, urls, _title) = result.unwrap();
        assert_eq!(urls, vec!["/first", "/second", "/third"]);
    }

    #[test]
    fn test_no_urls_on_page_without_links() {
        let html = r#"
            <html>
                <body>
                    <div id="content">
                        <p>This page has no links at all, just plain text content here.
                        It contains substantial text with multiple sentences and paragraphs.
                        The content discusses important topics that readers care about.</p>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (_text, urls, _title) = result.unwrap();
        assert!(urls.is_empty(), "Should have no URLs, got: {:?}", urls);
    }

    #[test]
    fn test_ignores_empty_href() {
        let html = r#"
            <html>
                <body>
                    <div id="content">
                        <p>This page has various link types, some valid and some not.
                        The content extraction should find all valid hrefs while ignoring empty ones.
                        Multiple sentences with commas, periods, and substantial text.</p>
                        <a href="">Empty href</a>
                        <a href="https://valid.com">Valid link</a>
                        <a>No href at all</a>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (_text, urls, _title) = result.unwrap();
        assert_eq!(urls.len(), 1, "Should find 1 valid URL, got: {:?}", urls);
        assert_eq!(urls[0], "https://valid.com");
    }

    #[test]
    fn test_extracts_urls_from_nav_and_content() {
        // URLs should be collected from the entire page, not just main content
        let html = r#"
            <html>
                <body>
                    <nav>
                        <a href="/nav1">Nav 1</a>
                        <a href="/nav2">Nav 2</a>
                    </nav>
                    <div id="article">
                        <p>Article content with <a href="/article-link">a link</a>.
                        This article has substantial text with multiple sentences.
                        The content discusses important topics that readers care about.</p>
                    </div>
                    <footer>
                        <a href="/footer">Footer link</a>
                    </footer>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (_text, urls, _title) = result.unwrap();
        assert_eq!(urls.len(), 4, "Should find all 4 URLs, got: {:?}", urls);
        assert!(urls.contains(&"/nav1".to_string()));
        assert!(urls.contains(&"/nav2".to_string()));
        assert!(urls.contains(&"/article-link".to_string()));
        assert!(urls.contains(&"/footer".to_string()));
    }

    #[test]
    fn test_handles_various_url_formats() {
        let html = r##"
            <html>
                <body>
                    <div id="content">
                        <p>This page demonstrates various URL formats that can appear in links.
                        The extraction should handle all of them correctly, including protocols,
                        relative paths, anchors, and special schemes like mailto and javascript.</p>
                        <a href="https://secure.example.com">HTTPS</a>
                        <a href="http://insecure.example.com">HTTP</a>
                        <a href="//protocol-relative.com">Protocol relative</a>
                        <a href="/absolute/path">Absolute path</a>
                        <a href="relative/path">Relative path</a>
                        <a href="#anchor">Anchor</a>
                        <a href="mailto:test@example.com">Email</a>
                        <a href="javascript:void(0)">JavaScript</a>
                    </div>
                </body>
            </html>
        "##;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (_text, urls, _title) = result.unwrap();
        assert_eq!(
            urls.len(),
            8,
            "Should collect all href values, got: {:?}",
            urls
        );
    }

    #[test]
    fn test_extracts_title() {
        let html = r#"
            <html>
                <head>
                    <title>My Page Title</title>
                </head>
                <body>
                    <div id="content">
                        <p>This is the actual body content that should be found.
                        It has multiple sentences with commas, periods, and details.
                        The content discusses important topics that readers care about.</p>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (_text, _urls, title) = result.unwrap();
        assert_eq!(
            title, "My Page Title",
            "Should extract title, got: {}",
            title
        );
    }

    #[test]
    fn test_semantic_tags_without_id_or_class() {
        // Semantic tags like <article>, <main>, <section> should be candidates
        // even without id or class attributes
        let html = r#"
            <html>
                <body>
                    <header>Site Header</header>
                    <main>
                        <article>
                            <p>This is the main article content with lots of text.
                            It has multiple sentences, and commas too.
                            The article discusses important topics that readers care about.
                            More content here to make this the clear winner.</p>
                            <p>Another paragraph with even more valuable content for readers.</p>
                        </article>
                    </main>
                    <footer>Copyright 2025</footer>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(
            result.is_ok(),
            "Should find content in semantic tags without id/class"
        );
        let (text, _urls, _title) = result.unwrap();
        assert!(
            text.contains("main article content"),
            "Expected article text, got: {}",
            text
        );
    }

    #[test]
    fn test_large_content_beats_small_element_with_good_class() {
        // A large block of text should beat a tiny element that just happens
        // to have a "content" class
        let html = r#"
            <html>
                <body>
                    <div class="content">Reset password</div>
                    <article>
                        <p>This is the actual main content of the page.
                        It contains substantial text across multiple sentences.
                        The article discusses important topics in detail.
                        More content here with commas, periods, and lots of words.
                        Even more paragraphs of valuable information for the reader.
                        This should clearly win over the tiny reset password div.</p>
                    </article>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (text, _urls, _title) = result.unwrap();
        assert!(
            text.contains("actual main content"),
            "Large content should beat small element, got: {}",
            text
        );
        assert!(
            !text.contains("Reset password"),
            "Should not select the tiny reset password div"
        );
    }

    #[test]
    fn test_wiki_navigation_rejected() {
        // Wiki pages often have navigation text that should be rejected
        let html = r#"
            <html>
                <body>
                    <div id="mw-navigation">
                        <span>Jump to navigation Jump to search</span>
                    </div>
                    <div id="content" class="mw-body">
                        <p>This is the actual wiki article content with substantial text.
                        It contains multiple paragraphs explaining the topic in detail.
                        The article has commas, periods, and proper structure.
                        More content here to establish this as the main readable area.</p>
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (text, _urls, _title) = result.unwrap();
        assert!(
            text.contains("actual wiki article"),
            "Should find article content, got: {}",
            text
        );
        assert!(
            !text.contains("Jump to navigation"),
            "Should not select navigation text"
        );
    }

    #[test]
    fn test_rejects_single_letter_garbage() {
        // Some pages have alphabetical indexes like "a a a b b b c c c"
        // These should be rejected due to low average word length
        let html = r#"
            <html>
                <body>
                    <div id="index" class="content">
                        a a a a a a b b b c c c d d d d d d d d d d d d d d d d e e e e e e f f f f f f f g g g h h h h h h h i i i i i i j j j k k k k l m m m n n n o o o o o o o o p p p q q q q q q q r r r r r r s s s s s t t u u u u u u v v v v v v w w w w x x y y z
                    </div>
                    <article>
                        <p>This is the actual article content with real words and sentences.
                        It contains substantial text discussing important topics in detail.
                        The article has proper structure with commas, periods, and paragraphs.</p>
                    </article>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (text, _urls, _title) = result.unwrap();
        assert!(
            text.contains("actual article content"),
            "Should find article, not index garbage, got: {}",
            text
        );
        assert!(
            !text.contains("a a a"),
            "Should not select single-letter garbage"
        );
    }
}
