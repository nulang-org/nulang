//! Deterministic sitemap serialization for successfully rendered static routes.
use std::collections::BTreeSet;
use std::fmt::Write;

const MAX_URLS: usize = 50_000;
const MAX_BYTES: usize = 50 * 1024 * 1024;

fn origin(site_url: &str) -> Result<&str, String> {
    let error = || {
        "web.site_url must be an HTTP(S) origin with an ASCII DNS name or IPv4 address and optional port".to_string()
    };
    let base = site_url.strip_suffix('/').unwrap_or(site_url);
    let authority = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .ok_or_else(error)?;
    let host = if let Some((host, port)) = authority.split_once(':') {
        if port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
            || !port.parse::<u16>().is_ok_and(|p| p > 0)
        {
            return Err(error());
        }
        host
    } else {
        authority
    };
    if host.len() > 253
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(error());
    }
    // Reject browser-specific alternate IPv4 spellings. Numeric final
    // labels are not DNS names in WHATWG HTTP(S) URL parsing.
    let last = host.rsplit('.').next().unwrap_or_default();
    if (last.bytes().all(|b| b.is_ascii_digit()) || last.to_ascii_lowercase().starts_with("0x"))
        && host.parse::<std::net::Ipv4Addr>().is_err()
    {
        return Err(error());
    }
    Ok(base)
}

pub(super) fn render(
    site_url: &str,
    paths: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<String, String> {
    let base = origin(site_url)?;
    let mut urls = BTreeSet::new();
    for path in paths {
        let path = path.as_ref();
        let rest = path.strip_prefix('/').ok_or("invalid sitemap route")?;
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if (path != "/"
            && (rest.is_empty()
                || rest
                    .split('/')
                    .any(|s| s.is_empty() || s == "." || s == "..")))
            || !path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~'))
        {
            return Err(format!("invalid sitemap route {path:?}"));
        }
        if base.len() + path.len() >= 2048 {
            return Err(format!(
                "sitemap URL must be shorter than 2048 bytes: {path:?}"
            ));
        }
        // Both components use a restricted ASCII alphabet without XML
        // metacharacters; no escaping or URL rewriting is necessary.
        urls.insert(format!("{base}{path}"));
        if urls.len() > MAX_URLS {
            return Err("sitemap exceeds 50,000 URLs".into());
        }
    }
    if urls.is_empty() {
        return Err("web.site_url requires at least one generated static GET page".into());
    }
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for url in urls {
        writeln!(xml, "  <url><loc>{url}</loc></url>").unwrap();
        if xml.len() + "</urlset>\n".len() > MAX_BYTES {
            return Err("sitemap exceeds 50 MiB".into());
        }
    }
    xml.push_str("</urlset>\n");
    Ok(xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_and_deduplicates_urls_without_inventing_dates() {
        let xml = render("https://example.com/", ["/z", "/", "/a/", "/z"]).unwrap();
        assert_eq!(
            xml,
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
                "<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
                "  <url><loc>https://example.com/</loc></url>\n",
                "  <url><loc>https://example.com/a/</loc></url>\n",
                "  <url><loc>https://example.com/z</loc></url>\n",
                "</urlset>\n"
            )
        );
    }

    #[test]
    fn rejects_ambiguous_and_unsafe_site_origins() {
        for origin in [
            "",
            "ftp://example.com",
            "https://",
            "https://user@example.com",
            "https://example.com/a",
            "https://example.com?query",
            "https://example.com/#frag",
            "https://example.com//",
            "https://example.com:0",
            "https://example.com:99999",
            "https://-host.example",
            "https://example.com\\evil",
            "https://example.com\n",
            "https://example.com%2f",
        ] {
            assert!(render(origin, ["/"]).is_err(), "{origin:?}");
        }
        assert!(render("http://127.0.0.1:8080", ["/"]).is_ok());
        assert!(render("https://xn--caf-dma.example", ["/"]).is_ok());
    }

    #[test]
    fn enforces_sitemap_protocol_limits_and_safe_paths() {
        assert!(render("https://example.com", std::iter::empty::<&str>()).is_err());
        assert!(render("https://example.com", ["/x&y"]).is_err());
        assert!(render("https://example.com", ["/../x"]).is_err());
        assert!(render("https://example.com", [format!("/{}", "x".repeat(2048))]).is_err());
        assert!(render("https://example.com", (0..50_001).map(|i| format!("/{i}"))).is_err());
    }
}
