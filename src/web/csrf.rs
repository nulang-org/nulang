//! Browser request provenance validation for experimental Nulang Web.
//!
//! This is a strict Origin/Referer + Fetch Metadata CSRF defense for HTTP
//! mutations, not application authentication or resource authorization.
//! Missing or ambiguous provenance fails closed; automated API clients must
//! supply a matching Origin header. Never trust X-Forwarded-* from clients.

use std::net::SocketAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginCheckError {
    MissingHost,
    DuplicateHeader,
    UntrustedHost,
    MissingProvenance,
    OriginMismatch,
    CrossOriginFetch,
    InvalidConfiguration,
}

fn single_header<'a>(
    headers: &'a [(String, String)],
    name: &str,
) -> Result<Option<&'a str>, OriginCheckError> {
    let mut found = None;
    for (header, value) in headers {
        if header.eq_ignore_ascii_case(name) {
            if found.is_some() {
                return Err(OriginCheckError::DuplicateHeader);
            }
            found = Some(value.as_str());
        }
    }
    Ok(found)
}

/// A configured origin must consist of exactly scheme and authority. Paths,
/// userinfo, fragments and query strings are never valid trusted origins.
fn configured_authority(origin: &str) -> Result<&str, OriginCheckError> {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .ok_or(OriginCheckError::InvalidConfiguration)?;
    if rest.is_empty()
        || rest
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || rest
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b'@' | 92))
        || rest.starts_with(':')
    {
        return Err(OriginCheckError::InvalidConfiguration);
    }
    Ok(rest)
}

fn direct_host_allowed(host: &str, local: SocketAddr) -> bool {
    let direct = local.to_string();
    if host.eq_ignore_ascii_case(&direct) {
        return true;
    }
    if local.port() == 80 && host.eq_ignore_ascii_case(&local.ip().to_string()) {
        return true;
    }
    if local.ip().is_loopback() {
        let localhost = if local.port() == 80 {
            "localhost".to_string()
        } else {
            format!("localhost:{}", local.port())
        };
        return host.eq_ignore_ascii_case(&localhost);
    }
    false
}

/// Verify provenance before invoking any state-changing Web route.
///
/// Configure public_origin for a trusted HTTPS reverse proxy. Without it,
/// only the local listening IP or localhost are admitted to prevent malicious
/// DNS-rebinding Host spoofing. Referer is fallback when Origin is absent.
pub fn validate_mutation_origin(
    method: &str,
    headers: &[(String, String)],
    local_addr: Option<SocketAddr>,
    public_origin: Option<&str>,
) -> Result<(), OriginCheckError> {
    if matches!(method, "GET" | "HEAD" | "OPTIONS") {
        return Ok(());
    }

    let host = single_header(headers, "Host")?.ok_or(OriginCheckError::MissingHost)?;
    let expected = if let Some(origin) = public_origin {
        let trusted_authority = configured_authority(origin)?;
        if !host.eq_ignore_ascii_case(trusted_authority) {
            return Err(OriginCheckError::UntrustedHost);
        }
        origin.to_string()
    } else {
        let local = local_addr.ok_or(OriginCheckError::UntrustedHost)?;
        if !direct_host_allowed(host, local) {
            return Err(OriginCheckError::UntrustedHost);
        }
        format!("http://{host}")
    };

    if let Some(site) = single_header(headers, "Sec-Fetch-Site")? {
        if site != "same-origin" && site != "none" {
            return Err(OriginCheckError::CrossOriginFetch);
        }
    }

    if let Some(origin) = single_header(headers, "Origin")? {
        return if origin == expected {
            Ok(())
        } else {
            Err(OriginCheckError::OriginMismatch)
        };
    }

    let referer = single_header(headers, "Referer")?.ok_or(OriginCheckError::MissingProvenance)?;
    let path = referer.strip_prefix(&expected);
    if referer == expected || path.is_some_and(|suffix| suffix.starts_with('/')) {
        Ok(())
    } else {
        Err(OriginCheckError::OriginMismatch)
    }
}

// Origin policy regression tests for the experimental HTTP server.

#[cfg(test)]
mod tests {
    use super::{validate_mutation_origin, OriginCheckError};
    use std::net::SocketAddr;

    fn loopback() -> SocketAddr {
        "127.0.0.1:8080".parse().unwrap()
    }

