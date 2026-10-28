//! Strict, pinned Ed25519 JWT access-token verifier for the disposable
//! tenant PostgreSQL authorization contract.
//!
//! No remote JWKS or token-supplied key location is ever consulted. This is
//! reference verifier code, **not** a production identity provider: key
//! provisioning/rotation, issuer trust, durable revocation, OIDC metadata and
//! real HTTP admission require independent security review.
use super::tenant_pg_reference::TrustedTokenVerifier;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_COMPACT_TOKEN: usize = 8192;
const MAX_LIFETIME_SECONDS: i64 = 900;
const MAX_FUTURE_ISSUE_SKEW: i64 = 30;

/// Implementations must fail closed when the revocation source is unavailable.
/// A no-op revocation policy cannot provide production revocation guarantees.
pub trait RevocationPolicy: Send + Sync {
    /// Some(false) = known active; Some(true) = revoked; None = unavailable
    /// or indeterminate (deny). Never interpret an outage as a valid token.
    fn is_revoked(&self, issuer: &str, subject: &str, jti: &str) -> Option<bool>;
}

/// A single key pinned by *server configuration*; never fetched from the JWT
/// header. The verifier returns the existing tenant adapter's issuer|subject
/// identity format, so delimiters are disallowed in configured identifiers.
pub struct PinnedEd25519Verifier<R> {
    issuer: String,
    audience: String,
    kid: String,
    public_key: VerifyingKey,
    revocation: R,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    kid: String,
    typ: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(aud) => aud == expected,
            Self::Many(auds) => !auds.is_empty() && auds.iter().any(|aud| aud == expected),
        }
    }
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: i64,
    iat: i64,
    #[serde(default)]
    nbf: Option<i64>,
    jti: String,
}

impl<R: RevocationPolicy> PinnedEd25519Verifier<R> {
    /// The exact issuer, audience and key ID must originate from trusted
    /// out-of-band configuration. The input public key is already parsed.
    pub fn new(
        issuer: &str,
        audience: &str,
        kid: &str,
        public_key: VerifyingKey,
        revocation: R,
    ) -> Option<Self> {
        if issuer.is_empty()
            || issuer.len() > 1024
            || issuer.contains('|')
            || audience.is_empty()
            || audience.len() > 1024
            || kid.is_empty()
            || kid.len() > 256
            || [issuer, audience, kid]
                .iter()
                .any(|s| s.bytes().any(|byte| byte.is_ascii_control()))
        {
            return None;
        }
        Some(Self {
            issuer: issuer.to_string(),
            audience: audience.to_string(),
            kid: kid.to_string(),
            public_key,
            revocation,
        })
    }

    fn verify_at(&self, token: &str, now: i64) -> Option<String> {
        if token.is_empty() || token.len() > MAX_COMPACT_TOKEN || !token.is_ascii() {
            return None;
        }

        // Require precisely three compact segments with canonical base64url
        // without padding. Reject all URL/algorithm/key confusion.
        let mut parts = token.split('.');
        let header_b64 = parts.next()?;
        let claims_b64 = parts.next()?;
        let signature_b64 = parts.next()?;
        if parts.next().is_some()
            || header_b64.is_empty()
            || claims_b64.is_empty()
            || signature_b64.is_empty()
        {
            return None;
        }
        let header_data = URL_SAFE_NO_PAD.decode(header_b64).ok()?;
        let header: Header = serde_json::from_slice(&header_data).ok()?;
        if header.alg != "EdDSA" || header.typ != "at+jwt" || header.kid != self.kid {
            return None;
        }
        let signature_data = URL_SAFE_NO_PAD.decode(signature_b64).ok()?;
        let signature = Signature::from_slice(&signature_data).ok()?;
        // Verify over the exact original ASCII compact signing input.
        let signature_start = token.len().checked_sub(signature_b64.len() + 1)?;
        let signing_input = &token.as_bytes()[..signature_start];
        self.public_key
            .verify_strict(signing_input, &signature)
            .ok()?;

        let claims_data = URL_SAFE_NO_PAD.decode(claims_b64).ok()?;
        let claims: Claims = serde_json::from_slice(&claims_data).ok()?;
        let ttl = claims.exp.checked_sub(claims.iat)?;
        let max_future_issued = now.checked_add(MAX_FUTURE_ISSUE_SKEW)?;
        if claims.iss != self.issuer
            || !claims.aud.contains(&self.audience)
            || claims.sub.is_empty()
            || claims.sub.len() > 256
            || claims.sub.contains('|')
            || claims.sub.chars().any(char::is_control)
            || claims.jti.is_empty()
            || claims.jti.len() > 256
            || ttl <= 0
            || ttl > MAX_LIFETIME_SECONDS
            || claims.exp <= now
            || claims.iat > max_future_issued
            || claims.nbf.is_some_and(|nbf| nbf > max_future_issued)
            || self
                .revocation
                .is_revoked(&claims.iss, &claims.sub, &claims.jti)
                != Some(false)
        {
            return None;
        }
        Some(format!("{}|{}", self.issuer, claims.sub))
    }
}

