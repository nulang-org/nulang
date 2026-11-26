//! CommonMark fragments for local content. Raw HTML is always escaped. URL
//! checks operate on decoded parser events, before the HTML writer escapes
//! attributes; never sanitize the generated HTML with string replacement.

use crate::web::html_safety::safe_logical_url;
use crate::web::ir::DeploymentIr;
use crate::web::runtime_bindings::RuntimeWebRoute;
use pulldown_cmark::{html, Event, LinkType, Parser, Tag};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

pub(super) struct Markdown {
    pub html: String,
    pub root_links: Vec<RootLink>,
}

pub(super) struct RootLink {
    source: String,
    url: String,
    path: String,
    relative: PathBuf,
}

pub(super) fn render(body: &str, source: &str) -> Result<Markdown, String> {
    let mut events = Vec::new();
    let mut root_links = Vec::new();
    for event in Parser::new(body) {
        match &event {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            })
            | Event::Start(Tag::Image {
                link_type,
                dest_url,
                ..
            }) => {
                let email = *link_type == LinkType::Email;
                // The writer prepends mailto: for email autolinks.
                let url = if email {
                    format!("mailto:{dest_url}")
                } else {
                    dest_url.to_string()
                };
                if !safe_logical_url(&url) || url.starts_with("//") {
                    return Err(format!("{source}: unsafe Markdown URL {url:?}"));
                }
                if url.starts_with('/') {
                    let path = url.split(['?', '#']).next().unwrap_or_default().to_owned();
                    let relative = root_path(&path).ok_or_else(|| {
                        format!("{source}: invalid root-relative Markdown URL {url:?}")
                    })?;
                    root_links.push(RootLink {
                        source: source.to_owned(),
                        url,
                        path,
                        relative,
                    });
                }
            }
            _ => {}
        }

        // Pulldown's HTML writer treats these two events as trusted markup.
        // Convert both to ordinary escaped text rather than enabling raw HTML.
        events.push(match event {
            Event::Html(value) | Event::InlineHtml(value) => Event::Text(value),
            other => other,
        });
    }
    let mut output = String::new();
    html::push_html(&mut output, events.into_iter());
    Ok(Markdown {
        html: output,
        root_links,
    })
}

/// Decode each individual URL segment once, never a full path. An encoded
/// separator, dot segment, control byte or drive prefix cannot become a
/// filesystem lookup outside the staged output.
fn root_path(path: &str) -> Option<PathBuf> {
    let inner = path.strip_prefix('/')?;
    let inner = inner.strip_suffix('/').unwrap_or(inner);
    let mut result = PathBuf::new();
    if inner.is_empty() {
        return Some(result);
    }
    for raw in inner.split('/') {
        if raw.is_empty() {
            return None;
        }
        // Reject malformed percent escapes instead of treating ambiguous
        // URL spelling as a different local asset name.
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                if i + 2 >= bytes.len()
                    || !bytes[i + 1].is_ascii_hexdigit()
                    || !bytes[i + 2].is_ascii_hexdigit()
                {
                    return None;
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        let segment = crate::web::request_bindings::percent_decode_path(raw);
        if segment.ends_with([' ', '.'])
            || segment.contains(['/', '\\', ':', '\u{fffd}'])
            || segment.chars().any(char::is_control)
            || !matches!(
                Path::new(&segment)
                    .components()
                    .collect::<Vec<_>>()
                    .as_slice(),
                [Component::Normal(_)]
            )
        {
            return None;
        }
        result.push(segment);
    }
    Some(result)
}

/// Root-relative URLs have an unambiguous base independent of the handler.
/// Check the final deployment contract (not the unexpanded route templates)
/// and regular files in the staged output. Relative links and fragments
/// cannot be resolved soundly without knowing where each fragment is embedded.
pub(super) fn validate_root_links(
    links: &[RootLink],
    ir: &DeploymentIr,
    runtime_routes: &[RuntimeWebRoute],
    output: &Path,
) -> Result<(), String> {
    // Do not let a collection's original wildcard make an unlisted
    // page appear reachable. Only routes retained as server handlers in
    // deployment IR are eligible for dynamic matching.
    let deployed: BTreeSet<&str> = ir
        .routes
        .iter()
        .filter(|r| r.method == "GET" && r.placement == "server")
        .map(|r| r.path.as_str())
        .collect();
    let server_routes: Vec<_> = runtime_routes
        .iter()
        .filter(|r| r.route.method.as_str() == "GET" && deployed.contains(r.route.path.as_str()))
        .collect();
    let mut errors = Vec::new();
    for link in links {
        let target = output.join(&link.relative);
        let file_exists =
            (!link.path.ends_with('/') && target.is_file()) || target.join("index.html").is_file();
        if file_exists
            || server_routes
                .iter()
                .any(|route| crate::web::dispatch::match_route(route, &link.path).is_some())
        {
            continue;
        }
        errors.push(format!(
            "{}: broken root-relative Markdown URL {:?}",
            link.source, link.url
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        errors.sort();
        errors.dedup();
        Err(errors.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_standard_commonmark_and_escapes_raw_html() {
        let rendered = render("# Title\n\n**bold** and *em*\n\n- item\n\n<script>alert(1)</script>\n\nHello <img src=x onerror=alert(1)>\n\n```rust\nlet x = 1 < 2;\n```\n", "doc.md").unwrap();
        assert!(rendered.html.contains("<h1>Title</h1>"));
        assert!(rendered.html.contains("<strong>bold</strong>"));
        assert!(rendered.html.contains("<em>em</em>"));
        assert!(rendered.html.contains("<li>item</li>"));
        assert!(rendered
            .html
            .contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(rendered.html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(rendered.html.contains("let x = 1 &lt; 2;"));
        assert!(!rendered.html.contains("<script>"));
        assert!(!rendered.html.contains("<img"));
    }

    #[test]
    fn validates_actual_link_and_image_destinations() {
        for body in [
            "[x](javascript:alert%281%29)",
            "[x](JaVaScRiPt&colon;alert%281%29)",
            "[x](javascript&#58;alert%281%29)",
            "![x](data:image/svg+xml;base64,xyz)",
            "[x](vbscript:run)",
            "[x](file:///etc/passwd)",
            "[x](//example.com/x)",
            "[ref][bad]\n\n[bad]: javascript:alert%281%29",
            "[x](java&#9;script:alert)",
            "[x](https:\\\\example.com)",
            "[x](/a/%2e%2e/secret)",
            "![x](/a/%2fsecret)",
            "[x](/a/..%20/secret)",
        ] {
            assert!(render(body, "doc.md").is_err(), "{body}");
        }
        let result = render("[web](https://example.com/?a=1&b=2) [local](/docs/a?q=1#top) ![photo](/img/caf%C3%A9.png) <me@example.com> [relative](next/) [anchor](#top)", "doc.md").unwrap();
        assert!(result.html.contains("https://example.com/?a=1&amp;b=2"));
        assert!(result.html.contains("mailto:me@example.com"));
        assert_eq!(result.root_links.len(), 2);
        assert_eq!(
            result.root_links[1].relative,
            PathBuf::from("img").join("café.png")
        );
    }
}
