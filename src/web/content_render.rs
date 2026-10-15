//! Safe, deterministic HTML rendering for the restricted Markdown content v1.
//!
//! This is an explicit small subset, not an implementation of CommonMark.
//! No file I/O, route registration, third-party scripts, or raw HTML pass-through.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::content::parse_page;

    fn page(md: &str) -> ContentPage {
        let source = format!("+++\ntitle = \"A < B & C\"\n+++\n{md}");
        parse_page("blog/example.md", &source).unwrap()
    }

    #[test]
    fn renders_document_with_escaped_metadata_and_heading() {
        let page = page("# Hello & <world>\n\nA paragraph with `code`.\n");
        let html = render_content_document(
            &page,
            &ContentLayout {
                site_title: "Site \"Name\"",
                language: "en-US",
            },
        )
        .unwrap();
        assert!(html.starts_with("<!doctype html>\n<html lang=\"en-US\">"));
        assert!(html.contains("<title>A &lt; B &amp; C — Site &quot;Name&quot;</title>"));
        assert!(html.contains("<h1>Hello &amp; &lt;world&gt;</h1>"));
        assert!(html.contains("<p>A paragraph with <code>code</code>.</p>"));
        assert!(!html.contains("<script>"));
        assert!(html.ends_with("</article></main></body></html>\n"));
    }

    #[test]
    fn escaped_code_fences_cannot_insert_active_html() {
        let page = page("```rust\n<script>alert(1)</script>\nlet x = \"<tag>\";\n```\n");
        let html = render_content_document(&page, &ContentLayout::default()).unwrap();
        assert!(html.contains("<pre><code class=\"language-rust\">"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("let x = &quot;&lt;tag&gt;&quot;;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn groups_lists_and_multiline_paragraphs_without_losing_order() {
        let page = page("Line one\nline two\n\n- First\n- Second\n\n## Next\nDone.");
        let html = render_content_document(&page, &ContentLayout::default()).unwrap();
        assert!(html.contains("<p>Line one line two</p>"));
        assert!(html.contains("<ul><li>First</li><li>Second</li></ul>"));
        assert!(html.contains("<h2>Next</h2><p>Done.</p>"));
    }

    #[test]
    fn refuses_drafts_even_when_passed_directly_to_renderer() {
        let mut draft = page("# Private");
        draft.draft = true;
        let error = render_content_document(&draft, &ContentLayout::default()).unwrap_err();
        assert!(error.contains("draft"), "{error}");
    }

    #[test]
    fn rejects_unfinished_fence_and_unsafe_language_identifiers() {
        let missing_end = page("```rust\nlet x = 1;\n");
        assert!(render_content_document(&missing_end, &ContentLayout::default())
            .unwrap_err()
            .contains("unclosed"));
        let unsafe_language = page("```rust\" onmouseover=\"alert(1)\nunsafe\n```\n");
        assert!(render_content_document(&unsafe_language, &ContentLayout::default())
            .unwrap_err()
            .contains("fence language"));
    }

    #[test]
    fn rejects_unsupported_markup_instead_of_silently_mangling_pages() {
        for source in [
            "[link](https://example.com)",
            "![cover](image.png)",
            "**bold**",
            "A paragraph with an unmatched ` delimiter",
        ] {
            let error =
                render_content_document(&page(source), &ContentLayout::default()).unwrap_err();
            assert!(error.contains("unsupported"), "{source}: {error}");
        }
    }

    #[test]
    fn validates_language_and_required_site_title() {
        let page = page("# Intro");
        let dangerous_language = ContentLayout {
            site_title: "Site",
            language: "en\" onload=\"alert(1)",
        };
        assert!(render_content_document(&page, &dangerous_language)
            .unwrap_err()
            .contains("language"));
        let blank_title = ContentLayout {
            site_title: "   ",
            language: "en",
        };
        assert!(render_content_document(&page, &blank_title)
            .unwrap_err()
            .contains("site title"));
    }

    #[test]
    fn renders_all_heading_levels_without_trusting_markdown_html() {
        let md = "# One\n## Two\n### Three\n#### Four\n##### Five\n###### Six\n\n<script>x</script>";
        let html = render_content_document(&page(md), &ContentLayout::default()).unwrap();
        for level in 1..=6 {
            assert!(html.contains(&format!("<h{level}>")));
        }
        assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
    }
}
