//! Turbo Streams support for server-rendered Nulang HTML.
//! The payload is already-escaped HTML (a Nulang `Html` value), never user input.

pub const TURBO_STREAM_MIME: &str = "text/vnd.turbo-stream.html";

/// Return true only when the caller explicitly advertises Turbo Streams with
/// a positive quality factor. This does not treat wildcard Accept as consent.
pub fn accepts_turbo_stream(accept: &str) -> bool {
    accept.split(',').any(|item| {
        let mut parts = item.trim().split(';');
        if !parts
            .next()
            .is_some_and(|media| media.trim().eq_ignore_ascii_case(TURBO_STREAM_MIME))
        {
            return false;
        }
        let mut quality = 1.0_f64;
        for param in parts {
            let Some((name, value)) = param.trim().split_once('=') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("q") {
                let Ok(value) = value.trim().parse::<f64>() else {
                    return false;
                };
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return false;
                }
                quality = value;
            }
        }
        quality > 0.0
    })
}

/// Serialize a single Turbo Stream action. `content` must be a rendered
/// Nulang Html fragment (escaped by Web.text / Web.html, not raw input).
/// Targets are intentionally limited to safe DOM IDs, not CSS selectors.
/// Unknown actions and malformed IDs fail closed instead of generating HTML.
pub fn render_stream(action: &str, target: &str, content: &str) -> Option<String> {
    let templated = matches!(
        action,
        "append" | "prepend" | "replace" | "update" | "before" | "after"
    );
    if action == "refresh" {
        return (target.is_empty() && content.is_empty())
            .then(|| "<turbo-stream action=\"refresh\"></turbo-stream>".to_string());
    }
    if !templated && action != "remove" {
        return None;
    }
    let mut bytes = target.bytes();
    if target.len() > 128
        || !matches!(bytes.next(), Some(b) if b.is_ascii_alphanumeric() || b == b'_')
        || !bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
        || (action == "remove" && !content.is_empty())
    {
        return None;
    }
    if templated {
        Some(format!(
            "<turbo-stream action=\"{action}\" target=\"{target}\"><template>{content}</template></turbo-stream>"
        ))
    } else {
        Some(format!(
            "<turbo-stream action=\"remove\" target=\"{target}\"></turbo-stream>"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_append_and_replace_with_html_fragments() {
        assert_eq!(
            render_stream("append", "todos", "<li>Tea &amp; toast</li>").unwrap(),
            "<turbo-stream action=\"append\" target=\"todos\"><template><li>Tea &amp; toast</li></template></turbo-stream>"
        );
        assert_eq!(
            render_stream("replace", "todo-42", "<p>Saved</p>").unwrap(),
            "<turbo-stream action=\"replace\" target=\"todo-42\"><template><p>Saved</p></template></turbo-stream>"
        );
    }

    #[test]
    fn remove_has_no_template_and_refresh_has_no_target() {
        assert_eq!(
            render_stream("remove", "todo-42", "").unwrap(),
            "<turbo-stream action=\"remove\" target=\"todo-42\"></turbo-stream>"
        );
        assert_eq!(
            render_stream("refresh", "", "").unwrap(),
            "<turbo-stream action=\"refresh\"></turbo-stream>"
        );
    }

    #[test]
    fn rejects_markup_injection_and_unknown_actions() {
        for target in [
            "",
            "a\" onload=\"alert(1)",
            "with space",
            "<script>",
            "a\nb",
        ] {
            assert!(
                render_stream("append", target, "<p>ok</p>").is_none(),
                "{target}"
            );
        }
        for action in ["execute", "append\" onload=\"x", "APPEND"] {
            assert!(render_stream(action, "todos", "").is_none(), "{action}");
        }
        assert!(render_stream("refresh", "todos", "").is_none());
        assert!(render_stream("remove", "todo-42", "<script>x</script>").is_none());
    }

    #[test]
    fn exposes_canonical_mime_and_accept_negotiation() {
        assert_eq!(TURBO_STREAM_MIME, "text/vnd.turbo-stream.html");
        assert!(accepts_turbo_stream(
            "text/html, text/vnd.turbo-stream.html"
        ));
        assert!(accepts_turbo_stream("text/vnd.turbo-stream.html; q=0.9"));
        assert!(!accepts_turbo_stream("text/html"));
        assert!(!accepts_turbo_stream("text/vnd.turbo-stream.html; q=0"));
        assert!(!accepts_turbo_stream("text/vnd.turbo-stream.html.evil"));
    }
}