impl<R: RevocationPolicy> TrustedTokenVerifier for PinnedEd25519Verifier<R> {
    fn verify(&self, bearer: &str) -> Option<String> {
        let now =
            i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
        self.verify_at(bearer, now)
    }
}

/*
 * Server-controlled Ed25519 verification key rollover. A key-ring update is
 * an explicit replacement of trusted server configuration; unverified JWT
 * headers are NEVER used as URLs or sources of new keys.
 */
impl<R: RevocationPolicy> RevocationPolicy for std::sync::Arc<R> {
    fn is_revoked(&self, issuer: &str, subject: &str, jti: &str) -> Option<bool> {
        (**self).is_revoked(issuer, subject, jti)
    }
}

/// Immutable pinned trust bundle for a staged key rollover. Do not generate
/// or load verification keys from untrusted incoming JWT metadata. No network
/// discovery or hot reload occurs; a caller must explicitly replace this ring.
pub struct PinnedEd25519KeyRing<R: RevocationPolicy> {
    keys: std::collections::HashMap<String, PinnedEd25519Verifier<std::sync::Arc<R>>>,
}

impl<R: RevocationPolicy> PinnedEd25519KeyRing<R> {
    pub fn new(
        issuer: &str,
        audience: &str,
        configured_keys: Vec<(String, VerifyingKey)>,
        revocation: R,
    ) -> Option<Self> {
        // Bound the memory/selection surface; don't silently overwrite
        // duplicate key identifiers during server configuration loading.
        if configured_keys.is_empty() || configured_keys.len() > 16 {
            return None;
        }
        let revocation = std::sync::Arc::new(revocation);
        let mut keys = std::collections::HashMap::with_capacity(configured_keys.len());
        for (kid, public_key) in configured_keys {
            if keys.contains_key(&kid) {
                return None;
            }
            let verifier = PinnedEd25519Verifier::new(
                issuer,
                audience,
                &kid,
                public_key,
                std::sync::Arc::clone(&revocation),
            )?;
            keys.insert(kid, verifier);
        }
        Some(Self { keys })
    }
}

impl<R: RevocationPolicy> TrustedTokenVerifier for PinnedEd25519KeyRing<R> {
    fn verify(&self, bearer: &str) -> Option<String> {
        // Inspect ONLY a bounded, structurally strict header to select an
        // already trusted key. The selected verifier independently rechecks
        // the entire header, the exact signing bytes, claim semantics and
        // revocation before supplying a verified principal.
        if bearer.is_empty() || bearer.len() > MAX_COMPACT_TOKEN || !bearer.is_ascii() {
            return None;
        }
        let mut segments = bearer.split('.');
        let header = segments.next()?;
        let payload = segments.next()?;
        let signature = segments.next()?;
        if segments.next().is_some()
            || header.is_empty()
            || payload.is_empty()
            || signature.is_empty()
            || header.len() > 2048
        {
            return None;
        }
        let decoded = URL_SAFE_NO_PAD.decode(header).ok()?;
        let protected: Header = serde_json::from_slice(&decoded).ok()?;
        if protected.alg != "EdDSA" || protected.typ != "at+jwt" {
            return None;
        }
        self.keys.get(&protected.kid)?.verify(bearer)
    }
}
