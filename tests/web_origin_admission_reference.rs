//! Contract parity against the audited Origin/Host algorithm in Web PR #2185.
//! No public HTTP endpoint is enabled by these tests.
use nulang::web::origin_admission::{validate_mutation_origin, OriginCheckError};

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

#[test]
fn reject_cross_origin_and_cross_site_mutations() {
    let bound = Some("127.0.0.1:8443".parse().unwrap());
    let origin = headers(&[
        ("Host", "127.0.0.1:8443"),
        ("Origin", "http://evil.example"),
        ("Sec-Fetch-Site", "cross-site"),
    ]);
    assert_eq!(
        validate_mutation_origin("POST", &origin, bound, None),
        Err(OriginCheckError::CrossOriginFetch)
    );
    let forged = headers(&[("Host", "evil.example"), ("Origin", "http://evil.example")]);
    assert_eq!(
        validate_mutation_origin("POST", &forged, bound, None),
        Err(OriginCheckError::UntrustedHost)
    );
}

#[test]
fn require_single_trusted_host_provenance_and_unambiguous_headers() {
    let bound = Some("127.0.0.1:8443".parse().unwrap());
    let missing = headers(&[("Host", "127.0.0.1:8443")]);
    assert_eq!(
        validate_mutation_origin("POST", &missing, bound, None),
        Err(OriginCheckError::MissingProvenance)
    );
    let duplicated = headers(&[
        ("Host", "127.0.0.1:8443"),
        ("Host", "127.0.0.1:8443"),
        ("Origin", "http://127.0.0.1:8443"),
    ]);
    assert_eq!(
        validate_mutation_origin("POST", &duplicated, bound, None),
        Err(OriginCheckError::DuplicateHeader)
    );
    let same = headers(&[
        ("Host", "127.0.0.1:8443"),
        ("Origin", "http://127.0.0.1:8443"),
        ("Sec-Fetch-Site", "same-origin"),
    ]);
    assert_eq!(validate_mutation_origin("POST", &same, bound, None), Ok(()));
    assert_eq!(
        validate_mutation_origin("GET", &missing, bound, None),
        Ok(())
    );
}

#[test]
fn reject_host_rebinding_even_on_read_only_get() {
    let bound = Some("127.0.0.1:8443".parse().unwrap());
    let foreign = headers(&[("Host", "evil.example")]);
    assert_eq!(
        validate_mutation_origin("GET", &foreign, bound, None),
        Err(OriginCheckError::UntrustedHost)
    );
}

#[test]
fn audited_legacy_api_reuses_canonical_policy_without_forking_rules() {
    use nulang::web::csrf::validate_mutation_origin as audited_legacy_origin_check;
    let bound = Some("127.0.0.1:8443".parse().unwrap());
    for (method, request) in [
        ("GET", headers(&[("Host", "127.0.0.1:8443")])),
        (
            "POST",
            headers(&[
                ("Host", "127.0.0.1:8443"),
                ("Origin", "http://127.0.0.1:8443"),
                ("Sec-Fetch-Site", "same-origin"),
            ]),
        ),
        (
            "POST",
            headers(&[
                ("Host", "attacker.example"),
                ("X-Forwarded-Host", "127.0.0.1:8443"),
                ("Origin", "http://127.0.0.1:8443"),
            ]),
        ),
        (
            "POST",
            headers(&[
                ("Host", "127.0.0.1:8443"),
                ("Origin", "https://attacker.example"),
            ]),
        ),
    ] {
        let canonical = validate_mutation_origin(method, &request, bound, None);
        let compatibility = audited_legacy_origin_check(method, &request, bound, None);
        assert_eq!(canonical, compatibility);
    }
}
