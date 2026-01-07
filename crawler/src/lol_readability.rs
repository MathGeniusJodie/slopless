//! # Readability Algorithm
//!
//! Extracts the "main content" from an HTML page using a single-pass streaming parser.
//!
//! ## How It Works
//!
//! 1. **Stream HTML through lol_html** - No DOM tree built in memory
//! 2. **Score each element** based on:
//!    - Tag type: `<article>`, `<main>`, `<section>` score high; `<nav>`, `<footer>` score low
//!    - Class/ID attributes: "content", "article" are positive; "sidebar", "nav" are negative
//!    - Text length: More text = more likely to be content (uses sqrt to avoid over-weighting)
//!    - Link density: Navigation has lots of links; penalty only above 40% (Wikipedia-style ok)
//!    - Commas: Real sentences have punctuation, spam doesn't
//! 3. **Track nested elements** - Child stats bubble up to parents
//! 4. **Return the highest-scoring element's text**
//!
//! ## Usage
//!
//! ```ignore
//! let (text, title, url) = find_main_content(html_bytes, "https://example.com")?;
//! ```

use html_escape::decode_html_entities;
use lol_html::{element, text, HtmlRewriter, Settings};
use regex::Regex;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;
use url::Url;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Maximum nesting depth for element tracking (prevents stack exhaustion from malformed HTML)
const MAX_ELEMENT_STACK_DEPTH: usize = 256;

/// Minimum text length required for an element to be considered main content
const MIN_TEXT_LENGTH: usize = 100;

/// Link density threshold below which no penalty is applied.
/// Wikipedia and gwern.net articles often have 30-40% of text as links.
/// Only penalize above this to avoid hurting link-rich but legitimate content.
const LINK_DENSITY_THRESHOLD: f32 = 0.4;

// Regex patterns for scoring element class/id attributes

/// Patterns that are very unlikely to be main content (heavily penalize)
/// Note: AMBIGUOUS_PATTERN matches override these (prevent the -50 penalty)
static UNLIKELY_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)combx|community|disqus|extra|remark|rss|share|shoutbox|skyscraper|ad-break|agegate|popup"
    ).unwrap()
});

/// Ambiguous patterns - if matched, prevents UNLIKELY_PATTERN penalty
static AMBIGUOUS_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)and|column|shadow").unwrap());

/// Positive signals that suggest main content
static POSITIVE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)article|body|content|entry|hentry|main|page|pagination|post|text|blog|story")
        .unwrap()
});

/// Negative signals that suggest non-content elements
static NEGATIVE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)hidden|banner|comment|com-|contact|foot|footer|footnote|header|masthead|media|meta|menu|nav|outbrain|promo|related|scroll|sidebar|sponsor|shopping|tags|tool|widget|namespace|action|catlinks|toc|printfooter|jump-to|siteSub|contentSub"
    ).unwrap()
});

// -----------------------------------------------------------------------------
// Data Structures
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagType {
    Div,
    P,
    Header,  // h1-h6
    List,    // ul, ol, dl
    Article, // article, main, section - semantic content tags
    Other,
}

/// A stack frame representing an open HTML element being scored
#[derive(Debug)]
pub(crate) struct ElementFrame {
    pub(crate) tag_type: TagType,
    pub(crate) base_score: f32,
    pub(crate) char_count: usize,
    pub(crate) link_char_count: usize,
    /// Proxy for sentence count - real content has punctuation, spam doesn't
    pub(crate) comma_count: usize,
    pub(crate) extracted_text: String,
}

