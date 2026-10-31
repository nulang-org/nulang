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
    // Parse the authority, not the entire tail after `://`: the latter
    // accepts hostless URLs and mistakes `@` in paths/queries for userinfo.
    let uri: Uri = url
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid remote libsql URI"))?;
    let authority = uri.authority().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote libsql URI requires a host",
        )
    })?;
    let host = authority.host();
    if host.is_empty() || authority.as_str().contains('@') || url.contains('#') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote libsql URI requires a host without userinfo or fragments",
        ));
    }
    // Authority's host includes IPv6 brackets but excludes an optional port.
    // A nonempty suffix therefore must be a valid numeric port, not `:abc`.
    let suffix = authority.as_str().strip_prefix(host).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid remote libsql authority",
        )
    })?;
    if !suffix.is_empty() && (!suffix.starts_with(':') || authority.port_u16().is_none()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote libsql URI has an invalid port",
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
            "https://?token=abc",
            "https://#fragment",
            "https://:443/path",
            "https://example.com/#fragment",
            "https://example.com:badport/path",
        ] {
            assert!(validate_remote_url(invalid).is_err(), "{invalid}");
        }
        assert!(validate_remote_url("libsql://example.com").is_ok());
        assert!(validate_remote_url("https://example.com").is_ok());
        assert!(validate_remote_url("https://example.com/path?email=a@b.test").is_ok());
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
}
