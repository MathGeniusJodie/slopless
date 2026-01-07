use html_escape::decode_html_entities;
use lol_html::{element, text, EndTagHandler, HtmlRewriter, Settings};
use regex::Regex;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;
use url::Url;

// -----------------------------------------------------------------------------
// Static Configuration (Compiled Once)
// -----------------------------------------------------------------------------

/// Maximum nesting depth for element tracking (prevents stack exhaustion from malformed HTML)
const MAX_ELEMENT_STACK_DEPTH: usize = 256;

// Regex patterns for scoring element class/id attributes

/// Patterns that are very unlikely to be main content (heavily penalize)
/// Note: RE_MAYBE patterns override these (prevent the -50 penalty)
static RE_UNLIKELY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)combx|community|disqus|extra|remark|rss|share|shoutbox|skyscraper|ad-break|agegate|popup"
    ).unwrap()
});

/// Ambiguous patterns - if matched, prevents RE_UNLIKELY penalty
static RE_MAYBE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)and|column|shadow").unwrap());

/// Positive signals that suggest main content
static RE_POSITIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)article|body|content|entry|hentry|main|page|pagination|post|text|blog|story")
        .unwrap()
});

/// Negative signals that suggest non-content elements
static RE_NEGATIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)hidden|banner|comment|com-|contact|foot|footer|footnote|header|masthead|media|meta|menu|nav|outbrain|promo|related|scroll|sidebar|sponsor|shopping|tags|tool|widget|namespace|action|catlinks|toc|printfooter|jump-to|siteSub|contentSub"
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

/// A stack frame representing an open HTML element being scored
#[derive(Debug)]
struct ElementFrame {
    tag_type: TagType,
    base_score: f32,          // Initial score from tag type + class/id
    text_len: u32,            // Total character count
    link_text_len: u32,       // Character count inside <a> tags
    comma_count: u32,         // Number of commas (heuristic for real sentences)
    accumulated_text: String, // Actual text content for this element
}

impl ElementFrame {
    /// Create a new frame for an HTML element with initial scoring based on tag type and attributes
    fn new(tag_name: &str, id: Option<&str>, class: Option<&str>) -> Self {
        let tag_type = Self::classify_tag(tag_name);
        let base_score = Self::calculate_base_score(tag_type, id, class);

        Self {
            tag_type,
            base_score,
            text_len: 0,
            link_text_len: 0,
            comma_count: 0,
            accumulated_text: String::new(),
        }
    }

    /// Map HTML tag name to internal tag type
    fn classify_tag(tag_name: &str) -> TagType {
        match tag_name {
            "div" => TagType::Div,
            "p" => TagType::P,
            "a" => TagType::A,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => TagType::Header,
            "ul" | "ol" | "dl" => TagType::List,
            "article" | "main" | "section" => TagType::Article,
            _ => TagType::Other,
        }
    }

    /// Calculate initial score based on tag type and class/id attributes
    fn calculate_base_score(tag_type: TagType, id: Option<&str>, class: Option<&str>) -> f32 {
        // Start with tag-based score
        let mut score = match tag_type {
            TagType::Article => 30.0, // Semantic HTML5 content tags
            TagType::Div => 5.0,
            TagType::P => 10.0,
            TagType::Header => -5.0, // Headers aren't main content
            TagType::List => -3.0,   // Lists often navigation
            _ => 0.0,
        };

        // Adjust based on class/id attributes
        if let Some(id_value) = id {
            score += Self::score_attribute(id_value);
        }
        if let Some(class_value) = class {
            score += Self::score_attribute(class_value);
        }

        score
    }

    /// Score a class or id attribute value based on regex patterns
    fn score_attribute(attr_value: &str) -> f32 {
        // Heavily penalize obvious non-content patterns
        if RE_UNLIKELY.is_match(attr_value) && !RE_MAYBE.is_match(attr_value) {
            return -50.0;
        }

        // Reward positive content signals
        if RE_POSITIVE.is_match(attr_value) {
            return 25.0;
        }

        // Penalize negative signals
        if RE_NEGATIVE.is_match(attr_value) {
            return -25.0;
        }

        0.0
    }

    /// Append text content, decoding HTML entities and normalizing whitespace
    fn append_text(&mut self, text: &str) {
        let decoded = decode_html_entities(text);
        let mut words = decoded.split_whitespace();

        let Some(first_word) = words.next() else {
            return; // Empty after whitespace normalization
        };

        // Add space separator if needed
        if !self.accumulated_text.is_empty() && !self.accumulated_text.ends_with(' ') {
            self.accumulated_text.push(' ');
        }

        self.accumulated_text.push_str(first_word);
        for word in words {
            self.accumulated_text.push(' ');
            self.accumulated_text.push_str(word);
        }
    }

