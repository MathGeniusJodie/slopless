use bytecount::count;
use html_escape::decode_html_entities;
use itertools::Itertools;
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
const MIN_TEXT_LENGTH: u32 = 100;

/// Minimum average word length to filter garbage like "a a a b b b"
const MIN_AVG_WORD_LENGTH: usize = 2;

/// Link density threshold above which content is penalized
const LINK_DENSITY_THRESHOLD: f32 = 0.5;

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
pub(crate) enum TagType {
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
pub(crate) struct ElementFrame {
    pub(crate) tag_type: TagType,
    pub(crate) base_score: f32, // Initial score from tag type + class/id
    pub(crate) text_len: u32,   // Total character count
    pub(crate) link_text_len: u32, // Character count inside <a> tags
    pub(crate) comma_count: u32, // Number of commas (heuristic for real sentences)
    pub(crate) accumulated_text: String, // Actual text content for this element
}

impl ElementFrame {
    /// Create a new frame for an HTML element with initial scoring based on tag type and attributes
    pub(crate) fn new(tag_name: &str, id: Option<&str>, class: Option<&str>) -> Self {
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
    pub(crate) fn classify_tag(tag_name: &str) -> TagType {
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
            v if RE_UNLIKELY.is_match(v) && !RE_MAYBE.is_match(v) => -50.0,
            v if RE_POSITIVE.is_match(v) => 25.0,
            v if RE_NEGATIVE.is_match(v) => -25.0,
            _ => 0.0,
        }
    }

    /// Append text content, decoding HTML entities and normalizing whitespace
    pub(crate) fn append_text(&mut self, text: &str) {
        let decoded = decode_html_entities(text);
        let mut words = decoded.split_whitespace().peekable();

        if words.peek().is_none() {
            return;
        }
        // Add space separator if needed
        if !self.accumulated_text.is_empty() && !self.accumulated_text.ends_with(' ') {
            self.accumulated_text.push(' ');
        }
        for part in words.intersperse(" ") {
            self.accumulated_text.push_str(part);
        }
    }

