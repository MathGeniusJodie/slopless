use lol_html::{element, text, EndTagHandler, HtmlRewriter, Settings};
use regex::Regex;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;

// -----------------------------------------------------------------------------
// Static Configuration (Compiled Once)
// -----------------------------------------------------------------------------

/// Maximum nesting depth for element tracking (prevents stack exhaustion from malformed HTML)
const MAX_ELEMENT_STACK_DEPTH: usize = 256;

// Regex patterns for scoring element class/id attributes

/// Patterns that are very unlikely to be main content (heavily penalize)
static RE_UNLIKELY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)banner|combx|comment|community|disqus|extra|foot|header|menu|related|remark|rss|share|shoutbox|sidebar|skyscraper|sponsor|ad-break|agegate|pagination|popup"
    ).unwrap()
});

/// Ambiguous patterns that could go either way
static RE_MAYBE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)and|article|body|column|main|shadow").unwrap());

/// Positive signals that suggest main content
static RE_POSITIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)article|body|content|entry|hentry|main|page|pagination|post|text|blog|story")
        .unwrap()
});

/// Negative signals that suggest non-content elements
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

/// A stack frame representing an open HTML element being scored
#[derive(Debug, Default)]
struct ElementFrame {
    tag_type: Option<TagType>,
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
            tag_type: Some(tag_type),
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

    /// Append text content, normalizing whitespace
    fn append_text(&mut self, text: &str) {
        let mut words = text.split_whitespace().peekable();
        if words.peek().is_none() {
            return; // Empty after whitespace normalization
        }

        // Add space separator if needed
        if !self.accumulated_text.is_empty() && !self.accumulated_text.ends_with(' ') {
            self.accumulated_text.push(' ');
        }

        // Join words with single spaces
        if let Some(first_word) = words.next() {
            self.accumulated_text.push_str(first_word);
            for word in words {
                self.accumulated_text.push(' ');
                self.accumulated_text.push_str(word);
            }
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
            Some(TagType::Article | TagType::Div | TagType::P | TagType::Other)
        );

        // Must have enough text (filter out tiny snippets)
        const MIN_TEXT_LENGTH: u32 = 100;
        let has_enough_text = self.text_len >= MIN_TEXT_LENGTH;

        // Must have real words (filter out "a a a b b b" garbage)
        let word_count = self.accumulated_text.split_whitespace().count();
        let avg_word_length = if word_count > 0 {
            self.accumulated_text.len() / word_count
        } else {
            0
        };
        let has_real_words = avg_word_length > 2;

        // Must have actual content
        let has_content = !self.accumulated_text.is_empty();

        is_content_tag && has_enough_text && has_real_words && has_content
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
    /// Parsing context shared across HTML rewriter callbacks
    struct ParsingContext {
        element_stack: Vec<ElementFrame>,
        best_content: Option<String>,
        best_content_score: f32,
        skip_depth: u32,
        anchor_depth: u32, // Track nesting inside <a> tags for link density
        extracted_urls: Vec<String>,
        page_title: String,
        currently_in_title: bool,
    }

    let parsing_context = Rc::new(RefCell::new(ParsingContext {
        element_stack: Vec::with_capacity(64),
        best_content: None,
        best_content_score: 0.0,
        skip_depth: 0,
        anchor_depth: 0,
        extracted_urls: Vec::new(),
        page_title: String::new(),
        currently_in_title: false,
    }));

    let context_for_open = parsing_context.clone();
    let context_for_text = parsing_context.clone();
    let context_for_close = parsing_context.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("*", move |el| {
                    let tag_name = el.tag_name();
                    let is_container_skip = is_non_content_container(&tag_name);
                    let is_leaf_skip = is_non_content_leaf(&tag_name);
                    let is_void = is_void_element(&tag_name);

                    // Extract URLs from all <a> tags (for link discovery)
                    if tag_name == "a" {
                        if let Some(href) = el.get_attribute("href") {
                            if !href.is_empty() {
                                context_for_open.borrow_mut().extracted_urls.push(href);
                            }
                        }
                    }

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
                        if !context.page_title.is_empty() {
                            context.page_title.push(' ');
                        }
                        context.page_title.push_str(text.trim());
                        return Ok(());
                    }

                    // Skip text inside non-content containers
                    if context.skip_depth > 0 {
                        return Ok(());
                    }

                    // Add text to current element frame
                    let inside_anchor = context.anchor_depth > 0;
                    if let Some(current_frame) = context.element_stack.last_mut() {
                        let text_length = text.len() as u32;
                        let comma_count = text.bytes().filter(|&b| b == b',').count() as u32;

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

    match final_context.best_content {
        Some(content_text) => Ok((
            content_text,
            final_context.extracted_urls,
            final_context.page_title,
        )),
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

        let result = find_main_content(html.as_bytes());
        assert!(result.is_ok());
        let (text, _urls, _title) = result.unwrap();
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
