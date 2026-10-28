//! Synchronizer-token primitive for authenticated browser sessions.
//!
//! This module does **not** expose an HTTP endpoint, authenticate a session,
//! authorize a tenant, or install CSRF middleware. A host must retrieve the
//! expected token from *server-side authenticated session storage*, validate
//! Host/Origin and the caller's authority, and then verify the submitted token
//! before decoding any form fields. Never accept an expected token from a
//! header, query string, form body, or untrusted browser cookie.

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

    /// Restore a synchronizer token from an authenticated, server-owned
    /// session store. Never call this with an untrusted browser-supplied value.
    /// Malformed persistence data must fail closed; no random replacement.
    pub fn from_server_storage_hex(encoded: &str) -> Result<Self, CsrfTokenError> {
        if encoded.len() != ENCODED_BYTES
            || !encoded
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CsrfTokenError::Malformed);
        }
        let mut bytes = [0u8; TOKEN_BYTES];
        hex::decode_to_slice(encoded, &mut bytes).map_err(|_| CsrfTokenError::Malformed)?;
        Ok(Self(bytes))
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