    /// Append text and return the content length added (excluding separator spaces)
    pub(crate) fn append_text_and_measure(&mut self, text: &str) -> usize {
        let len_before = self.accumulated_text.len();
        self.append_text(text);
        let total_added = self.accumulated_text.len().saturating_sub(len_before);

        // If we added a separator space, don't count it as content
        if len_before > 0 && total_added > 0 {
            total_added.saturating_sub(1)
        } else {
            total_added
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
        (self.text_len as f32).sqrt()
    }

    /// Multiplier based on link density (penalizes navigation-heavy content)
    fn link_density_multiplier(&self) -> f32 {
        if self.text_len == 0 {
            return 1.0;
        }
        let link_density = self.link_text_len as f32 / self.text_len as f32;
        if link_density > LINK_DENSITY_THRESHOLD {
            1.0 - link_density
        } else {
            1.0
        }
    }

    /// Check if this element is a viable candidate for main content
    pub(crate) fn is_viable_candidate(&self) -> bool {
        self.is_content_tag() && self.has_enough_text() && self.has_real_words()
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
        self.text_len >= MIN_TEXT_LENGTH
    }

    /// Check if element has real words (not garbage like "a a a b b b")
    fn has_real_words(&self) -> bool {
        let word_count = self.accumulated_text.split_whitespace().count();
        let avg_word_length = self
            .accumulated_text
            .len()
            .checked_div(word_count)
            .unwrap_or(0);
        avg_word_length > MIN_AVG_WORD_LENGTH
    }
}

// -----------------------------------------------------------------------------
// Helper Functions
// -----------------------------------------------------------------------------

/// Container tags whose entire content should be ignored
pub(crate) fn is_non_content_container(tag: &str) -> bool {
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
pub(crate) fn is_void_element(tag: &str) -> bool {
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
pub(crate) fn is_non_content_leaf(tag: &str) -> bool {
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

/// What action to take when encountering an HTML element
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementAction {
    /// Track title element specially (want its text even though it's in skipped <head>)
    TrackTitle,
    /// Increment skip depth (inside a non-content container)
    IncrementSkipDepth,
    /// Start skipping a non-content container
    StartSkipping,
    /// Push a frame for this content element
    PushFrame,
    /// Ignore this element entirely (void, leaf, or depth limit)
    Ignore,
}

/// Parsing context shared across HTML rewriter callbacks
struct ParsingContext {
    element_stack: Vec<ElementFrame>,
    best_content: Option<String>,
    best_content_score: f32,
    skip_depth: u32,
    anchor_depth: u32,
    page_title: String,
    currently_in_title: bool,
    canonical_url: Option<String>,
}

impl ParsingContext {
    fn new() -> Self {
        Self {
            element_stack: Vec::with_capacity(64),
            best_content: None,
            best_content_score: 0.0,
            skip_depth: 0,
            anchor_depth: 0,
            page_title: String::new(),
            currently_in_title: false,
            canonical_url: None,
        }
    }

    /// Determine what action to take for an element based on current state and tag properties
    fn classify_element(&self, tag_name: &str) -> ElementAction {
        if tag_name == "title" {
            return ElementAction::TrackTitle;
        }

        let is_void = is_void_element(tag_name);

        // Inside a skipped container?
        if self.skip_depth > 0 {
            return if is_void {
                ElementAction::Ignore
            } else {
                ElementAction::IncrementSkipDepth
            };
        }

        // Non-content container (script, style, head, etc.)?
        if is_non_content_container(tag_name) {
            return ElementAction::StartSkipping;
        }

        // Non-content leaf or void element?
        if is_non_content_leaf(tag_name) || is_void {
            return ElementAction::Ignore;
        }

        // Stack depth limit reached?
        if self.element_stack.len() >= MAX_ELEMENT_STACK_DEPTH {
            return ElementAction::Ignore;
        }

        ElementAction::PushFrame
    }

    /// Handle closing a title element
    fn close_title(&mut self) {
        self.currently_in_title = false;
    }

    /// Decrement skip depth when closing a skipped element
    fn close_skipped(&mut self) {
        self.skip_depth -= 1;
    }

    /// Handle closing a content element: pop frame, bubble stats, maybe update best
    fn close_content_element(&mut self, is_anchor: bool) {
        if is_anchor {
            self.anchor_depth -= 1;
        }

        let Some(frame) = self.element_stack.pop() else {
            return;
        };

        let score = frame.calculate_final_score();
        let is_new_best = frame.is_viable_candidate() && score > self.best_content_score;

        // Bubble stats to parent
        if let Some(parent) = self.element_stack.last_mut() {
            parent.text_len += frame.text_len;
            parent.link_text_len += frame.link_text_len;
            parent.comma_count += frame.comma_count;
            parent.append_text(&frame.accumulated_text);
        }

        // Update best if this is better
        if is_new_best {
            self.best_content_score = score;
            self.best_content = Some(frame.accumulated_text);
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

        let text_len = frame.append_text_and_measure(text) as u32;
        frame.text_len += text_len;
        frame.comma_count += count(text.as_bytes(), b',') as u32;

        if self.anchor_depth > 0 {
            frame.link_text_len += text_len;
        }
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

    let ctx_canonical = ctx.clone();
    let ctx_element = ctx.clone();
    let ctx_text = ctx.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                // Extract canonical URL
                element!("link[rel=canonical]", move |el| {
                    if let Some(href) = el.get_attribute("href").filter(|h| !h.is_empty()) {
                        ctx_canonical.borrow_mut().canonical_url = Some(href);
                    }
                    Ok(())
                }),
                // Process all elements
                element!("*", move |el| {
                    let tag_name = el.tag_name();
                    let is_anchor = tag_name == "a";

                    let action = ctx_element.borrow().classify_element(&tag_name);
                    if !execute_open_action(&ctx_element, &el, &tag_name, action, is_anchor) {
                        return Ok(());
                    }
                    let Some(handlers) = el.end_tag_handlers() else {
                        return Ok(());
                    };

                    let ctx_close = ctx_element.clone();
                    handlers.push(Box::new(move |_| {
                        let mut ctx = ctx_close.borrow_mut();
                        match action {
                            ElementAction::TrackTitle => ctx.close_title(),
                            ElementAction::IncrementSkipDepth | ElementAction::StartSkipping => {
                                ctx.close_skipped()
                            }
                            ElementAction::PushFrame => ctx.close_content_element(is_anchor),
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

                    let mut context = ctx_text.borrow_mut();
                    if context.currently_in_title {
                        context.append_title_text(text);
                    } else if context.skip_depth == 0 {
                        context.append_content_text(text);
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
    el: &lol_html::html_content::Element<'_, '_>,
    tag_name: &str,
    action: ElementAction,
    is_anchor: bool,
) -> bool {
    match action {
        ElementAction::TrackTitle => {
            ctx.borrow_mut().currently_in_title = true;
        }
        ElementAction::IncrementSkipDepth | ElementAction::StartSkipping => {
            ctx.borrow_mut().skip_depth += 1;
        }
        ElementAction::PushFrame => {
            let id = el.get_attribute("id");
            let class = el.get_attribute("class");
            let frame = ElementFrame::new(tag_name, id.as_deref(), class.as_deref());

            let mut ctx = ctx.borrow_mut();
            ctx.element_stack.push(frame);
            if is_anchor {
                ctx.anchor_depth += 1;
            }
        }
        ElementAction::Ignore => return false,
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

    let Some(content_text) = final_context.best_content else {
        anyhow::bail!("No main content found")
    };

    let resolved_url = resolve_url(url, final_context.canonical_url.as_deref())?;
    let content: String = content_text.nfkc().collect();
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
