#[cfg(test)]
mod tests {
    use crate::{parse_domain_line, parse_domains};

    #[test]
    fn test_parse_domain_line_simple() {
        assert_eq!(
            parse_domain_line("example.com"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_with_subdomain() {
        assert_eq!(
            parse_domain_line("www.example.com"),
            Some("www.example.com".to_string())
        );
        assert_eq!(
            parse_domain_line("blog.example.com"),
            Some("blog.example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_strips_https() {
        assert_eq!(
            parse_domain_line("https://example.com"),
            Some("example.com".to_string())
        );
        assert_eq!(
            parse_domain_line("https://example.com/"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_strips_http() {
        assert_eq!(
            parse_domain_line("http://example.com"),
            Some("example.com".to_string())
        );
        assert_eq!(
            parse_domain_line("http://example.com/"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_strips_trailing_slash() {
        assert_eq!(
            parse_domain_line("example.com/"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_preserves_path() {
        assert_eq!(
            parse_domain_line("example.com/path"),
            Some("example.com/path".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_trims_whitespace() {
        assert_eq!(
            parse_domain_line("  example.com  "),
            Some("example.com".to_string())
        );
        assert_eq!(
            parse_domain_line("\texample.com\n"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_empty() {
        assert_eq!(parse_domain_line(""), None);
        assert_eq!(parse_domain_line("   "), None);
        assert_eq!(parse_domain_line("\t\n"), None);
    }

    #[test]
    fn test_parse_domain_line_comment() {
        assert_eq!(parse_domain_line("# this is a comment"), None);
        assert_eq!(parse_domain_line("  # indented comment"), None);
    }

    #[test]
    fn test_parse_domain_line_invalid() {
        assert_eq!(parse_domain_line("not a valid domain!@#$"), None);
        assert_eq!(parse_domain_line("http://"), None);
        assert_eq!(parse_domain_line("://missing-scheme"), None);
    }

    #[test]
    fn test_parse_domain_line_with_port() {
        assert_eq!(
            parse_domain_line("example.com:8080"),
            Some("example.com:8080".to_string())
        );
    }

    #[test]
    fn test_parse_domain_line_international() {
        assert_eq!(
            parse_domain_line("xn--n3h.com"),
            Some("xn--n3h.com".to_string())
        );
    }

    #[test]
    fn test_parse_domains_single() {
        let content = "example.com";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com"]);
    }

    #[test]
    fn test_parse_domains_multiple() {
        let content = "example.com\ntest.org\nfoo.bar";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org", "foo.bar"]);
    }

    #[test]
    fn test_parse_domains_with_empty_lines() {
        let content = "example.com\n\ntest.org\n\n\nfoo.bar\n";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org", "foo.bar"]);
    }

    #[test]
    fn test_parse_domains_with_comments() {
        let content = "# Domain list\nexample.com\n# Another comment\ntest.org";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org"]);
    }

    #[test]
    fn test_parse_domains_mixed_protocols() {
        let content = "https://example.com\nhttp://test.org/\nfoo.bar";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org", "foo.bar"]);
    }

    #[test]
    fn test_parse_domains_filters_invalid() {
        let content = "example.com\ninvalid!@#\ntest.org";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org"]);
    }

    #[test]
    fn test_parse_domains_empty_content() {
        let content = "";
        let domains = parse_domains(content);
        assert!(domains.is_empty());
    }

    #[test]
    fn test_parse_domains_only_comments_and_empty() {
        let content = "# Comment 1\n\n# Comment 2\n   \n";
        let domains = parse_domains(content);
        assert!(domains.is_empty());
    }

    #[test]
    fn test_parse_domains_windows_line_endings() {
        let content = "example.com\r\ntest.org\r\n";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "test.org"]);
    }

    #[test]
    fn test_parse_domains_preserves_order() {
        let content = "zebra.com\nalpha.org\nmiddle.net";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["zebra.com", "alpha.org", "middle.net"]);
    }

    #[test]
    fn test_parse_domains_deduplication_not_applied() {
        let content = "example.com\nexample.com";
        let domains = parse_domains(content);
        assert_eq!(domains, vec!["example.com", "example.com"]);
    }
}
