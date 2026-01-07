#[cfg(test)]
mod tests {
    use crate::lol_readability::{
        find_main_content, is_non_content_container, is_non_content_leaf, is_void_element,
        ElementFrame, TagType,
    };

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
