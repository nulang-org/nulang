//! libSQL remote connector using patched rustls 0.23 on Hyper 0.14.
//! The upstream libSQL TLS feature still pins rustls-webpki 0.102.x.

use hyper::client::connect::{Connected, Connection, HttpConnector};
use hyper::Uri;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tower_service::Service;

/// A TCP or certificate-verified TLS stream suitable for Hyper 0.14.
pub struct ModernStream(Stream);

enum Stream {
    Http(TcpStream),
    Https(TlsStream<TcpStream>),
}

impl Connection for ModernStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl AsyncRead for ModernStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.0 {
            Stream::Http(io) => Pin::new(io).poll_read(cx, buf),
            Stream::Https(io) => Pin::new(io).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ModernStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.0 {
            Stream::Http(io) => Pin::new(io).poll_write(cx, data),
            Stream::Https(io) => Pin::new(io).poll_write(cx, data),
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.0 {
            Stream::Http(io) => Pin::new(io).poll_flush(cx),
            Stream::Https(io) => Pin::new(io).poll_flush(cx),
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.0 {
            Stream::Http(io) => Pin::new(io).poll_shutdown(cx),
            Stream::Https(io) => Pin::new(io).poll_shutdown(cx),
        }
    }
}

#[derive(Clone)]
pub struct ModernConnector {
    tcp: HttpConnector,
    tls: TlsConnector,
}

/// Reject plaintext transport when a bearer token would be transmitted.
/// An unauthenticated HTTP URL remains valid for local development, but
/// authenticated remote sessions require TLS and never accept URL userinfo.
pub(crate) fn validate_authenticated_remote_transport(url: &str, token: &str) -> io::Result<()> {
    let normalized = if let Some(tail) = url.strip_prefix("libsql://") {
        format!("https://{tail}")
    } else {
        url.to_string()
    };
    let uri: Uri = normalized
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid libSQL remote URL"))?;
    if uri.host().is_none()
        || uri.authority().is_some_and(|a| a.as_str().contains('@'))
        || !matches!(uri.scheme_str(), Some("https" | "http"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "libSQL remote URL requires an HTTP(S) host without inline credentials",
        ));
    }
    if uri.scheme_str() == Some("http") && !token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "libSQL authentication token requires encrypted HTTPS transport",
        ));
    }
    Ok(())
}

/// Requires usable platform roots; no insecure trust-all fallback.
pub fn connector() -> io::Result<ModernConnector> {
    let certs = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(certs.certs);
    if added == 0 {
        return Err(io::Error::other(
            "no usable native TLS roots for libSQL remote connection",
        ));
    }
    Ok(connector_with_roots(roots))
}

/// Use the same rustls certificate verifier for both system and test roots.
fn connector_with_roots(roots: RootCertStore) -> ModernConnector {
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut tcp = HttpConnector::new();
    tcp.enforce_http(false);
    tcp.set_nodelay(true);
    ModernConnector {
        tcp,
        tls: TlsConnector::from(std::sync::Arc::new(config)),
    }
}

impl Service<Uri> for ModernConnector {
    type Response = ModernStream;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<ModernStream, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.tcp.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let is_https = match dst.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => {
                return Box::pin(async {
                    Err(io::Error::other("invalid libSQL URL scheme").into())
                })
            }
        };
        let name = if is_https {
            match dst
                .host()
                .and_then(|host| ServerName::try_from(host.to_string()).ok())
            {
                Some(name) => Some(name),
                None => {
                    return Box::pin(async {
                        Err(io::Error::other("invalid TLS server name").into())
                    })
                }
            }
        } else {
            None
        };
        let connect = self.tcp.call(dst);
        let tls = self.tls.clone();
        Box::pin(async move {
            let socket = connect.await?;
            match name {
                Some(name) => Ok(ModernStream(Stream::Https(
                    tls.connect(name, socket).await?,
                ))),
                None => Ok(ModernStream(Stream::Http(socket))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::ServerConfig;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::time::{timeout, Duration};
    use tokio_rustls::TlsAcceptor;

    #[test]
    fn remote_bearer_token_requires_verified_tls() {
        assert!(super::validate_authenticated_remote_transport(
            "http://127.0.0.1:8080",
            "fake-token"
        )
        .is_err());
        assert!(super::validate_authenticated_remote_transport(
            "http://example.invalid",
            "fake-token"
        )
        .is_err());
        assert!(super::validate_authenticated_remote_transport(
            "https://db.turso.io",
            "fake-token"
        )
        .is_ok());
        assert!(super::validate_authenticated_remote_transport(
            "libsql://db.turso.io",
            "fake-token"
        )
        .is_ok());
        // Preserve unauthenticated HTTP development connections.
        assert!(
            super::validate_authenticated_remote_transport("http://127.0.0.1:8080", "").is_ok()
        );
        assert!(super::validate_authenticated_remote_transport(
            "https://user:token@db.turso.io",
            "fake-token"
        )
        .is_err());
        assert!(
            super::validate_authenticated_remote_transport("file:secret.db", "fake-token").is_err()
        );
        // Exercise the public store boundary, not only its helper, without
        // opening a network connection or exposing a real credential.
        let error = crate::runtime::persistence::LibsqlStore::new_remote(
            "http://127.0.0.1:1",
            "fake-token",
        )
        .err()
        .expect("plaintext bearer-token transport must fail before IO");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn remote_tls_enforces_server_certificate_chain_and_hostname() {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::from_params(&ca_params, &ca_key);

        let leaf_key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["localhost".to_string()])
            .unwrap()
            .signed_by(&leaf_key, &issuer)
            .unwrap();
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
            )
            .unwrap();
        let server = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for _ in 0..3 {
                let (socket, _) = listener.accept().await.unwrap();
                let _ = server.accept(socket).await;
            }
        });

        // Untrusted server must fail verification, not silently fall back.
        let uri: Uri = format!("https://localhost:{}", addr.port())
            .parse()
            .unwrap();
        let mut untrusted = connector_with_roots(RootCertStore::empty());
        let error = match timeout(Duration::from_secs(5), untrusted.call(uri.clone()))
            .await
            .unwrap()
        {
            Ok(_) => panic!("untrusted TLS certificate must not authenticate"),
            Err(error) => error,
        };
        assert!(
            error.to_string().to_lowercase().contains("certificate"),
            "{error}"
        );

        // A real configured trust anchor and matching SNI must work.
        let mut roots = RootCertStore::empty();
        roots.add(ca_cert.der().clone()).unwrap();
        let mut trusted = connector_with_roots(roots);
        timeout(Duration::from_secs(5), trusted.call(uri))
            .await
            .unwrap()
            .expect("trusted certificate must be accepted");

        // The same valid certificate must not authenticate a different hostname.
        let bad_uri: Uri = format!("https://127.0.0.1:{}", addr.port())
            .parse()
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(ca_cert.der().clone()).unwrap();
        let mut wrong_name = connector_with_roots(roots);
        assert!(timeout(Duration::from_secs(5), wrong_name.call(bad_uri))
            .await
            .unwrap()
            .is_err());
        task.abort();
    }
}
