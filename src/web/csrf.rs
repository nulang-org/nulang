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

/// An explicitly configured reverse-proxy origin must use HTTPS and consist
/// of exactly scheme and authority. Plain HTTP can be used only for direct,
/// unconfigured localhost development. Userinfo, paths, query and fragments
/// are never valid trusted origins.
fn configured_authority(origin: &str) -> Result<&str, OriginCheckError> {
    let rest = origin
        .strip_prefix("https://")
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

/// Validate the Host for every Web request before dispatch, then require
/// same-origin provenance for state-changing requests.
///
/// This also protects read-only routes and SSE subscriptions from DNS
/// rebinding. Configure public_origin for a trusted reverse proxy. Without it,
/// only the local listening IP or localhost are accepted. The Origin/Referer
/// checks are required for unsafe methods only. This is not authentication.
pub fn validate_mutation_origin(
    method: &str,
    headers: &[(String, String)],
    local_addr: Option<SocketAddr>,
    public_origin: Option<&str>,
) -> Result<(), OriginCheckError> {
    let safe_method = matches!(method, "GET" | "HEAD" | "OPTIONS");
    let host = single_header(headers, "Host")?.ok_or(OriginCheckError::MissingHost)?;
    let expected = if let Some(origin) = public_origin {
        let trusted_authority = configured_authority(origin)?;
        if !host.eq_ignore_ascii_case(trusted_authority) {
            return Err(OriginCheckError::UntrustedHost);
        }
        origin.to_string()
    } else {
        let local = local_addr.ok_or(OriginCheckError::UntrustedHost)?;
        // Legacy read-only HTTP fixtures send Host: localhost without a port.
        // This name cannot be DNS-rebound by an attacker-controlled hostname.
        // Unsafe requests retain the strict authority-with-port policy.
        let legacy_loopback_read = safe_method
            && local.ip().is_loopback()
            && (host.eq_ignore_ascii_case("localhost")
                || host.eq_ignore_ascii_case(&local.ip().to_string()));
        if !direct_host_allowed(host, local) && !legacy_loopback_read {
            return Err(OriginCheckError::UntrustedHost);
        }
        format!("http://{host}")
    };

    // Safe reads need neither Origin nor Referer, but must have passed Host
    // admission above. Otherwise an attacker-controlled DNS name resolving to
    // localhost can read sensitive GET pages using its own browser origin.
    if safe_method {
        return Ok(());
    }

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
    fn safe_requests_require_a_trusted_host_but_no_origin() {
        for method in ["GET", "HEAD", "OPTIONS"] {
            let trusted = headers(&[("Host", "localhost:8080")]);
            assert_eq!(
                validate_mutation_origin(method, &trusted, Some(loopback()), None),
                Ok(())
            );
            // Existing Web HTTP smoke fixtures use this loopback-only form.
            let legacy = headers(&[("Host", "localhost")]);
            assert_eq!(
                validate_mutation_origin(method, &legacy, Some(loopback()), None),
                Ok(())
            );
            assert_eq!(
                validate_mutation_origin(method, &[], Some(loopback()), None),
                Err(OriginCheckError::MissingHost)
            );
            let rebound = headers(&[("Host", "attacker.example:8080")]);
            assert_eq!(
                validate_mutation_origin(method, &rebound, Some(loopback()), None),
                Err(OriginCheckError::UntrustedHost)
            );
            let duplicate = headers(&[
                ("Host", "localhost:8080"),
                ("Host", "attacker.example:8080"),
            ]);
            assert_eq!(
                validate_mutation_origin(method, &duplicate, Some(loopback()), None),
                Err(OriginCheckError::DuplicateHeader)
            );
        }
    }

    #[test]
    fn read_requests_through_proxy_require_configured_public_host() {
        let wrong_host = headers(&[("Host", "attacker.example")]);
        assert_eq!(
            validate_mutation_origin(
                "GET",
                &wrong_host,
                Some(loopback()),
                Some("https://app.example.com")
            ),
            Err(OriginCheckError::UntrustedHost)
        );
        let trusted = headers(&[("Host", "app.example.com")]);
        assert_eq!(
            validate_mutation_origin(
                "GET",
                &trusted,
                Some(loopback()),
                Some("https://app.example.com")
            ),
            Ok(())
        );
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
    fn configured_proxy_origin_requires_tls_and_ignores_forwarded_spoofing() {
        let insecure = headers(&[
            ("Host", "app.example.com"),
            ("Origin", "http://app.example.com"),
        ]);
        assert_eq!(
            validate_mutation_origin(
                "POST",
                &insecure,
                Some(loopback()),
                Some("http://app.example.com"),
            ),
            Err(OriginCheckError::InvalidConfiguration),
            "a configured reverse-proxy origin must not opt out of TLS"
        );

        let forwarded_host_spoof = headers(&[
            ("Host", "attacker.example"),
            ("Origin", "https://app.example.com"),
            ("X-Forwarded-Host", "app.example.com"),
            ("X-Forwarded-Proto", "https"),
        ]);
        assert_eq!(
            validate_mutation_origin(
                "POST",
                &forwarded_host_spoof,
                Some(loopback()),
                Some("https://app.example.com"),
            ),
            Err(OriginCheckError::UntrustedHost),
            "untrusted Forwarded headers cannot override the actual Host"
        );

        let trusted_referer = headers(&[
            ("Host", "app.example.com"),
            ("Referer", "https://app.example.com/account/update?x=1"),
            ("Sec-Fetch-Site", "same-origin"),
        ]);
        assert_eq!(
            validate_mutation_origin(
                "PATCH",
                &trusted_referer,
                Some(loopback()),
                Some("https://app.example.com"),
            ),
            Ok(()),
            "verified same-origin Referer fallback should work behind TLS termination"
        );

        let malicious_referer = headers(&[
            ("Host", "app.example.com"),
            ("Referer", "https://app.example.com.attacker.test/account"),
        ]);
        assert_eq!(
            validate_mutation_origin(
                "POST",
                &malicious_referer,
                Some(loopback()),
                Some("https://app.example.com"),
            ),
            Err(OriginCheckError::OriginMismatch)
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

// Session preflight synchronizer token primitives (merged from #2387).
// Synchronizer-token primitive for authenticated browser sessions.
//
// This module does **not** expose an HTTP endpoint, authenticate a session,
// authorize a tenant, or install CSRF middleware. A host must retrieve the
// expected token from *server-side authenticated session storage*, validate
// Host/Origin and the caller's authority, and then verify the submitted token
// before decoding any form fields. Never accept an expected token from a
// header, query string, form body, or untrusted browser cookie.

use rand_core::{OsRng, RngCore};
use std::fmt;
use subtle::ConstantTimeEq;

const TOKEN_BYTES: usize = 32;
const ENCODED_BYTES: usize = TOKEN_BYTES * 2;

/// Server-owned 256-bit synchronizer token, scoped to one authenticated session.
///
/// Keep this in session storage; emit its encoded value only into the
/// authenticated document or same-origin response used to submit forms.
/// Rotate on session creation, privilege changes and logout. Do not log it.
#[derive(Clone)]
pub struct SessionCsrfToken([u8; TOKEN_BYTES]);

impl fmt::Debug for SessionCsrfToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionCsrfToken(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsrfTokenError {
    Missing,
    Ambiguous,
    Malformed,
    Mismatch,
}

impl SessionCsrfToken {
    /// Generate a cryptographically unpredictable token. Propagate OS RNG
    /// failure instead of silently falling back to a predictable generator.
    pub fn generate() -> Result<Self, rand_core::Error> {
        let mut bytes = [0u8; TOKEN_BYTES];
        OsRng.try_fill_bytes(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Canonical lowercase hexadecimal token for an authenticated form.
    /// This is secret material; never put it in logs, URLs, or analytics.
    pub fn encode(&self) -> String {
        hex::encode(self.0)
    }

    /// Verify exactly one token from *all* submitted sources (header and form).
    /// Callers must preserve duplicates before parsing request fields into a
    /// map: otherwise two tokens can become one through parameter pollution.
    ///
    /// Use one generic 403 for all errors at the HTTP boundary; the error
    /// variants exist for tests and private structured diagnostics only.
    #[must_use]
    pub fn verify_presented(&self, submissions: &[&str]) -> Result<(), CsrfTokenError> {
        let presented = match submissions {
            [] => return Err(CsrfTokenError::Missing),
            [token] => *token,
            _ => return Err(CsrfTokenError::Ambiguous),
        };
        if presented.len() != ENCODED_BYTES
            || !presented
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CsrfTokenError::Malformed);
        }
        let mut decoded = [0u8; TOKEN_BYTES];
        hex::decode_to_slice(presented, &mut decoded).map_err(|_| CsrfTokenError::Malformed)?;
        if self.0.ct_eq(&decoded).unwrap_u8() == 1 {
            Ok(())
        } else {
            Err(CsrfTokenError::Mismatch)
        }
    }
}

/// Errors from the small CSRF-checked input-decoding composition. A host must
/// translate both variants into safe, generic HTTP failures; do not serialize
/// internal compiler diagnostics or distinguish CSRF failures to browsers.
#[derive(Debug)]
pub enum CsrfPreflightError {
    Csrf(CsrfTokenError),
    InvalidPlan(crate::web::request_bindings::RequestDecodeError),
}

/// Check the caller's **server-owned, authenticated-session** synchronizer
/// token before running the read-only scalar preflight. This deliberately does
/// not run the form handler or effects. It is only one part of admission:
/// production hosts must also authenticate and authorize the principal and
/// resource, validate Origin/Host, throttle requests, and perform any
/// transaction-level domain validation again on final submission.
///
/// There is no public HTTP endpoint for this helper.
pub fn preflight_after_session_csrf(
    expected: &SessionCsrfToken,
    submitted: &[&str],
    bindings: &[crate::web::bindings::RouteBindingContract],
    handler_param_count: usize,
    values: &crate::web::request_bindings::RequestBindingValues<'_>,
) -> Result<crate::web::request_bindings::RequestPreflight, CsrfPreflightError> {
    expected
        .verify_presented(submitted)
        .map_err(CsrfPreflightError::Csrf)?;
    crate::web::request_bindings::preflight_request_arguments(bindings, handler_param_count, values)
        .map_err(CsrfPreflightError::InvalidPlan)
}
