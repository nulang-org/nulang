//! Audited browser request Host/Origin/Fetch Metadata contract.
//! Compatibility copy from draft review Web security PR #2185.
//! Do NOT merge both copies without reconciling into one canonical provenance module.
//! Separate from session synchronizer `web::csrf`; this is not authentication.

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
