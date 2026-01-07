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
    pub(crate) base_score: f32,          // Initial score from tag type + class/id
    pub(crate) text_len: u32,            // Total character count
    pub(crate) link_text_len: u32,       // Character count inside <a> tags
    pub(crate) comma_count: u32,         // Number of commas (heuristic for real sentences)
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
    pub(crate) fn calculate_base_score(tag_type: TagType, id: Option<&str>, class: Option<&str>) -> f32 {
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
    pub(crate) fn score_attribute(attr_value: &str) -> f32 {
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
    pub(crate) fn append_text(&mut self, text: &str) {
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
    pub(crate) fn calculate_final_score(&self) -> f32 {
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
    pub(crate) fn is_viable_candidate(&self) -> bool {
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