    fn headers(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn safe_requests_need_no_origin_or_host() {
        for method in ["GET", "HEAD", "OPTIONS"] {
            assert_eq!(validate_mutation_origin(method, &[], None, None), Ok(()));
        }
    }

    #[test]
    fn same_origin_mutation_is_allowed_for_loopback_and_localhost() {
        for host in ["localhost:8080", "127.0.0.1:8080"] {
            let request = headers(&[
                ("Host", host),
                ("Origin", &format!("http://{host}")),
                ("Sec-Fetch-Site", "same-origin"),
            ]);
            assert_eq!(
                validate_mutation_origin("POST", &request, Some(loopback()), None),
                Ok(())
            );
        }
    }

    #[test]
    fn missing_and_foreign_origin_fail_closed() {
        let missing = headers(&[("Host", "localhost:8080")]);
        assert_eq!(
            validate_mutation_origin("POST", &missing, Some(loopback()), None),
            Err(OriginCheckError::MissingProvenance)
        );
        let foreign = headers(&[
            ("Host", "localhost:8080"),
            ("Origin", "https://attacker.example"),
        ]);
        assert_eq!(
            validate_mutation_origin("POST", &foreign, Some(loopback()), None),
            Err(OriginCheckError::OriginMismatch)
        );
        let null = headers(&[("Host", "localhost:8080"), ("Origin", "null")]);
        assert!(validate_mutation_origin("POST", &null, Some(loopback()), None).is_err());
    }

    #[test]
    fn referer_fallback_requires_an_exact_origin_boundary() {
        let valid = headers(&[
            ("Host", "localhost:8080"),
            ("Referer", "http://localhost:8080/path?query=yes"),
        ]);
        assert_eq!(
            validate_mutation_origin("PATCH", &valid, Some(loopback()), None),
            Ok(())
        );
        let spoofed = headers(&[
            ("Host", "localhost:8080"),
            ("Referer", "http://localhost:8080.attacker.example/path"),
        ]);
        assert!(validate_mutation_origin("PATCH", &spoofed, Some(loopback()), None).is_err());
    }

    #[test]
    fn cross_site_fetch_metadata_overrides_a_claimed_same_origin() {
        let request = headers(&[
            ("Host", "localhost:8080"),
            ("Origin", "http://localhost:8080"),
            ("Sec-Fetch-Site", "same-site"),
        ]);
        assert_eq!(
            validate_mutation_origin("DELETE", &request, Some(loopback()), None),
            Err(OriginCheckError::CrossOriginFetch)
        );
    }

    #[test]
    fn dns_rebinding_and_duplicate_provenance_headers_fail_closed() {
        let rebound = headers(&[
            ("Host", "attacker.example:8080"),
            ("Origin", "http://attacker.example:8080"),
        ]);
        assert_eq!(
            validate_mutation_origin("POST", &rebound, Some(loopback()), None),
            Err(OriginCheckError::UntrustedHost)
        );
        let duplicate = headers(&[
            ("Host", "localhost:8080"),
            ("Origin", "http://localhost:8080"),
            ("Origin", "http://attacker.example"),
        ]);
        assert_eq!(
            validate_mutation_origin("POST", &duplicate, Some(loopback()), None),
            Err(OriginCheckError::DuplicateHeader)
        );
    }

    #[test]
    fn configured_https_public_origin_supports_trusted_reverse_proxies() {
        let valid = headers(&[
            ("Host", "app.example.com"),
            ("Origin", "https://app.example.com"),
            ("Sec-Fetch-Site", "same-origin"),
        ]);
        assert_eq!(
            validate_mutation_origin(
                "POST",
                &valid,
                Some(loopback()),
                Some("https://app.example.com")
            ),
            Ok(())
        );
        assert_eq!(
            validate_mutation_origin(
                "POST",
                &valid,
                Some(loopback()),
                Some("https://app.example.com/unsafe/path")
            ),
            Err(OriginCheckError::InvalidConfiguration)
        );
        let injected_host = headers(&[
            ("Host", "attacker.example"),
            ("Origin", "https://app.example.com"),
        ]);
        assert_eq!(
            validate_mutation_origin(
                "POST",
                &injected_host,
                Some(loopback()),
                Some("https://app.example.com")
            ),
            Err(OriginCheckError::UntrustedHost)
        );
    }
}
