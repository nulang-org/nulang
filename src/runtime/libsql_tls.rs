//! A libsql Hyper 0.14 connector backed by current Rustls 0.23.
//!
//! libsql 0.10's built-in TLS feature depends on unpatched rustls 0.22.
//! This explicitly injected HTTPS-only connector uses OS trust anchors and
//! normal hostname/certificate verification, with no insecure fallback.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper::client::connect::{Connected, Connection, HttpConnector};
use hyper::Uri;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
use tokio_rustls::{client::TlsStream, TlsConnector};
use tower_service::Service;

pub(super) fn validate_remote_url(url: &str) -> io::Result<()> {
    if !(url.starts_with("https://") || url.starts_with("libsql://")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote libsql requires an HTTPS or libsql URL; plaintext HTTP is forbidden",
        ));
    }
    let host = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    if host.is_empty() || host.starts_with('/') || host.contains('@') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote libsql URL must specify a host without userinfo",
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct VerifiedLibsqlConnector {
    tcp: HttpConnector,
    tls: TlsConnector,
}

pub(super) fn verified_connector() -> io::Result<VerifiedLibsqlConnector> {
    let certificates = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(certificates.certs);
    if roots.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no system TLS certificate authorities available for remote libsql",
        ));
    }
    // RootCertStore + the default WebPki server verifier enforces server
    // certificate chains, validity, and matching DNS names. Never disable it.
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut tcp = HttpConnector::new();
    tcp.enforce_http(false); // HTTPS is enforced *before* opening a socket.
    tcp.set_nodelay(true);
    Ok(VerifiedLibsqlConnector {
        tcp,
        tls: TlsConnector::from(Arc::new(config)),
    })
}

pub(super) struct VerifiedTlsStream(TlsStream<TcpStream>);

impl Connection for VerifiedTlsStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}
impl AsyncRead for VerifiedTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsyncWrite for VerifiedTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl Service<Uri> for VerifiedLibsqlConnector {
    type Response = VerifiedTlsStream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<VerifiedTlsStream, io::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        // Reject plaintext before DNS resolution, TCP connect or credentials.
        if uri.scheme_str() != Some("https") {
            return Box::pin(async {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "remote libsql connector requires HTTPS",
                ))
            });
        }
        let name = uri
            .host()
            .and_then(|host| ServerName::try_from(host.to_owned()).ok());
        let Some(server_name) = name else {
            return Box::pin(async {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "remote libsql HTTPS URI requires a valid TLS hostname",
                ))
            });
        };
        let mut tcp = self.tcp.clone();
        let tls = self.tls.clone();
        Box::pin(async move {
            let socket = tcp.call(uri).await.map_err(io::Error::other)?;
            let stream = tls
                .connect(server_name, socket)
                .await
                .map_err(io::Error::other)?;
            Ok(VerifiedTlsStream(stream))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_forbid_plaintext_and_credentials() {
        for invalid in [
            "",
            "http://example.com",
            "ftp://example.com",
            "libsql://",
            "https:///foo",
            "https://user:pass@example.com",
        ] {
            assert!(validate_remote_url(invalid).is_err(), "{invalid}");
        }
        assert!(validate_remote_url("libsql://example.com").is_ok());
        assert!(validate_remote_url("https://example.com").is_ok());
    }

    #[tokio::test]
    async fn rejects_self_signed_remote_tls_certificate_on_loopback() {
        use tokio::net::TcpListener;
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

        // Deliberately untrusted test-only certificate with SAN 127.0.0.1.
        // If the connector skips verification, the client handshake succeeds
        // and this regression fails.
        let fixture = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
            .expect("generate ephemeral untrusted certificate");
        let cert = CertificateDer::from(fixture.cert.der().as_ref().to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            fixture.signing_key.serialize_der(),
        ));
        let server_config = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let server_task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = server.accept(socket).await;
        });
        let mut client = verified_connector().unwrap();
        let uri: Uri = format!("https://127.0.0.1:{}/", address.port())
            .parse()
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), client.call(uri))
            .await
            .expect("TLS handshake should not hang");
        assert!(result.is_err(), "untrusted certificate must be rejected");
        server_task.await.unwrap();
    }

    #[test]
    fn libsql_store_remote_entry_point_rejects_plaintext_url() {
        let failure = crate::runtime::persistence::LibsqlStore::new_remote(
            "http://127.0.0.1:1",
            "test-token",
        )
        .err()
        .expect("plaintext URL must fail before network access");
        assert_eq!(failure.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn connector_rejects_plaintext_without_opening_sockets() {
        let mut connector = verified_connector().unwrap();
        let uri: Uri = "http://127.0.0.1:1/".parse().unwrap();
        let failure = connector
            .call(uri)
            .await
            .err()
            .expect("plaintext must fail");
        assert_eq!(failure.kind(), io::ErrorKind::PermissionDenied);
    }

    fn trusted_loopback_certificate(
        server_san: &str,
    ) -> (
        RootCertStore,
        Arc<tokio_rustls::rustls::ServerConfig>,
    ) {
        use rcgen::{
            BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
            KeyUsagePurpose,
        };
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_key = KeyPair::generate().unwrap();
        let ca_certificate = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);

        let mut leaf_params = CertificateParams::new(vec![server_san.to_owned()]).unwrap();
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_key = KeyPair::generate().unwrap();
        let leaf_certificate = leaf_params.signed_by(&leaf_key, &issuer).unwrap();
        let server = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(leaf_certificate.der().as_ref().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
            )
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(ca_certificate.der().as_ref().to_vec()))
            .unwrap();
        (roots, Arc::new(server))
    }

    async fn probe_trusted_loopback_tls(server_san: &str) -> io::Result<()> {
        use tokio::net::TcpListener;

        let (roots, server_config) = trusted_loopback_certificate(server_san);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
        let server_task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = acceptor.accept(socket).await;
        });

        // The client connects to the numeric loopback IP so the test never
        // depends on external DNS. Only the ephemeral test CA is trusted.
        let mut connector = verified_connector_with_roots(roots)?;
        let uri: Uri = format!("https://127.0.0.1:{port}/").parse().unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), connector.call(uri))
            .await
            .expect("loopback TLS handshake timed out");
        server_task.await.unwrap();
        outcome.map(|_| ())
    }

    #[tokio::test]
    async fn trusted_ca_with_matching_ip_san_succeeds_on_loopback() {
        probe_trusted_loopback_tls("127.0.0.1")
            .await
            .expect("a valid trusted CA chain and matching IP SAN must connect");
    }

    #[tokio::test]
    async fn trusted_ca_with_wrong_ip_san_is_rejected_on_loopback() {
        assert!(
            probe_trusted_loopback_tls("127.0.0.2").await.is_err(),
            "a trusted CA must not bypass hostname/IP SAN verification"
        );
    }

}