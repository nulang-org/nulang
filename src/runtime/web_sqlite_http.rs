//! Opt-in native HTTP dispatch for trusted SQLite-backed Web applications.
//!
//! Only requests under an explicit reserved prefix enter this mount. The
//! Authorization header is verified before dispatch; app handlers receive a
//! tenant-scoped capability, never the unrestricted SQLite connection.
//!
//! The default Nulang Web routes and ephemeral Web.kv_* are not modified.

use std::io;
use std::sync::Arc;

use super::{
    AuthorizedWebSqlite, HttpRequest, HttpResponse, WebSqliteAuthGate, WebSqliteStore,
    WebTenantTokenVerifier,
};

/// Reserved native-host prefix. Without an explicit mount the legacy router
/// remains unchanged. A similar-looking prefix (e.g. /sqliteevil) never matches.
pub const WEB_SQLITE_HTTP_PREFIX: &str = "/__nulang/sqlite/";

/// Trusted native handler invoked only after tenant membership verification.
///
/// No reference to the raw store is supplied. Nulang bytecode handlers do not
/// implement this trait and cannot gain a store capability via Web.kv_*.
pub trait WebSqliteNativeHandler: Send + Sync {
    fn handle(
        &self,
        request: &HttpRequest,
        authorized: &AuthorizedWebSqlite<'_>,
    ) -> io::Result<HttpResponse>;
}

/// One native storage mount, shared for the lifetime of the listener.
///
/// A host must supply a real verifier and its own trusted route implementation.
/// This adapter deliberately does not register a generic public CRUD service.
pub struct WebSqliteHttpMount {
    store: Arc<WebSqliteStore>,
    verifier: Arc<dyn WebTenantTokenVerifier>,
    handler: Arc<dyn WebSqliteNativeHandler>,
}

impl WebSqliteHttpMount {
    pub fn new(
        store: Arc<WebSqliteStore>,
        verifier: Arc<dyn WebTenantTokenVerifier>,
        handler: Arc<dyn WebSqliteNativeHandler>,
    ) -> Self {
        Self {
            store,
            verifier,
            handler,
        }
    }

    pub(crate) fn handles(&self, target: &str) -> bool {
        // HTTP request-target includes the query. Reserve the exact root
        // and its subtree before regular Nulang/static route dispatch.
        let path = target.split('?').next().unwrap_or(target);
        path == "/__nulang/sqlite" || path.starts_with(WEB_SQLITE_HTTP_PREFIX)
    }

    /// Auth errors are handled inside this boundary, never by a permissive
    /// fallback to static content or a standard Nulang route.
    pub(crate) fn dispatch(&self, request: &HttpRequest) -> HttpResponse {
        let gate = WebSqliteAuthGate::new(&self.store, self.verifier.as_ref());
        match gate.authorize_headers(&request.headers) {
            Ok(authorized) => match self.handler.handle(request, &authorized) {
                Ok(mut response) => {
                    // Do not allow authenticated tenant data to be cached
                    // by intermediary/shared HTTP caches by default.
                    response
                        .headers
                        .retain(|(name, _)| !name.eq_ignore_ascii_case("cache-control"));
                    response
                        .headers
                        .push(("Cache-Control".into(), "no-store".into()));
                    response
                }
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                    error_response(400, b"Invalid request")
                }
                Err(_) => error_response(500, b"Internal server error"),
            },
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                let mut response = error_response(401, b"Unauthorized");
                response
                    .headers
                    .push(("WWW-Authenticate".into(), "Bearer".into()));
                response
            }
            Err(_) => error_response(503, b"Identity verification unavailable"),
        }
    }
}

fn error_response(status: u16, body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![
            ("Content-Type".into(), "text/plain; charset=utf-8".into()),
            ("Cache-Control".into(), "no-store".into()),
        ],
        body: body.to_vec(),
    }
}