impl ElementFrame {
    /// Create a new frame for an HTML element with initial scoring based on tag type and attributes
    pub(crate) fn new(tag_name: &str, id: Option<&str>, class: Option<&str>) -> Self {
        let tag_type = Self::classify_tag(tag_name);
        let base_score = Self::calculate_base_score(tag_type, id, class);

        Self {
            tag_type,
            base_score,
            char_count: 0,
            link_char_count: 0,
            comma_count: 0,
            extracted_text: String::new(),
        }
    }
    /// Map HTML tag name to internal tag type
    pub(crate) fn classify_tag(tag_name: &str) -> TagType {
        match tag_name {
            "div" => TagType::Div,
            "p" => TagType::P,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => TagType::Header,
            "ul" | "ol" | "dl" => TagType::List,
            "article" | "main" | "section" => TagType::Article,
            _ => TagType::Other,
        }
    }
    /// Calculate initial score based on tag type and class/id attributes
    pub(crate) fn calculate_base_score(
        tag_type: TagType,
        id: Option<&str>,
        class: Option<&str>,
    ) -> f32 {
        // Start with tag-based score
        let mut score = match tag_type {
            TagType::Article => 30.0, // Semantic HTML5 content tags
            TagType::Div => 5.0,
            TagType::P => 10.0,
            TagType::Header => -5.0, // Headers aren't main content
            TagType::List => -3.0,   // Lists often navigation
            _ => 0.0,
        };
        score += id.map(Self::score_attribute).unwrap_or(0.0);
        score += class.map(Self::score_attribute).unwrap_or(0.0);
        score
    }
    /// Score a class or id attribute value based on regex patterns
    pub(crate) fn score_attribute(attr_value: &str) -> f32 {
        // Heavily penalize obvious non-content patterns
        match attr_value {
            v if UNLIKELY_PATTERN.is_match(v) && !AMBIGUOUS_PATTERN.is_match(v) => -50.0,
            v if POSITIVE_PATTERN.is_match(v) => 25.0,
            v if NEGATIVE_PATTERN.is_match(v) => -25.0,
            _ => 0.0,
        }
    }
    /// Add text content, updating all counters. Decodes HTML entities and normalizes whitespace.
    /// If `is_link_text` is true, also counts toward link_char_count.
    pub(crate) fn add_text(&mut self, text: &str, is_link_text: bool) {
        let decoded = decode_html_entities(text);
        let len_before = self.extracted_text.len();

        self.append_whitespace_normalized(&decoded);

        // Count chars added, excluding the leading separator space (if any)
        let mut start = len_before;
        if self.extracted_text.as_bytes().get(start) == Some(&b' ') {
            start += 1;
        }
        let chars_added = self.extracted_text.len().saturating_sub(start);

        self.char_count += chars_added;
        self.comma_count += bytecount::count(decoded.as_bytes(), b',');
        if is_link_text {
            self.link_char_count += chars_added;
        }
    }

    /// Append already-decoded text (used for bubbling child text to parent)
    pub(crate) fn append_text(&mut self, text: &str) {
        self.append_whitespace_normalized(text);
    }

    /// Append text with whitespace normalization (collapses whitespace, trims)
    fn append_whitespace_normalized(&mut self, text: &str) {
        for part in text.split_whitespace() {
            if !self.extracted_text.is_empty() {
                self.extracted_text.push(' ');
            }
            self.extracted_text.push_str(part);
        }
    }
    /// Calculate final score including text-based heuristics
    pub(crate) fn calculate_final_score(&self) -> f32 {
        let mut score = self.base_score;
        score += self.text_length_score();
        score += self.comma_count as f32;
        score *= self.link_density_multiplier();
        score
    }
    /// Score contribution from text length (sqrt to avoid over-weighting long elements)
    fn text_length_score(&self) -> f32 {
        (self.char_count as f32).sqrt()
    }
    /// Multiplier based on link density (penalizes navigation-heavy content).
    ///
    /// Link density = fraction of text that's inside <a> tags.
    /// - Below threshold (40%): multiplier 1.0 (no penalty)
    /// - Above threshold: linear penalty ramping to 0.0 at 100% links
    ///
    /// The 40% threshold allows Wikipedia/gwern-style content with many inline links
    /// to score normally, while navigation menus (mostly links) are still penalized.
    fn link_density_multiplier(&self) -> f32 {
        if self.char_count == 0 {
            return 1.0;
        }
        let link_density = self.link_char_count as f32 / self.char_count as f32;
        if link_density <= LINK_DENSITY_THRESHOLD {
            return 1.0;
        }
        // Linear penalty from threshold to 100%
        // At threshold: 1.0, at 100%: 0.0
        let excess = link_density - LINK_DENSITY_THRESHOLD;
        let range = 1.0 - LINK_DENSITY_THRESHOLD;
        (1.0 - excess / range).clamp(0.0, 1.0)
    }
    /// Check if this element is a viable candidate for main content
    pub(crate) fn is_viable_candidate(&self) -> bool {
        self.is_content_tag() && self.has_enough_text()
    }
    /// Check if this is a content-bearing tag type
    fn is_content_tag(&self) -> bool {
        matches!(
            self.tag_type,
            TagType::Article | TagType::Div | TagType::P | TagType::Other
        )
    }
    /// Check if element has enough text to be considered content
    fn has_enough_text(&self) -> bool {
        self.char_count >= MIN_TEXT_LENGTH
    }
}

// -----------------------------------------------------------------------------
// Tag Classification
// -----------------------------------------------------------------------------

/// Categories of HTML tags for parsing decisions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagCategory {
    /// Container whose content should be ignored (head, script, style, etc.)
    NonContentContainer,
    /// Self-closing tags with no content (br, img, input, etc.)
    VoidElement,
    /// Non-content tags that have content we skip (textarea, select, etc.)
    NonContentLeaf,
    /// Normal content-bearing element
    Content,
}

