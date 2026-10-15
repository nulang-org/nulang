//! Conservative host-side HTML serialization for Web.html.
//!
//! Markup and attribute values reach this boundary after Nulang's Html
//! values have already been rendered. The serializer must not treat dynamic
//! tag names, attribute names, or URL-bearing attributes as trusted.

#[cfg(test)]
mod tests {
    use super::render_element;

    fn attrs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn normal_markup_keeps_preescaped_text_and_attributes() {
        let output = render_element(
            "div",
            &attrs(&[("class", "card"), ("title", "A&amp;B")]),
            &["&lt;Hello&gt;".to_string()],
        );
        assert_eq!(
            output.as_deref(),
            Some("<div class=\"card\" title=\"A&amp;B\">&lt;Hello&gt;</div>")
        );
    }

    #[test]
    fn dynamic_tag_names_cannot_inject_markup_or_active_content() {
        assert!(render_element("img src=x onerror=alert(1)", &[], &[]).is_none());
        assert!(render_element("script", &[], &["alert(1)".to_string()]).is_none());
        assert!(render_element("svg", &[], &[]).is_none());
        assert!(render_element("my-card", &[], &[]).is_some());
    }

    #[test]
    fn attribute_names_cannot_inject_handlers_or_markup() {
        assert!(render_element("img", &attrs(&[("onerror", "alert(1)")]), &[]).is_none());
        assert!(render_element("img", &attrs(&[("OnLoad", "alert(1)")]), &[]).is_none());
        assert!(render_element("p", &attrs(&[("title\" autofocus", "x")]), &[]).is_none());
        assert!(render_element("div", &attrs(&[("srcdoc", "<script>")]), &[]).is_none());
        assert!(render_element("div", &attrs(&[("style", "background:url(javascript:x)")]), &[]).is_none());
    }

    #[test]
    fn attribute_values_cannot_break_out_of_quoted_context() {
        let html = render_element(
            "div",
            &attrs(&[("title", "hello\" onclick=\"alert(1)")]),
            &[],
        )
        .expect("quoted attribute stays safe");
        assert_eq!(
            html,
            "<div title=\"hello&quot; onclick=&quot;alert(1)\"></div>"
        );
    }

    #[test]
    fn URL_attributes_fail_closed_on_script_and_entity_obfuscation() {
        for url in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "java&#x73;cript:alert(1)",
            "javascript&#58;alert(1)",
            "java&colon;script:alert(1)",
            "\tjavascript:alert(1)",
            "java\nscript:alert(1)",
        ] {
            assert!(render_element("a", &attrs(&[("href", url)]), &[]).is_none(), "{url}");
        }
        assert!(render_element("img", &attrs(&[("srcset", "x.jpg 1x")]), &[]).is_none());
        assert!(render_element("button", &attrs(&[("formaction", "javascript:x")]), &[]).is_none());
    }

    #[test]
    fn safe_URLs_and_document_metadata_are_preserved() {
        for url in ["/docs?q=a&amp;b=2", "https://example.com/a", "#section", "mailto:a@example.com"] {
            assert!(render_element("a", &attrs(&[("href", url)]), &[]).is_some(), "{url}");
        }
        assert!(render_element("meta", &attrs(&[("name", "description"), ("content", "Safe")]), &[]).is_some());
        assert!(render_element("form", &attrs(&[("method", "POST"), ("action", "/submit")]), &[]).is_some());
    }
}
