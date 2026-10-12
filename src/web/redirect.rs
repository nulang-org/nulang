//! Conservative target policy for upgrading a Web.redirect HTML value to
//! an HTTP Location header. The language-level fallback renderer remains in
//! src/runtime/callbacks.rs and retains its compatibility and safety policy.

/// HTTP 303 upgrades require a same-origin absolute ASCII path. Relative and
/// external redirects keep the already-escaped HTML fallback from PR #1500.
pub fn is_safe_redirect_target(url: &str) -> bool {
    url.starts_with('/')
        && !url.starts_with("//")
        && !url.contains('\\')
        && url.bytes().all(|byte| byte.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::is_safe_redirect_target;

    #[test]
    fn accepts_safe_same_origin_absolute_paths() {
        for target in [
            "/",
            "/tasks",
            "/tasks?tab=mine&sort=recent",
            "/users/42#activity",
        ] {
            assert!(is_safe_redirect_target(target), "rejected {target:?}");
        }
    }

    #[test]
    fn refuses_external_relative_and_header_breakout_targets() {
        for target in [
            "",
            "tasks",
            "//evil.example",
            "https://evil.example",
            "javascript:alert(1)",
            "/\\evil.example",
            "/\r\nLocation: https://evil.example",
            "/ok\nX-Header: injected",
            "/ok\u{007f}",
            "/caf\u{00e9}",
        ] {
            assert!(!is_safe_redirect_target(target), "accepted {target:?}");
        }
    }
}
