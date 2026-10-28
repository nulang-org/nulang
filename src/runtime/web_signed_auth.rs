//! Experimental Ed25519-signed, short-lived, tenant-bound bearer credentials.
//!
//! This is a deliberately small, versioned token format, **not a JWT** or
//! OAuth/OIDC implementation. Host applications must provision trusted public
//! keys and resolve the expected tenant from a trustworthy route context.

use super::http_server::{HttpRequest, WebCapabilityAuthorizer};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const SIGNATURE_CONTEXT: &[u8] = b"nulang-web-capability-v1\0";
const MAX_TOKEN_LEN: usize = 4096;
const MAX_PAYLOAD_BYTES: usize = 2048;
const MAX_TOKEN_LIFETIME_SECONDS: u64 = 3600;

/// Host-defined, trusted tenant lookup. Do not derive tenant authority from an
/// unverified `X-Tenant` header. A path-derived tenant is only a requested
/// resource scope: it must match the tenant in a verified signed credential.
pub type WebTenantResolver = dyn Fn(&HttpRequest) -> Option<String> + Send + Sync + 'static;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantClaims {
    sub: String,
    tenant: String,
    aud: String,
    iat: u64,
    exp: u64,
    permissions: Vec<String>,
}

/// Verifies an Ed25519 signature and binds its claimed tenant and permissions
/// to the host's expected audience and resolved tenant resource scope.
///
/// Token wire format: `Authorization: Bearer nula1.<hex UTF-8 JSON>.<hex Ed25519 signature>`.
/// The signed message is the ASCII context `nulang-web-capability-v1\\0`
/// followed by the **exact** UTF-8 JSON payload bytes. No token-encoded
/// algorithm or key selector is trusted. Validity <= 1 hour, no clock leeway.
/// Tokens are bearer credentials; revocation/replay mitigation is host-owned.
pub struct SignedTenantAuthorizer {
    verifying_key: VerifyingKey,
    audience: String,
    tenant_resolver: Arc<WebTenantResolver>,
}

impl SignedTenantAuthorizer {
    pub fn new(
        verifying_key: VerifyingKey,
        audience: impl Into<String>,
        tenant_resolver: Arc<WebTenantResolver>,
    ) -> Result<Self, &'static str> {
        let audience = audience.into();
        if audience.is_empty() || audience.len() > 128 || !audience.is_ascii() {
            return Err("invalid expected token audience");
        }
        Ok(Self {
            verifying_key,
            audience,
            tenant_resolver,
        })
    }

    /// Turn the trusted credential verifier into the HTTP server's existing
    /// callback. The server enforces AND across all declared capabilities.
    pub fn into_authorizer(self) -> Arc<WebCapabilityAuthorizer> {
        Arc::new(move |request, permission| self.verify(request, permission))
    }

    pub fn verify(&self, request: &HttpRequest, permission: &str) -> bool {
        let Some(now) = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
        else {
            return false;
        };
        self.verify_at(request, permission, now)
    }

    /// Exposed to support deterministic expiry and tamper tests. Production
    /// callers must use `verify` / `into_authorizer` with the system clock.
    pub fn verify_at(&self, request: &HttpRequest, permission: &str, now: u64) -> bool {
        let mut auth_headers = request
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str());
        let Some(auth) = auth_headers.next() else {
            return false;
        };
        if auth_headers.next().is_some() || auth.len() > MAX_TOKEN_LEN + 7 {
            return false;
        }
        let Some(token) = auth.strip_prefix("Bearer ") else {
            return false;
        };
        if token.len() > MAX_TOKEN_LEN || !token.is_ascii() {
            return false;
        }
        let mut parts = token.split('.');
        let (Some("nula1"), Some(payload_hex), Some(signature_hex), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        if payload_hex.len() > MAX_PAYLOAD_BYTES * 2 || signature_hex.len() != 128 {
            return false;
        }
        let (Ok(payload), Ok(signature)) = (hex::decode(payload_hex), hex::decode(signature_hex))
        else {
            return false;
        };
        if payload.len() > MAX_PAYLOAD_BYTES {
            return false;
        }
        let Ok(signature) = Signature::from_slice(&signature) else {
            return false;
        };
        let mut message = Vec::with_capacity(SIGNATURE_CONTEXT.len() + payload.len());
        message.extend_from_slice(SIGNATURE_CONTEXT);
        message.extend_from_slice(&payload);
        if self
            .verifying_key
            .verify_strict(&message, &signature)
            .is_err()
        {
            return false;
        }
        let Ok(claims) = serde_json::from_slice::<TenantClaims>(&payload) else {
            return false;
        };
        if claims.aud != self.audience
            || claims.sub.is_empty()
            || claims.sub.len() > 128
            || claims.tenant.is_empty()
            || claims.tenant.len() > 128
            || claims.permissions.len() > 64
            || claims.iat > now
            || claims.exp <= now
            || claims.exp <= claims.iat
            || claims.exp - claims.iat > MAX_TOKEN_LIFETIME_SECONDS
        {
            return false;
        }
        if !claims.permissions.iter().any(|item| item == permission) {
            return false;
        }
        let Some(expected_tenant) = (self.tenant_resolver)(request) else {
            return false;
        };
        expected_tenant == claims.tenant
    }
}
