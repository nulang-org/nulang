//! Pinned, server-controlled JWT verification-key rotation reference.
//! Tests use deterministic signing keys; do NOT deploy private fixture keys.
#![cfg(feature = "postgres")]
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use nulang::web::tenant_jwt_reference::{PinnedEd25519KeyRing, RevocationPolicy};
use nulang::web::tenant_pg_reference::TrustedTokenVerifier;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Default)]
struct SharedRevocation(Arc<Mutex<HashSet<String>>>);
impl RevocationPolicy for SharedRevocation {
    fn is_revoked(&self, _issuer: &str, _subject: &str, jti: &str) -> Option<bool> {
        Some(self.0.lock().ok()?.contains(jti))
    }
}
fn signer(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}
fn ring(
    kids: &[(&str, u8)],
    revoked: SharedRevocation,
) -> Option<PinnedEd25519KeyRing<SharedRevocation>> {
    PinnedEd25519KeyRing::new(
        "issuer-a",
        "nulang-tenant-api",
        kids.iter()
            .map(|(kid, seed)| ((*kid).into(), signer(*seed).verifying_key()))
            .collect(),
        revoked,
    )
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
fn sign(seed: u8, header: Value, claims: Value) -> String {
    let msg = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap()),
    );
    format!(
        "{msg}.{}",
        URL_SAFE_NO_PAD.encode(signer(seed).sign(msg.as_bytes()).to_bytes())
    )
}
fn valid_header(kid: &str) -> Value {
    json!({"alg":"EdDSA","kid":kid,"typ":"at+jwt"})
}
fn claims(jti: &str) -> Value {
    let ts = now();
    json!({
        "iss":"issuer-a","sub":"alice","aud":"nulang-tenant-api",
        "exp":ts+120,"iat":ts-2,"nbf":ts-2,"jti":jti
    })
}

#[test]
fn staged_rotation_accepts_old_and_new_then_retirement_denies_old() {
    let state = SharedRevocation::default();
    let old = sign(51, valid_header("old"), claims("jti-old"));
    let new = sign(52, valid_header("new"), claims("jti-new"));
    let staged = ring(&[("old", 51), ("new", 52)], state.clone()).unwrap();
    assert_eq!(staged.verify(&old), Some("issuer-a|alice".into()));
    assert_eq!(staged.verify(&new), Some("issuer-a|alice".into()));
    // Retirement is an explicit new server-controlled trust configuration,
    // not a JWT-supplied instruction or cache expiration.
    let retired = ring(&[("new", 52)], state).unwrap();
    assert_eq!(retired.verify(&old), None);
    assert_eq!(retired.verify(&new), Some("issuer-a|alice".into()));
}

#[test]
fn unknown_kid_duplicate_kid_and_malformed_trust_configs_fail_closed() {
    let state = SharedRevocation::default();
    assert!(ring(&[], state.clone()).is_none());
    assert!(ring(&[("dup", 51), ("dup", 52)], state.clone()).is_none());
    assert!(ring(&[("", 51)], state.clone()).is_none());
    assert!(ring(&[("old\nkey", 51)], state.clone()).is_none());
    assert!(ring(&[("a", 1); 17], state.clone()).is_none());
    let v = ring(&[("trusted", 51)], state).unwrap();
    assert_eq!(
        v.verify(&sign(51, valid_header("unknown"), claims("jti-x"))),
        None
    );
    assert_eq!(v.verify("not.jwt"), None);
    assert_eq!(v.verify(&"x".repeat(9000)), None);
}

#[test]
fn algorithm_confusion_and_wrong_key_with_correct_kid_fail() {
    let v = ring(&[("kid-a", 51), ("kid-b", 52)], SharedRevocation::default()).unwrap();
    let data = claims("jti-3");
    for header in [
        json!({"alg":"none","kid":"kid-a","typ":"at+jwt"}),
        json!({"alg":"HS256","kid":"kid-a","typ":"at+jwt"}),
        json!({"alg":"EdDSA","kid":"kid-a","typ":"JWT"}),
        json!({"alg":"EdDSA","kid":"kid-a","typ":"at+jwt","jku":"https://attacker.invalid/key"}),
    ] {
        assert_eq!(v.verify(&sign(51, header, data.clone())), None);
    }
    assert_eq!(v.verify(&sign(52, valid_header("kid-a"), data)), None);
}

#[test]
fn shared_revocation_persists_across_rotation_and_denies_both_keys() {
    let state = SharedRevocation::default();
    let old = sign(51, valid_header("old"), claims("revoked-jti"));
    let new = sign(52, valid_header("new"), claims("revoked-jti"));
    let live = ring(&[("old", 51), ("new", 52)], state.clone()).unwrap();
    assert!(live.verify(&old).is_some());
    assert!(live.verify(&new).is_some());
    state.0.lock().unwrap().insert("revoked-jti".into());
    assert_eq!(live.verify(&old), None);
    assert_eq!(live.verify(&new), None);
    assert_eq!(ring(&[("new", 52)], state).unwrap().verify(&new), None);
}

#[test]
fn unsupported_or_duplicated_protected_headers_deny() {
    let v = ring(&[("kid-a", 51)], SharedRevocation::default()).unwrap();
    let claims = claims("jti-duplicate");
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    for raw_header in [
        r#"{"alg":"EdDSA","kid":"kid-a","kid":"unknown","typ":"at+jwt"}"#,
        r#"{"alg":"EdDSA","kid":"kid-a","typ":"at+jwt","crit":["exp"]}"#,
    ] {
        let msg = format!("{}.{}", URL_SAFE_NO_PAD.encode(raw_header), body);
        let token = format!(
            "{msg}.{}",
            URL_SAFE_NO_PAD.encode(signer(51).sign(msg.as_bytes()).to_bytes())
        );
        assert_eq!(v.verify(&token), None);
    }
}
