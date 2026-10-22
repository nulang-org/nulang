//! Conservative host-side HTML serialization for Web.html.
//!
//! Markup and attribute values reach this boundary after Nulang's Html
//! values have already been rendered. The serializer must not treat dynamic
//! tag names, attribute names, or URL-bearing attributes as trusted.

/// Render an HTML element from already-rendered Nulang `Html` fragments.
///
/// `Web.text` escapes text before this point; do not encode '&' again or
/// legitimate `&amp;` values become corrupted. Validate names and URL
/// attributes, and escape delimiter characters in quoted attribute values.
/// `Web.raw` remains an explicitly trusted escape hatch for HTML children.
pub(crate) fn render_element(
    tag: &str,
    attrs: &[(String, String)],
    children: &[String],
) -> Option<String> {
    if !safe_identifier(tag) || dangerous_tag(tag) {
        return None;
    }

    // Validate all attributes before constructing any output.
    for (name, value) in attrs {
        if !safe_identifier(name) || dangerous_attribute(name) {
            return None;
        }
        if url_attribute(name) && !safe_url_attribute(value) {
            return None;
        }
    }

    let mut html = String::new();
    html.push('<');
    html.push_str(tag);
    for (name, value) in attrs {
        html.push(' ');
        html.push_str(name);
        html.push_str("=\"");
        html.push_str(&escape_attribute_delimiters(value));
        html.push('"');
    }
    html.push('>');
    for child in children {
        html.push_str(child);
    }
    html.push_str("</");
    html.push_str(tag);
    html.push('>');
    Some(html)
}

fn safe_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(first) if first.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':'))
}

fn dangerous_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "script"
            | "style"
            | "iframe"
            | "object"
            | "embed"
            | "svg"
            | "math"
            | "foreignobject"
            | "template"
            | "base"
            | "xmp"
            | "plaintext"
            | "noscript"
            | "frame"
            | "frameset"
            | "applet"
    )
}

fn dangerous_attribute(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("on")
        || matches!(
            lower.as_str(),
            "style" | "srcdoc" | "srcset" | "imagesrcset" | "ping" | "http-equiv"
        )
}

fn url_attribute(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "href"
            | "src"
            | "action"
            | "formaction"
            | "poster"
            | "cite"
            | "data"
            | "longdesc"
            | "background"
            | "manifest"
            | "xlink:href"
    )
}

fn safe_url_attribute(value: &str) -> bool {
    if value.trim() != value || value.contains('\\') || value.chars().any(char::is_control) {
        return false;
    }
    // The host receives pre-rendered Html. Allow the HTML-safe ampersands
    // produced by Web.text; reject all other references that could conceal
    // a colon or letters of a scriptable scheme (e.g. &#58;, &colon;).
    let mut rest = value;
    while let Some(index) = rest.find('&') {
        if !rest[index..].starts_with("&amp;") {
            return false;
        }
        rest = &rest[index + "&amp;".len()..];
    }

    if let Some((prefix, _)) = value.split_once(':') {
        // A slash, query or fragment before ':' means a relative URL.
        if !prefix.contains(&['/', '?', '#'][..]) {
            return matches!(
                prefix.to_ascii_lowercase().as_str(),
                "http" | "https" | "mailto" | "tel"
            );
        }
    }
    true
}

fn escape_attribute_delimiters(value: &str) -> String {
    value
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

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
        assert!(render_element(
            "div",
            &attrs(&[("style", "background:url(javascript:x)")]),
            &[]
        )
        .is_none());
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
    fn url_attributes_fail_closed_on_script_and_entity_obfuscation() {
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
            assert!(
                render_element("a", &attrs(&[("href", url)]), &[]).is_none(),
                "{url}"
            );
        }
        assert!(render_element("img", &attrs(&[("srcset", "x.jpg 1x")]), &[]).is_none());
        assert!(render_element("button", &attrs(&[("formaction", "javascript:x")]), &[]).is_none());
    }

    #[test]
    fn safe_urls_and_document_metadata_are_preserved() {
        for url in [
            "/docs?q=a&amp;b=2",
            "https://example.com/a",
            "#section",
            "mailto:a@example.com",
        ] {
            assert!(
                render_element("a", &attrs(&[("href", url)]), &[]).is_some(),
                "{url}"
            );
        }
        assert!(render_element(
            "meta",
            &attrs(&[("name", "description"), ("content", "Safe")]),
            &[]
        )
        .is_some());
        assert!(render_element(
            "form",
            &attrs(&[("method", "POST"), ("action", "/submit")]),
            &[]
        )
        .is_some());
    }
}