    /// Calculate final score including text-based heuristics
    fn calculate_final_score(&self) -> f32 {
        // Start with base score from tag/attributes
        let mut final_score = self.base_score;

        // Reward text length (use sqrt to avoid over-weighting very long elements)
        let text_score = (self.text_len as f32).sqrt();
        final_score += text_score;

        // Reward comma count (real sentences have punctuation)
        final_score += self.comma_count as f32;

        // Penalize high link density (navigation vs content)
        if self.text_len > 0 {
            let link_density = self.link_text_len as f32 / self.text_len as f32;
            if link_density > 0.5 {
                final_score *= 1.0 - link_density;
            }
        }

        final_score
    }

    /// Check if this element is a viable candidate for main content
    fn is_viable_candidate(&self) -> bool {
        // Must be a content-bearing tag type
        let is_content_tag = matches!(
            self.tag_type,
            TagType::Article | TagType::Div | TagType::P | TagType::Other
        );

        // Must have enough text (filter out tiny snippets)
        const MIN_TEXT_LENGTH: u32 = 100;
        let has_enough_text = self.text_len >= MIN_TEXT_LENGTH;

        // Must have real words (filter out "a a a b b b" garbage)
        let word_count = self.accumulated_text.split_whitespace().count();
        let avg_word_length = self
            .accumulated_text
            .len()
            .checked_div(word_count)
            .unwrap_or(0);
        let has_real_words = avg_word_length > 2;

        is_content_tag && has_enough_text && has_real_words
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

/// Non-content leaf tags (excludes void elements which are handled separately)
fn is_non_content_leaf(tag: &str) -> bool {
    matches!(
        tag,
        "title"
            | "textarea"
            | "select"
            | "option"
            | "optgroup"
            | "button"
            | "map"
            | "datalist"
            | "output"
            | "progress"
            | "meter"
    )
}

/// Parsing context shared across HTML rewriter callbacks
struct ParsingContext {
    element_stack: Vec<ElementFrame>,
    best_content: Option<String>,
    best_content_score: f32,
    skip_depth: u32,
    anchor_depth: u32, // Track nesting inside <a> tags for link density
    page_title: String,
    currently_in_title: bool,
    canonical_url: Option<String>,
}

// -----------------------------------------------------------------------------
// Main Function (Single Pass)
// -----------------------------------------------------------------------------

/// Find and extract the main content as plain text.
/// Returns: (content_text, title, url)
/// The URL will be the canonical URL if found, otherwise the input URL.
/// Returns an error if no suitable content element was found.
pub fn find_main_content(html: &[u8], url: &str) -> anyhow::Result<(String, String, String)> {
    let parsing_context = Rc::new(RefCell::new(ParsingContext {
        element_stack: Vec::with_capacity(64),
        best_content: None,
        best_content_score: 0.0,
        skip_depth: 0,
        anchor_depth: 0,
        page_title: String::new(),
        currently_in_title: false,
        canonical_url: None,
    }));

    let context_for_open = parsing_context.clone();
    let context_for_text = parsing_context.clone();
    let context_for_close = parsing_context.clone();
    let context_for_link = parsing_context.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                // Extract canonical URL from <link rel="canonical" href="...">
                element!("link[rel=canonical]", move |el| {
                    if let Some(href) = el.get_attribute("href") {
                        if !href.is_empty() {
                            context_for_link.borrow_mut().canonical_url = Some(href);
                        }
                    }
                    Ok(())
                }),
                element!("*", move |el| {
                    let tag_name = el.tag_name();
                    let is_container_skip = is_non_content_container(&tag_name);
                    let is_leaf_skip = is_non_content_leaf(&tag_name);
                    let is_void = is_void_element(&tag_name);

                    // Track <title> element (special case: in <head>, but we want its text)
                    let is_title = tag_name == "title";
                    if is_title {
                        context_for_open.borrow_mut().currently_in_title = true;
                    }

                    // Determine what to do with this element
                    let is_anchor = tag_name == "a";
                    let (pushed_element_frame, incremented_skip_depth) = {
                        let mut context = context_for_open.borrow_mut();

                        if is_title {
                            // Title is in <head> which we skip, but we want title text
                            // Don't push frame, don't increment skip depth, just track state
                            (false, false)
                        } else if context.skip_depth > 0 && !is_void {
                            // Inside a skipped container: increment depth counter
                            context.skip_depth += 1;
                            (false, true)
                        } else if context.skip_depth > 0 && is_void {
                            // Void element inside skipped container: ignore
                            (false, false)
                        } else if is_container_skip {
                            // Start skipping a non-content container (<script>, <style>, etc.)
                            context.skip_depth += 1;
                            (false, true)
                        } else if is_leaf_skip || is_void {
                            // Non-content leaf/void element: ignore
                            (false, false)
                        } else if context.element_stack.len() >= MAX_ELEMENT_STACK_DEPTH {
                            // Stack depth limit reached: skip deeply nested elements
                            (false, false)
                        } else {
                            // Normal content element: create frame and push to stack
                            let id = el.get_attribute("id");
                            let class = el.get_attribute("class");
                            let frame =
                                ElementFrame::new(&tag_name, id.as_deref(), class.as_deref());
                            context.element_stack.push(frame);
                            if is_anchor {
                                context.anchor_depth += 1;
                            }
                            (true, false)
                        }
                    };

                    // Register end tag handler to process the element when it closes
                    if let Some(handlers) = el.end_tag_handlers() {
                        let context_for_close_handler = context_for_close.clone();
                        let handler: EndTagHandler<'static> = Box::new(move |_| {
                            let mut context = context_for_close_handler.borrow_mut();

                            // Handle </title> closing
                            if is_title {
                                context.currently_in_title = false;
                                return Ok(());
                            }

                            // Handle closing skipped container
                            if incremented_skip_depth {
                                context.skip_depth -= 1;
                                return Ok(());
                            }

                            // If we didn't push a frame, nothing to do
                            if !pushed_element_frame {
                                return Ok(());
                            }

                            // Decrement anchor depth if this was an <a> tag
                            if is_anchor {
                                context.anchor_depth -= 1;
                            }

                            // Pop the frame for this element and evaluate it
                            if let Some(element_frame) = context.element_stack.pop() {
                                // Calculate final score for this element
                                let final_score = element_frame.calculate_final_score();

                                // Check if this is a viable candidate for main content
                                let is_candidate = element_frame.is_viable_candidate()
                                    && final_score > context.best_content_score;

                                // Always bubble stats to parent (so parent elements can compete)
                                if let Some(parent_frame) = context.element_stack.last_mut() {
                                    parent_frame.text_len += element_frame.text_len;
                                    parent_frame.link_text_len += element_frame.link_text_len;
                                    parent_frame.comma_count += element_frame.comma_count;
                                    parent_frame.append_text(&element_frame.accumulated_text);
                                }

                                // Update best candidate if this is better
                                if is_candidate {
                                    context.best_content_score = final_score;
                                    context.best_content = Some(element_frame.accumulated_text);
                                }
                            }
                            Ok(())
                        });
                        handlers.push(handler);
                    }

                    Ok(())
                }),
                text!("*", move |text_chunk| {
                    let text = text_chunk.as_str();
                    if text.trim().is_empty() {
                        return Ok(()); // Skip whitespace-only text
                    }

                    let mut context = context_for_text.borrow_mut();

                    // Special case: capture <title> text
                    if context.currently_in_title {
                        let decoded = decode_html_entities(text.trim());
                        if !decoded.is_empty() {
                            if !context.page_title.is_empty() {
                                context.page_title.push(' ');
                            }
                            context.page_title.push_str(&decoded);
                        }
                        return Ok(());
                    }

                    // Skip text inside non-content containers
                    if context.skip_depth > 0 {
                        return Ok(());
                    }

                    // Add text to current element frame
                    let inside_anchor = context.anchor_depth > 0;
                    if let Some(current_frame) = context.element_stack.last_mut() {
                        let text_length = u32::try_from(text.len()).unwrap_or(u32::MAX);
                        let comma_count =
                            u32::try_from(text.bytes().filter(|&b| b == b',').count())
                                .unwrap_or(u32::MAX);

                        current_frame.text_len += text_length;
                        current_frame.comma_count += comma_count;
                        current_frame.append_text(text);

                        // Track link text separately (for link density calculation)
                        // Use anchor_depth to handle nested elements inside <a> tags
                        if inside_anchor {
                            current_frame.link_text_len += text_length;
                        }
                    }
                    Ok(())
                }),
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );

    // Process the HTML
    rewriter.write(html)?;
    rewriter.end()?;

    // Extract final results
    let final_context = Rc::into_inner(parsing_context)
        .expect("Rc should have single owner")
        .into_inner();

    let Some(content_text) = final_context.best_content else {
        anyhow::bail!("No main content found")
    };

    // Parse and normalize the base URL
    let base_url = Url::parse(url)?;

    // Resolve canonical URL (may be relative) or use base URL
    let resolved_url = match final_context.canonical_url {
        Some(canonical) => base_url.join(&canonical)?,
        None => base_url,
    };

    // Normalize: strip trailing slash (but not for root paths like "https://example.com/")
    let mut url_str = resolved_url.to_string();
    if url_str.ends_with('/') && resolved_url.path().len() > 1 {
        url_str.pop();
    }

    // Normalize unicode (NFKC)
    let content: String = content_text.nfkc().collect();
    let title: String = final_context.page_title.nfkc().collect();

    Ok((content, title, url_str))
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok(), "Should find a main content element");
        let (text, _title, _canonical) = result.unwrap();
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok(), "Should find content element");
        let (text, _title, _canonical) = result.unwrap();
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _canonical) = result.unwrap();
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _canonical) = result.unwrap();
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
        let result = find_main_content(html, "https://example.com/test");
        // Empty page should return an error
        assert!(result.is_err());
    }

    #[test]
    fn test_minimal_content() {
        // Minimal content without id/class won't be identified as main content
        let html = b"<html><body><p>Hello world</p></body></html>";
        let result = find_main_content(html, "https://example.com/test");
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok(), "Should find content element");
        let (text, _title, _canonical) = result.unwrap();
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _canonical) = result.unwrap();
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok(), "Should find the article content");
        let (text, _title, _canonical) = result.unwrap();
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _canonical) = result.unwrap();
        // Should NOT contain form content
        assert!(
            text.contains("actual page content") && !text.contains("Option one"),
            "Should find content without form text, got: {}",
            text
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (_text, title, _canonical) = result.unwrap();
        assert_eq!(
            title, "My Page Title",
            "Should extract title, got: {}",
            title
        );
    }

    #[test]
    fn test_extracts_canonical_url() {
        let html = r#"
            <html>
                <head>
                    <title>Test Page</title>
                    <link rel="canonical" href="https://example.com/canonical-page">
                </head>
                <body>
                    <article>
                        <p>This is the main article content with lots of text.
                        It has multiple sentences, and commas too.
                        The article discusses important topics that readers care about.</p>
                    </article>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/original");
        assert!(result.is_ok());
        let (_text, _title, url) = result.unwrap();
        assert_eq!(
            url, "https://example.com/canonical-page",
            "Should return canonical URL when present"
        );
    }

    #[test]
    fn test_no_canonical_returns_input_url() {
        let html = r#"
            <html>
                <head><title>Test Page</title></head>
                <body>
                    <article>
                        <p>This is the main article content with lots of text.
                        It has multiple sentences, and commas too.
                        The article discusses important topics that readers care about.</p>
                    </article>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/original");
        assert!(result.is_ok());
        let (_text, _title, url) = result.unwrap();
        assert_eq!(
            url, "https://example.com/original",
            "Should return input URL when no canonical"
        );
    }

    #[test]
    fn test_relative_canonical_resolved() {
        let html = r#"
            <html>
                <head>
                    <title>Test Page</title>
                    <link rel="canonical" href="/canonical-page">
                </head>
                <body>
                    <article>
                        <p>This is the main article content with lots of text.
                        It has multiple sentences, and commas too.
                        The article discusses important topics that readers care about.</p>
                    </article>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/some/path/page.html");
        assert!(result.is_ok());
        let (_text, _title, url) = result.unwrap();
        assert_eq!(
            url, "https://example.com/canonical-page",
            "Should resolve relative canonical against base URL"
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(
            result.is_ok(),
            "Should find content in semantic tags without id/class"
        );
        let (text, _title, _canonical) = result.unwrap();
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _canonical) = result.unwrap();
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _canonical) = result.unwrap();
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
    fn test_nested_link_text_counted_for_density() {
        // Text nested inside <a> tags should count toward link density
        // even when wrapped in other elements like <span>.
        // Section-a: MORE text but 100% link density (all in nested links)
        // Section-b: LESS text but 0% link density
        // If link density works, section-b should win despite being shorter.
        // If link density is broken, section-a wins due to longer text.
        let html = r#"
            <html>
                <body>
                    <div class="section-a">
                        <a href="/1"><span>These are words inside a link with span wrapper element</span></a>
                        <a href="/2"><em><strong>More words here also inside deeply nested link tags</strong></em></a>
                        <a href="/3"><span>Even more words in another nested link wrapper here</span></a>
                        <a href="/4"><span>And yet another link with nested span containing text</span></a>
                        <a href="/5"><span>Plus one more nested link to make this section longer</span></a>
                    </div>
                    <div class="section-b">
                        Plain text without any links at all here.
                        More plain text also without links.
                    </div>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _canonical) = result.unwrap();
        // section-a is LONGER but has 100% link density -> score *= ~0
        // section-b is SHORTER but has 0% link density -> no penalty
        // section-b should win despite being shorter
        assert!(
            text.contains("Plain text without"),
            "Shorter 0% link-dense section should beat longer 100% link-dense section, got: {}",
            text
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

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _canonical) = result.unwrap();
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

    #[test]
    fn test_url_normalization_trailing_slash() {
        let html = r#"
            <html>
                <head><title>Test</title></head>
                <body>
                    <article>
                        <p>This is the main article content with lots of text.
                        It has multiple sentences, and commas too.
                        The article discusses important topics that readers care about.</p>
                    </article>
                </body>
            </html>
        "#;

        // Trailing slash should be stripped
        let result = find_main_content(html.as_bytes(), "https://example.com/page/");
        let (_text, _title, url) = result.unwrap();
        assert_eq!(
            url, "https://example.com/page",
            "Should strip trailing slash"
        );

        // Root path should keep its slash
        let result = find_main_content(html.as_bytes(), "https://example.com/");
        let (_text, _title, url) = result.unwrap();
        assert_eq!(
            url, "https://example.com/",
            "Should keep trailing slash for root"
        );

        // Host should be lowercased
        let result = find_main_content(html.as_bytes(), "https://EXAMPLE.COM/page");
        let (_text, _title, url) = result.unwrap();
        assert_eq!(url, "https://example.com/page", "Should lowercase host");

        // Path case should be preserved (important for base64 params etc)
        let result = find_main_content(html.as_bytes(), "https://example.com/Page/ABC123");
        let (_text, _title, url) = result.unwrap();
        assert_eq!(
            url, "https://example.com/Page/ABC123",
            "Should preserve path case"
        );
    }

    // =========================================================================
    // Unit tests for ElementFrame helper methods
    // =========================================================================

    #[test]
    fn test_classify_tag_div() {
        assert_eq!(ElementFrame::classify_tag("div"), TagType::Div);
    }

    #[test]
    fn test_classify_tag_paragraph() {
        assert_eq!(ElementFrame::classify_tag("p"), TagType::P);
    }

    #[test]
    fn test_classify_tag_anchor() {
        assert_eq!(ElementFrame::classify_tag("a"), TagType::A);
    }

    #[test]
    fn test_classify_tag_headers() {
        assert_eq!(ElementFrame::classify_tag("h1"), TagType::Header);
        assert_eq!(ElementFrame::classify_tag("h2"), TagType::Header);
        assert_eq!(ElementFrame::classify_tag("h3"), TagType::Header);
        assert_eq!(ElementFrame::classify_tag("h4"), TagType::Header);
        assert_eq!(ElementFrame::classify_tag("h5"), TagType::Header);
        assert_eq!(ElementFrame::classify_tag("h6"), TagType::Header);
    }

    #[test]
    fn test_classify_tag_lists() {
        assert_eq!(ElementFrame::classify_tag("ul"), TagType::List);
        assert_eq!(ElementFrame::classify_tag("ol"), TagType::List);
        assert_eq!(ElementFrame::classify_tag("dl"), TagType::List);
    }

    #[test]
    fn test_classify_tag_semantic_article() {
        assert_eq!(ElementFrame::classify_tag("article"), TagType::Article);
        assert_eq!(ElementFrame::classify_tag("main"), TagType::Article);
        assert_eq!(ElementFrame::classify_tag("section"), TagType::Article);
    }

    #[test]
    fn test_classify_tag_other() {
        assert_eq!(ElementFrame::classify_tag("span"), TagType::Other);
        assert_eq!(ElementFrame::classify_tag("blockquote"), TagType::Other);
        assert_eq!(ElementFrame::classify_tag("table"), TagType::Other);
        assert_eq!(ElementFrame::classify_tag("unknown"), TagType::Other);
    }

    #[test]
    fn test_calculate_base_score_tag_types() {
        // Article tags get +30
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Article, None, None),
            30.0
        );
        // Div gets +5
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, None, None),
            5.0
        );
        // P gets +10
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::P, None, None),
            10.0
        );
        // Header gets -5
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Header, None, None),
            -5.0
        );
        // List gets -3
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::List, None, None),
            -3.0
        );
        // Other gets 0
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Other, None, None),
            0.0
        );
        // A (anchor) gets 0
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::A, None, None),
            0.0
        );
    }

    #[test]
    fn test_calculate_base_score_with_positive_id() {
        // Div (5) + positive id (25) = 30
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, Some("main-content"), None),
            30.0
        );
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, Some("article"), None),
            30.0
        );
    }

    #[test]
    fn test_calculate_base_score_with_positive_class() {
        // Div (5) + positive class (25) = 30
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, None, Some("entry-content")),
            30.0
        );
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, None, Some("post body")),
            30.0
        );
    }

    #[test]
    fn test_calculate_base_score_with_negative_id() {
        // Div (5) + negative id (-25) = -20
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, Some("sidebar"), None),
            -20.0
        );
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, Some("footer"), None),
            -20.0
        );
    }

    #[test]
    fn test_calculate_base_score_with_negative_class() {
        // Div (5) + negative class (-25) = -20
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, None, Some("nav-menu")),
            -20.0
        );
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, None, Some("comment-section")),
            -20.0
        );
    }

    #[test]
    fn test_calculate_base_score_with_unlikely_pattern() {
        // Div (5) + unlikely (-50) = -45
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, Some("ad-break"), None),
            -45.0
        );
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, None, Some("disqus-comments")),
            -45.0
        );
    }

    #[test]
    fn test_calculate_base_score_unlikely_overridden_by_maybe() {
        // "shadow" contains RE_MAYBE pattern, so RE_UNLIKELY penalty doesn't apply
        // Div (5) + 0 (maybe overrides unlikely) = 5
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, Some("shadow"), None),
            5.0
        );
        assert_eq!(
            ElementFrame::calculate_base_score(TagType::Div, None, Some("column")),
            5.0
        );
    }

    #[test]
    fn test_score_attribute_positive_patterns() {
        assert_eq!(ElementFrame::score_attribute("article"), 25.0);
        assert_eq!(ElementFrame::score_attribute("main-content"), 25.0);
        assert_eq!(ElementFrame::score_attribute("entry-body"), 25.0);
        assert_eq!(ElementFrame::score_attribute("post-text"), 25.0);
        assert_eq!(ElementFrame::score_attribute("blog-content"), 25.0);
        assert_eq!(ElementFrame::score_attribute("story-body"), 25.0);
        assert_eq!(ElementFrame::score_attribute("page-content"), 25.0);
    }

    #[test]
    fn test_score_attribute_negative_patterns() {
        assert_eq!(ElementFrame::score_attribute("sidebar"), -25.0);
        assert_eq!(ElementFrame::score_attribute("navigation"), -25.0);
        assert_eq!(ElementFrame::score_attribute("footer-links"), -25.0);
        assert_eq!(ElementFrame::score_attribute("header-menu"), -25.0);
        assert_eq!(ElementFrame::score_attribute("widget-area"), -25.0);
        assert_eq!(ElementFrame::score_attribute("comment-box"), -25.0);
        assert_eq!(ElementFrame::score_attribute("related-items"), -25.0);
        assert_eq!(ElementFrame::score_attribute("sponsor-box"), -25.0);
    }

    #[test]
    fn test_score_attribute_unlikely_patterns() {
        assert_eq!(ElementFrame::score_attribute("ad-break"), -50.0);
        assert_eq!(ElementFrame::score_attribute("disqus-thread"), -50.0);
        assert_eq!(ElementFrame::score_attribute("popup-overlay"), -50.0);
        assert_eq!(ElementFrame::score_attribute("shoutbox"), -50.0);
        assert_eq!(ElementFrame::score_attribute("rss-feed"), -50.0);
        assert_eq!(ElementFrame::score_attribute("share-buttons"), -50.0);
    }

    #[test]
    fn test_score_attribute_maybe_overrides_unlikely() {
        // "shadow" matches MAYBE, so UNLIKELY penalty doesn't apply
        assert_eq!(ElementFrame::score_attribute("shadow"), 0.0);
        assert_eq!(ElementFrame::score_attribute("column"), 0.0);
        // "and" in "expand" matches MAYBE
        assert_eq!(ElementFrame::score_attribute("expand"), 0.0);
    }

    #[test]
    fn test_score_attribute_neutral() {
        assert_eq!(ElementFrame::score_attribute("wrapper"), 0.0);
        assert_eq!(ElementFrame::score_attribute("container"), 0.0);
        assert_eq!(ElementFrame::score_attribute("box"), 0.0);
        assert_eq!(ElementFrame::score_attribute(""), 0.0);
    }

    // =========================================================================
    // Unit tests for is_viable_candidate
    // =========================================================================

    #[test]
    fn test_is_viable_candidate_article_with_enough_text() {
        let mut frame = ElementFrame::new("article", None, None);
        frame.text_len = 150;
        frame.accumulated_text = "This is a sample text with real words that passes the minimum length requirement for content".to_string();
        assert!(frame.is_viable_candidate());
    }

    #[test]
    fn test_is_viable_candidate_div_with_enough_text() {
        let mut frame = ElementFrame::new("div", Some("content"), None);
        frame.text_len = 150;
        frame.accumulated_text = "This is a sample text with real words that passes the minimum length requirement for content".to_string();
        assert!(frame.is_viable_candidate());
    }

    #[test]
    fn test_is_viable_candidate_too_short() {
        let mut frame = ElementFrame::new("article", None, None);
        frame.text_len = 50; // Below 100 minimum
        frame.accumulated_text = "Short text only".to_string();
        assert!(!frame.is_viable_candidate());
    }

    #[test]
    fn test_is_viable_candidate_header_not_content_tag() {
        let mut frame = ElementFrame::new("h1", None, None);
        frame.text_len = 150;
        frame.accumulated_text =
            "This is a very long header that should not be considered main content at all"
                .to_string();
        assert!(!frame.is_viable_candidate());
    }

    #[test]
    fn test_is_viable_candidate_list_not_content_tag() {
        let mut frame = ElementFrame::new("ul", None, None);
        frame.text_len = 150;
        frame.accumulated_text =
            "This is a very long list that should not be considered main content at all ok"
                .to_string();
        assert!(!frame.is_viable_candidate());
    }

    #[test]
    fn test_is_viable_candidate_anchor_not_content_tag() {
        let mut frame = ElementFrame::new("a", None, None);
        frame.text_len = 150;
        frame.accumulated_text =
            "This is a very long anchor that should not be considered main content ever"
                .to_string();
        assert!(!frame.is_viable_candidate());
    }

    #[test]
    fn test_is_viable_candidate_single_letter_words_rejected() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.text_len = 150;
        // Average word length is ~1, should be rejected
        frame.accumulated_text = "a b c d e f g h i j k l m n o p q r s t u v w x y z a b c d e f g h i j k l m n o p q r s t u v w x y z a b c d".to_string();
        assert!(!frame.is_viable_candidate());
    }

    #[test]
    fn test_is_viable_candidate_real_words_accepted() {
        let mut frame = ElementFrame::new("p", None, None);
        frame.text_len = 150;
        frame.accumulated_text = "These are real words with proper length that should pass the average word length check easily".to_string();
        assert!(frame.is_viable_candidate());
    }

    // =========================================================================
    // Unit tests for helper functions
    // =========================================================================

    #[test]
    fn test_is_non_content_container_all_containers() {
        assert!(is_non_content_container("head"));
        assert!(is_non_content_container("script"));
        assert!(is_non_content_container("style"));
        assert!(is_non_content_container("noscript"));
        assert!(is_non_content_container("template"));
        assert!(is_non_content_container("svg"));
        assert!(is_non_content_container("math"));
        assert!(is_non_content_container("canvas"));
        assert!(is_non_content_container("iframe"));
        assert!(is_non_content_container("object"));
        assert!(is_non_content_container("embed"));
        assert!(is_non_content_container("applet"));
        assert!(is_non_content_container("audio"));
        assert!(is_non_content_container("video"));
        assert!(is_non_content_container("form"));
    }

    #[test]
    fn test_is_non_content_container_false_for_content() {
        assert!(!is_non_content_container("div"));
        assert!(!is_non_content_container("p"));
        assert!(!is_non_content_container("article"));
        assert!(!is_non_content_container("section"));
        assert!(!is_non_content_container("span"));
    }

    #[test]
    fn test_is_void_element_all_void() {
        assert!(is_void_element("area"));
        assert!(is_void_element("base"));
        assert!(is_void_element("br"));
        assert!(is_void_element("col"));
        assert!(is_void_element("embed"));
        assert!(is_void_element("hr"));
        assert!(is_void_element("img"));
        assert!(is_void_element("input"));
        assert!(is_void_element("link"));
        assert!(is_void_element("meta"));
        assert!(is_void_element("param"));
        assert!(is_void_element("source"));
        assert!(is_void_element("track"));
        assert!(is_void_element("wbr"));
    }

    #[test]
    fn test_is_void_element_false_for_non_void() {
        assert!(!is_void_element("div"));
        assert!(!is_void_element("p"));
        assert!(!is_void_element("span"));
        assert!(!is_void_element("a"));
        assert!(!is_void_element("script"));
    }

    #[test]
    fn test_is_non_content_leaf_all_leaves() {
        assert!(is_non_content_leaf("title"));
        assert!(is_non_content_leaf("textarea"));
        assert!(is_non_content_leaf("select"));
        assert!(is_non_content_leaf("option"));
        assert!(is_non_content_leaf("optgroup"));
        assert!(is_non_content_leaf("button"));
        assert!(is_non_content_leaf("map"));
        assert!(is_non_content_leaf("datalist"));
        assert!(is_non_content_leaf("output"));
        assert!(is_non_content_leaf("progress"));
        assert!(is_non_content_leaf("meter"));
    }

    #[test]
    fn test_is_non_content_leaf_false_for_content() {
        assert!(!is_non_content_leaf("div"));
        assert!(!is_non_content_leaf("p"));
        assert!(!is_non_content_leaf("span"));
        assert!(!is_non_content_leaf("article"));
    }

    // =========================================================================
    // Unit tests for ElementFrame::append_text
    // =========================================================================

    #[test]
    fn test_append_text_basic() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.append_text("Hello world");
        assert_eq!(frame.accumulated_text, "Hello world");
    }

    #[test]
    fn test_append_text_multiple_calls() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.append_text("Hello");
        frame.append_text("world");
        assert_eq!(frame.accumulated_text, "Hello world");
    }

    #[test]
    fn test_append_text_normalizes_whitespace() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.append_text("Hello   world   test");
        assert_eq!(frame.accumulated_text, "Hello world test");
    }

    #[test]
    fn test_append_text_decodes_html_entities() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.append_text("Tom &amp; Jerry");
        assert_eq!(frame.accumulated_text, "Tom & Jerry");
    }

    #[test]
    fn test_append_text_decodes_numeric_entities() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.append_text("&#60;div&#62;");
        assert_eq!(frame.accumulated_text, "<div>");
    }

    #[test]
    fn test_append_text_ignores_whitespace_only() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.append_text("Hello");
        frame.append_text("   \n\t  ");
        frame.append_text("world");
        assert_eq!(frame.accumulated_text, "Hello world");
    }

    #[test]
    fn test_append_text_trims_leading_trailing() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.append_text("   Hello world   ");
        assert_eq!(frame.accumulated_text, "Hello world");
    }

    // =========================================================================
    // Unit tests for ElementFrame::calculate_final_score
    // =========================================================================

    #[test]
    fn test_calculate_final_score_base_only() {
        let frame = ElementFrame::new("article", None, None);
        // base_score = 30, text_len = 0, commas = 0
        // final = 30 + sqrt(0) + 0 = 30
        assert_eq!(frame.calculate_final_score(), 30.0);
    }

    #[test]
    fn test_calculate_final_score_with_text() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.text_len = 100;
        // base_score = 5, text_len = 100
        // final = 5 + sqrt(100) = 5 + 10 = 15
        assert_eq!(frame.calculate_final_score(), 15.0);
    }

    #[test]
    fn test_calculate_final_score_with_commas() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.text_len = 100;
        frame.comma_count = 5;
        // base_score = 5, text = sqrt(100) = 10, commas = 5
        // final = 5 + 10 + 5 = 20
        assert_eq!(frame.calculate_final_score(), 20.0);
    }

    #[test]
    fn test_calculate_final_score_high_link_density() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.text_len = 100;
        frame.link_text_len = 80; // 80% link density
                                  // base_score = 5, text = sqrt(100) = 10
                                  // link_density = 0.8 > 0.5, so score *= (1 - 0.8) = 0.2
                                  // final = (5 + 10) * 0.2 = 3
        let score = frame.calculate_final_score();
        assert!((score - 3.0).abs() < 0.001, "Expected ~3.0, got {}", score);
    }

    #[test]
    fn test_calculate_final_score_low_link_density() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.text_len = 100;
        frame.link_text_len = 30; // 30% link density - no penalty
                                  // base_score = 5, text = sqrt(100) = 10
                                  // link_density = 0.3 <= 0.5, no penalty
                                  // final = 5 + 10 = 15
        assert_eq!(frame.calculate_final_score(), 15.0);
    }

    #[test]
    fn test_calculate_final_score_exactly_half_link_density() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.text_len = 100;
        frame.link_text_len = 50; // exactly 50% - no penalty
                                  // final = 5 + 10 = 15
        assert_eq!(frame.calculate_final_score(), 15.0);
    }

    #[test]
    fn test_calculate_final_score_all_links() {
        let mut frame = ElementFrame::new("div", None, None);
        frame.text_len = 100;
        frame.link_text_len = 100; // 100% links
                                   // base_score = 5, text = 10
                                   // link_density = 1.0, score *= 0
                                   // final = 0
        assert_eq!(frame.calculate_final_score(), 0.0);
    }

    // =========================================================================
    // Edge case tests for find_main_content
    // =========================================================================

    #[test]
    fn test_deeply_nested_content() {
        // Test that deeply nested content is still found
        let html = r#"
            <html><body>
                <div><div><div><div><div><div><div><div><div><div>
                    <article id="content">
                        <p>This is deeply nested content that should still be found.
                        It has multiple sentences with commas, periods, and details.
                        The article discusses important topics that readers care about.</p>
                    </article>
                </div></div></div></div></div></div></div></div></div></div>
            </body></html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _url) = result.unwrap();
        assert!(text.contains("deeply nested content"));
    }

    #[test]
    fn test_unicode_content() {
        let html = r#"
            <html><body>
                <article>
                    <p>Unicode content: café, naïve, 日本語, emoji 🎉 and more text.
                    This article has substantial content with multiple sentences.
                    The content includes various Unicode characters and symbols.</p>
                </article>
            </body></html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _url) = result.unwrap();
        assert!(text.contains("café"));
        assert!(text.contains("日本語"));
    }

    #[test]
    fn test_html_entities_decoded() {
        let html = r#"
            <html><body>
                <article id="content">
                    <p>HTML entities: &lt;div&gt; and &amp; and &quot;quotes&quot; and &#169; copyright.
                    This is substantial content with multiple sentences discussing topics.
                    More text here to establish this as the main content area.</p>
                </article>
            </body></html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _url) = result.unwrap();
        assert!(text.contains("<div>"));
        assert!(text.contains("&"));
        assert!(text.contains("\"quotes\""));
    }

    #[test]
    fn test_empty_elements_ignored() {
        let html = r#"
            <html><body>
                <div id="empty"></div>
                <article id="content">
                    <p>This is the actual content that should be found and extracted.
                    It contains substantial text with multiple sentences and details.
                    The article discusses topics that readers want to read about.</p>
                </article>
                <div class="also-empty">   </div>
            </body></html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _url) = result.unwrap();
        assert!(text.contains("actual content"));
    }

    #[test]
    fn test_mixed_positive_negative_class() {
        // Element has both positive and negative patterns in class
        let html = r#"
            <html><body>
                <div class="sidebar content-area">
                    <p>This element has mixed signals in its class attribute.
                    It contains substantial text with multiple sentences here.
                    More content to establish this as a potential main area.</p>
                </div>
            </body></html>
        "#;

        // Should still find content - positive "content" matches
        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
    }

    #[test]
    fn test_title_with_html_entities() {
        let html = r#"
            <html>
                <head>
                    <title>Tom &amp; Jerry&#39;s Adventure</title>
                </head>
                <body>
                    <article>
                        <p>This is the main article content with lots of text.
                        It has multiple sentences, and commas too.
                        The article discusses important topics that readers care about.</p>
                    </article>
                </body>
            </html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (_text, title, _url) = result.unwrap();
        assert_eq!(title, "Tom & Jerry's Adventure");
    }

    #[test]
    fn test_competing_content_areas() {
        // Two good content areas - larger one should win
        let html = r#"
            <html><body>
                <article id="short-article">
                    <p>Short article with some content here.</p>
                </article>
                <article id="long-article">
                    <p>This is a much longer article with substantially more content.
                    It has multiple paragraphs discussing important topics in detail.
                    The content includes commas, periods, and proper sentence structure.
                    More text here to ensure this article clearly wins the scoring.</p>
                    <p>Another paragraph with even more valuable content for readers.
                    This should make this article the clear winner in the scoring.</p>
                </article>
            </body></html>
        "#;

        let result = find_main_content(html.as_bytes(), "https://example.com/test");
        assert!(result.is_ok());
        let (text, _title, _url) = result.unwrap();
        assert!(text.contains("much longer article"));
    }
}
