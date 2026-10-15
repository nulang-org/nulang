//! Experimental typed Markdown content metadata.
//! This module does not register routes or render Markdown.
#[cfg(test)]
mod tests {
    use super::parse_page;

    #[test]
    fn parses_typed_front_matter_and_preserves_markdown() {
        let page = parse_page(
            "blog/hello.md",
            "+++\ntitle = \"Hello\"\ndraft = true\ntags = [\"nulang\", \"web\"]\n+++\n# Hello\n",
        ).unwrap();
        assert_eq!(page.slug, "/blog/hello");
        assert_eq!(page.title, "Hello");
        assert!(page.draft);
        assert_eq!(page.tags, vec!["nulang", "web"]);
        assert_eq!(page.body, "# Hello\n");
    }

    #[test]
    fn page_bundle_index_has_same_slug_as_leaf_file() {
        let source = "+++\ntitle = \"Post\"\n+++\nBody";
        let leaf = parse_page("blog/post.md", source).unwrap();
        let bundle = parse_page("blog/post/index.md", source).unwrap();
        assert_eq!(leaf.slug, bundle.slug);
        assert!(!bundle.draft);
        assert!(bundle.tags.is_empty());
    }

    #[test]
    fn rejects_invalid_typed_front_matter() {
        assert!(parse_page("post.md", "# Missing metadata").is_err());
        assert!(parse_page("post.md", "+++\ndraft = true\n+++\nBody").is_err());
        assert!(parse_page("post.md", "+++\ntitle = 2\n+++\nBody").is_err());
        assert!(parse_page("post.md", "+++\ntitle = \"Ok\"\ntags = \"not-array\"\n+++\nBody").is_err());
        assert!(parse_page("post.md", "+++\ntitle = \"Incomplete\"\nBody").is_err());
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_paths() {
        let source = "+++\ntitle = \"Safe\"\n+++\nBody";
        for path in ["../post.md", "/absolute.md", "blog/Bad Name.md", "blog\\post.md", "blog/./post.md", "blog//post.md"] {
            assert!(parse_page(path, source).is_err(), "{path}");
        }
        assert!(parse_page("blog/post.txt", source).is_err());
    }
}
