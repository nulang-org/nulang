//! Prevent a default HTTP agent from transmitting Nulang Cloud deployment credentials.
//! Registry URL policy tests exercise the actual native-TLS agent independently.

#[cfg(feature = "ureq")]
#[test]
fn deploy_upload_uses_verified_credential_transport() {
    let source = include_str!("../src/package/commands.rs");
    let deploy = source
        .split_once("fn cmd_deploy(\n")
        .expect("deployment function")
        .1
        .split_once("/// `nula deploy` — disabled without the `ureq` feature.")
        .expect("end of deployment function")
        .0;

    assert!(
        deploy.contains("crate::registry::client::verified_https_agent(&url)"),
        "nula deploy must apply credential-bearing URL and TLS policy",
    );
    assert!(
        !deploy.contains("ureq::post(&url)"),
        "nula deploy must not use the implicit default HTTP agent",
    );
}
