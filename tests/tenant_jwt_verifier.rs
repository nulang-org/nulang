//! Signed access-token contract tests. Uses a deterministic TEST signing key.
//! No production private key, identity provider or live database is contacted.
#![cfg(feature = "postgres")]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use nulang::web::tenant_jwt_reference::{PinnedEd25519Verifier, RevocationPolicy};
use nulang::web::tenant_pg_reference::TrustedTokenVerifier;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

struct Revoked {
    revoked: HashSet<String>,
}
impl RevocationPolicy for Revoked {
    fn is_revoked(&self, _issuer: &str, _subject: &str, jti: &str) -> Option<bool> {
        Some(self.revoked.contains(jti))
    }
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn signer() -> SigningKey {
    SigningKey::from_bytes(&[17_u8; 32]) // fixture only, never a deployment secret
}

fn verifier(revoked: &[&str]) -> PinnedEd25519Verifier<Revoked> {
    PinnedEd25519Verifier::new(
        "issuer-a",
        "nulang-tenant-api",
        "test-key-2026",
        signer().verifying_key(),
        Revoked {
            revoked: revoked.iter().map(|x| (*x).to_string()).collect(),
        },
    )
    .unwrap()
}

fn claims() -> Value {
    let t = now();
    json!({
        "iss": "issuer-a", "sub": "alice", "aud": "nulang-tenant-api",
        "iat": t-2, "nbf": t-2, "exp": t+120, "jti": "session-1"
    })
}

fn sign(header: Value, claims: Value) -> String {
    let head = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let payload = format!("{head}.{body}");
    let signature = signer().sign(payload.as_bytes());
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

fn valid_header() -> Value {
    json!({"alg":"EdDSA","kid":"test-key-2026","typ":"at+jwt"})
}
fn token() -> String {
    sign(valid_header(), claims())
}

#[test]
fn signed_access_token_binds_exact_issuer_audience_and_subject() {
    let t = token();
    assert_eq!(verifier(&[]).verify(&t), Some("issuer-a|alice".into()));
}

#[test]
fn reject_unsigned_wrong_algorithm_typ_kid_and_tampered_signature() {
    let good = token();
    let v = verifier(&[]);
    for head in [
        json!({"alg":"none","kid":"test-key-2026","typ":"at+jwt"}),
        json!({"alg":"HS256","kid":"test-key-2026","typ":"at+jwt"}),
        json!({"alg":"EdDSA","kid":"wrong-key","typ":"at+jwt"}),
        json!({"alg":"EdDSA","kid":"test-key-2026","typ":"JWT"}),
        json!({"alg":"EdDSA","kid":"test-key-2026","typ":"at+jwt","jku":"https://attacker.test/jwks"}),
    ] {
        assert_eq!(v.verify(&sign(head, claims())), None);
    }
    // A token with the right header but signed by another Ed25519 key is
    // not authenticated by the pinned verifier.
    let signing_input = good.rsplit_once('.').unwrap().0;
    let wrong_signer = SigningKey::from_bytes(&[19_u8; 32]);
    let wrong_signature = wrong_signer.sign(signing_input.as_bytes());
    assert_eq!(
        v.verify(&format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(wrong_signature.to_bytes())
        )),
        None
    );

    let mut parts: Vec<String> = good.split('.').map(ToString::to_string).collect();
    parts[1] = URL_SAFE_NO_PAD.encode(br#"{"iss":"issuer-a","sub":"mallory"}"#);
    assert_eq!(v.verify(&parts.join(".")), None);
    assert_eq!(v.verify("e30.e30."), None);
    assert_eq!(v.verify("abc"), None);
    assert_eq!(v.verify(&format!("{good}.extra")), None);
}

#[test]
fn reject_expired_future_missing_and_wrong_claims() {
    let v = verifier(&[]);
    let base = claims();
    for (key, value) in [
        ("iss", json!("issuer-b")),
        ("aud", json!("other-api")),
        ("aud", json!([])),
        ("sub", json!("")),
        ("sub", json!(12)),
        ("jti", json!("")),
        ("exp", json!(now() - 1)),
        ("exp", json!(now() + 7200)),
        ("iat", json!(now() + 3600)),
        ("iat", json!(now() - 7200)),
        ("nbf", json!(now() + 3600)),
    ] {
        let mut changed = base.clone();
        changed[key] = value;
        assert_eq!(
            v.verify(&sign(valid_header(), changed)),
            None,
            "invalid {key} accepted"
        );
    }
    let mut missing = base;
    missing.as_object_mut().unwrap().remove("iat");
    assert_eq!(v.verify(&sign(valid_header(), missing)), None);
}

#[test]
fn accepts_audience_array_and_rejects_revocation() {
    let mut data = claims();
    data["aud"] = json!(["other-api", "nulang-tenant-api"]);
    assert_eq!(
        verifier(&[]).verify(&sign(valid_header(), data)),
        Some("issuer-a|alice".into())
    );
    assert_eq!(verifier(&["session-1"]).verify(&token()), None);
}

#[test]
fn malformed_json_and_oversized_tokens_are_rejected() {
    let v = verifier(&[]);
    assert_eq!(v.verify(&"x".repeat(9000)), None);
    // Duplicate protected claim keys MUST NOT be accepted with last-wins semantics.
    let now = now();
    let raw = format!("{{\"iss\":\"issuer-a\",\"sub\":\"alice\",\"aud\":\"nulang-tenant-api\",\"exp\":{},\"iat\":{},\"jti\":\"x\",\"sub\":\"mallory\"}}", now+100, now);
    let h = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&valid_header()).unwrap());
    let b = URL_SAFE_NO_PAD.encode(raw.as_bytes());
    let input = format!("{h}.{b}");
    let sig = signer().sign(input.as_bytes());
    assert_eq!(
        v.verify(&format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(sig.to_bytes())
        )),
        None
    );
}

#[test]
fn unavailable_revocation_policy_must_deny_signed_access_token() {
    struct UnavailableRevocation;
    impl RevocationPolicy for UnavailableRevocation {
        // A backend outage or unknown revocation state is not "not revoked".
        fn is_revoked(&self, _issuer: &str, _subject: &str, _jti: &str) -> Option<bool> {
            None
        }
    }
    let verifier = PinnedEd25519Verifier::new(
        "issuer-a",
        "nulang-tenant-api",
        "test-key-2026",
        signer().verifying_key(),
        UnavailableRevocation,
    )
    .unwrap();
    assert_eq!(verifier.verify(&token()), None);
}
