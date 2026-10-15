//! Safe, deterministic HTML rendering for the restricted Markdown content v1.
//!
//! This is an explicit small subset, not an implementation of CommonMark.
//! No file I/O, route registration, third-party scripts, or raw HTML pass-through.

use crate::web::content::ContentPage;

/// Shared document wrapper for content pages and sections.
#[derive(Clone, Copy, Debug)]
pub struct ContentLayout<'a> {
    pub site_title: &'a str,
    pub language: &'a str,
}

impl Default for ContentLayout<'_> {
    fn default() -> Self {
        Self {
            site_title: "Nulang",
            language: "en",
        }
    }
}

/// Render a published content entry as a standalone, escaped HTML document.
///
/// Markdown v1 intentionally supports only ATX headings, paragraphs, unordered
/// lists, fenced code blocks, and inline code spans. Unsupported link/image/
/// emphasis syntax fails closed rather than being presented as rendered markup.
/// No content can inject trusted HTML; later CommonMark support should replace
/// this parser with a maintained library while retaining the document contract.
pub fn render_content_document(
    page: &ContentPage,
    layout: &ContentLayout<'_>,
) -> Result<String, String> {
    if page.draft {
        return Err(format!(
            "cannot render draft content {}",
            page.source_path
        ));
    }
    if layout.site_title.trim().is_empty() {
        return Err("site title must not be empty".to_string());
    }
    if layout.language.is_empty()
        || layout.language.starts_with('-')
        || layout.language.ends_with('-')
        || !layout
            .language
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("invalid document language".to_string());
    }

    let body = render_markdown_blocks(&page.body)
        .map_err(|error| format!("{}: {error}", page.source_path))?;
    let title = format!("{} — {}", page.title, layout.site_title);

    let mut html = String::from("<!doctype html>\n<html lang=\"");
    html.push_str(&escape_html(layout.language));
    html.push_str("\"><head><meta charset=\"utf-8\">");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    html.push_str("<title>");
    html.push_str(&escape_html(&title));
    html.push_str("</title></head><body><main><article>");
    html.push_str(&body);
    html.push_str("</article></main></body></html>\n");
    Ok(html)
}

fn render_markdown_blocks(markdown: &str) -> Result<String, String> {
    let mut html = String::new();
    let mut paragraph = Vec::new();
    let mut list = Vec::new();
    let mut fence_language: Option<String> = None;
    let mut code = String::new();

    for (offset, line) in markdown.lines().enumerate() {
        let line_number = offset + 1;
        if let Some(language) = &fence_language {
            if line.trim() == "```" {
                html.push_str("<pre><code");
                if !language.is_empty() {
                    html.push_str(" class=\"language-");
                    html.push_str(language);
                    html.push('"');
                }
                html.push('>');
                html.push_str(&escape_html(&code));
                html.push_str("</code></pre>");
                fence_language = None;
                code.clear();
            } else {
                code.push_str(line);
                code.push('\n');
            }
            continue;
        }

        if let Some(language) = line.strip_prefix("```") {
            flush_paragraph(&mut paragraph, &mut html)?;
            flush_list(&mut list, &mut html)?;
            let language = language.trim();
            if !language.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'-'
                    || b == b'_'
                    || b == b'+'
            }) {
                return Err(format!("invalid fence language at line {line_number}"));
            }
            fence_language = Some(language.to_string());
            continue;
        }

        if line.trim().is_empty() {
            flush_paragraph(&mut paragraph, &mut html)?;
            flush_list(&mut list, &mut html)?;
            continue;
        }

        if let Some((level, heading)) = heading_line(line) {
            flush_paragraph(&mut paragraph, &mut html)?;
            flush_list(&mut list, &mut html)?;
            html.push_str(&format!("<h{level}>"));
            html.push_str(&render_inline(heading)?);
            html.push_str(&format!("</h{level}>"));
            continue;
        }

        if let Some(item) = line.strip_prefix("- ") {
            flush_paragraph(&mut paragraph, &mut html)?;
            list.push(item);
            continue;
        }

        flush_list(&mut list, &mut html)?;
        paragraph.push(line);
    }

    if fence_language.is_some() {
        return Err("unclosed Markdown code fence".to_string());
    }
    flush_paragraph(&mut paragraph, &mut html)?;
    flush_list(&mut list, &mut html)?;
    Ok(html)
}

fn heading_line(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&level) || line.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    Some((level, line[level + 1..].trim()))
}

fn flush_paragraph(lines: &mut Vec<&str>, html: &mut String) -> Result<(), String> {
    if lines.is_empty() {
        return Ok(());
    }
    html.push_str("<p>");
    html.push_str(&render_inline(&lines.join(" "))?);
    html.push_str("</p>");
    lines.clear();
    Ok(())
}

fn flush_list(items: &mut Vec<&str>, html: &mut String) -> Result<(), String> {
    if items.is_empty() {
        return Ok(());
    }
    html.push_str("<ul>");
    for item in items.iter() {
        html.push_str("<li>");
        html.push_str(&render_inline(item)?);
        html.push_str("</li>");
    }
    html.push_str("</ul>");
    items.clear();
    Ok(())
}

fn render_inline(text: &str) -> Result<String, String> {
    let spans: Vec<&str> = text.split('`').collect();
    if spans.len() % 2 == 0 {
        return Err("unsupported unmatched inline code delimiter".to_string());
    }
    let mut html = String::new();
    for (index, span) in spans.iter().enumerate() {
        if index % 2 == 1 {
            if span.is_empty() {
                return Err("unsupported empty inline code span".to_string());
            }
            html.push_str("<code>");
            html.push_str(&escape_html(span));
            html.push_str("</code>");
        } else {
            if span.contains("](") || span.contains("![") || span.contains("**") {
                return Err("unsupported Markdown inline syntax".to_string());
            }
            html.push_str(&escape_html(span));
        }
    }
    Ok(html)
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

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