/// Classify an HTML tag into a category for parsing decisions
pub(crate) fn categorize_tag(tag: &str) -> TagCategory {
    match tag {
        // Container tags whose entire content should be ignored
        "head" | "script" | "style" | "noscript" | "template" | "svg" | "math" | "canvas"
        | "iframe" | "object" | "embed" | "applet" | "audio" | "video" | "form" => {
            TagCategory::NonContentContainer
        }
        // Void/self-closing tags
        "area" | "base" | "br" | "col" | "hr" | "img" | "input" | "link" | "meta" | "param"
        | "source" | "track" | "wbr" => TagCategory::VoidElement,
        // Non-content leaf tags
        "title" | "textarea" | "select" | "option" | "optgroup" | "button" | "map" | "datalist"
        | "output" | "progress" | "meter" => TagCategory::NonContentLeaf,
        // Everything else is content
        _ => TagCategory::Content,
    }
}

/// What action to take when encountering an HTML element
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementAction {
    /// Track title element specially (want its text even though it's in skipped <head>)
    TrackTitle,
    /// Skip this element and its contents (non-content container like script, style, head)
    Skip,
    /// Push a frame for this content element
    PushFrame,
    /// Push a lightweight link tracker (no full frame, just nesting depth)
    PushLink,
    /// Ignore this element entirely (void, leaf, or depth limit)
    Ignore,
}

/// Parsing context shared across HTML rewriter callbacks
struct ParsingContext {
    element_stack: Vec<ElementFrame>,
    best_extracted_text: Option<String>,
    highest_score: f32,
    /// Depth inside non-content containers (script, style, head, etc.)
    non_content_depth: u32,
    /// Depth inside <a> tags (for tracking link text)
    link_nesting_depth: u32,
    page_title: String,
    inside_title_tag: bool,
    canonical_url: Option<String>,
}

impl ParsingContext {
    fn new() -> Self {
        Self {
            element_stack: Vec::with_capacity(64),
            best_extracted_text: None,
            highest_score: 0.0,
            non_content_depth: 0,
            link_nesting_depth: 0,
            page_title: String::new(),
            inside_title_tag: false,
            canonical_url: None,
        }
    }

    /// Determine what action to take for an element based on current state and tag properties
    fn classify_element(&self, tag_name: &str) -> ElementAction {
        if tag_name == "title" {
            return ElementAction::TrackTitle;
        }

        let category = categorize_tag(tag_name);

        // Inside a non-content container?
        if self.non_content_depth > 0 {
            return if category == TagCategory::VoidElement {
                ElementAction::Ignore
            } else {
                ElementAction::Skip
            };
        }

        // Check tag category
        match category {
            TagCategory::NonContentContainer => return ElementAction::Skip,
            TagCategory::NonContentLeaf | TagCategory::VoidElement => return ElementAction::Ignore,
            TagCategory::Content => {}
        }

        // Links get lightweight tracking (no full frame needed)
        if tag_name == "a" {
            return ElementAction::PushLink;
        }

        // Stack depth limit reached?
        if self.element_stack.len() >= MAX_ELEMENT_STACK_DEPTH {
            return ElementAction::Ignore;
        }
        ElementAction::PushFrame
    }

    /// Handle closing a title element
    fn close_title(&mut self) {
        self.inside_title_tag = false;
    }

    /// Decrement non-content depth when closing a skipped element
    fn close_skipped(&mut self) {
        self.non_content_depth = self.non_content_depth.saturating_sub(1);
    }

    /// Handle closing a link element (lightweight - just decrement depth)
    fn close_link(&mut self) {
        self.link_nesting_depth = self.link_nesting_depth.saturating_sub(1);
    }

    /// Handle closing a content element: pop frame, bubble stats, maybe update best
    fn close_content_element(&mut self) {
        let Some(frame) = self.element_stack.pop() else {
            return;
        };

        let score = frame.calculate_final_score();
        let is_new_best = frame.is_viable_candidate() && score > self.highest_score;

        // Bubble stats to parent
        if let Some(parent) = self.element_stack.last_mut() {
            parent.char_count += frame.char_count;
            parent.link_char_count += frame.link_char_count;
            parent.comma_count += frame.comma_count;
            parent.append_text(&frame.extracted_text);
        }

        // Update best if this is better
        if is_new_best {
            self.highest_score = score;
            self.best_extracted_text = Some(frame.extracted_text);
        }
    }

    /// Add text to the page title
    fn append_title_text(&mut self, text: &str) {
        let decoded = decode_html_entities(text.trim());
        if decoded.is_empty() {
            return;
        }
        if !self.page_title.is_empty() {
            self.page_title.push(' ');
        }
        self.page_title.push_str(&decoded);
    }

    /// Add text to the current element frame
    fn append_content_text(&mut self, text: &str) {
        let Some(frame) = self.element_stack.last_mut() else {
            return;
        };
        frame.add_text(text, self.link_nesting_depth > 0);
    }
}

// -----------------------------------------------------------------------------
// Main Function (Single Pass)
// -----------------------------------------------------------------------------

/// Find and extract the main content as plain text.
/// Returns: (content_text, title, url)
/// The URL will be the canonical URL if found, otherwise the input URL.
/// Returns an error if no suitable content element was found.
pub fn find_main_content(html: &[u8], url: &str) -> anyhow::Result<(String, String, String)> {
    let ctx = Rc::new(RefCell::new(ParsingContext::new()));

    // Each callback closure needs its own Rc clone (same underlying context)
    let ctx_for_canonical = ctx.clone();
    let ctx_for_elements = ctx.clone();
    let ctx_for_text = ctx.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                // Extract canonical URL from <link rel="canonical">
                element!("link[rel=canonical]", move |element| {
                    if let Some(href) = element.get_attribute("href").filter(|h| !h.is_empty()) {
                        ctx_for_canonical.borrow_mut().canonical_url = Some(href);
                    }
                    Ok(())
                }),
                // Process all elements for scoring
                element!("*", move |element| {
                    let tag_name = element.tag_name();
                    let action = ctx_for_elements.borrow().classify_element(&tag_name);
                    if !execute_open_action(&ctx_for_elements, &element, &tag_name, action) {
                        return Ok(());
                    }
                    let Some(end_tag_handlers) = element.end_tag_handlers() else {
                        return Ok(());
                    };

                    let ctx_for_close = ctx_for_elements.clone();
                    end_tag_handlers.push(Box::new(move |_| {
                        let mut ctx = ctx_for_close.borrow_mut();
                        match action {
                            ElementAction::TrackTitle => ctx.close_title(),
                            ElementAction::Skip => ctx.close_skipped(),
                            ElementAction::PushLink => ctx.close_link(),
                            ElementAction::PushFrame => ctx.close_content_element(),
                            ElementAction::Ignore => {}
                        }
                        Ok(())
                    }));
                    Ok(())
                }),
                // Process text content
                text!("*", move |text_chunk| {
                    let text = text_chunk.as_str();
                    if text.trim().is_empty() {
                        return Ok(());
                    }
                    let mut ctx = ctx_for_text.borrow_mut();
                    if ctx.inside_title_tag {
                        ctx.append_title_text(text);
                    } else if ctx.non_content_depth == 0 {
                        ctx.append_content_text(text);
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
    finalize_result(ctx, url)
}

/// Execute the open tag action; returns true if a close handler is needed
fn execute_open_action(
    ctx: &Rc<RefCell<ParsingContext>>,
    element: &lol_html::html_content::Element<'_, '_>,
    tag_name: &str,
    action: ElementAction,
) -> bool {
    if action == ElementAction::Ignore {
        return false;
    }
    let mut ctx = ctx.borrow_mut();
    match action {
        ElementAction::TrackTitle => ctx.inside_title_tag = true,
        ElementAction::Skip => ctx.non_content_depth += 1,
        ElementAction::PushLink => ctx.link_nesting_depth += 1,
        ElementAction::PushFrame => {
            let id = element.get_attribute("id");
            let class = element.get_attribute("class");
            ctx.element_stack
                .push(ElementFrame::new(tag_name, id.as_deref(), class.as_deref()));
        }
        ElementAction::Ignore => unreachable!(),
    }
    true
}

/// Extract results from the parsing context and normalize the URL
fn finalize_result(
    ctx: Rc<RefCell<ParsingContext>>,
    url: &str,
) -> anyhow::Result<(String, String, String)> {
    let final_context = Rc::into_inner(ctx)
        .expect("Rc should have single owner")
        .into_inner();
    let Some(extracted_text) = final_context.best_extracted_text else {
        anyhow::bail!("No main content found")
    };
    let resolved_url = resolve_url(url, final_context.canonical_url.as_deref())?;
    let content: String = extracted_text.nfkc().collect();
    let title: String = final_context.page_title.nfkc().collect();
    Ok((content, title, resolved_url))
}

/// Resolve canonical URL against base URL and normalize
fn resolve_url(base: &str, canonical: Option<&str>) -> anyhow::Result<String> {
    let base_url = Url::parse(base)?;
    let resolved = match canonical {
        Some(c) => base_url.join(c)?,
        None => base_url,
    };
    let mut url_str = resolved.to_string();
    // Strip trailing slash (but not for root paths like "https://example.com/")
    if url_str.ends_with('/') && resolved.path().len() > 1 {
        url_str.pop();
    }
    Ok(url_str)
}
