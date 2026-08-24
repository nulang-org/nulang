//! Minimal HTTP package registry server.
//!
//! Serves the registry API over HTTP/1.1 on a background thread, using
//! `std::net::TcpListener` + `httparse` (same pattern as
//! `crate::runtime::http_server`). Shutdown is controlled by an
//! `AtomicBool` flag; `stop()` sets it and joins the listener thread.

#[cfg(feature = "tcp")]
use std::io::{Read, Write};
#[cfg(feature = "tcp")]
use std::net::{TcpListener, TcpStream};
#[cfg(feature = "tcp")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "tcp")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "tcp")]
use std::sync::Arc;
#[cfg(feature = "tcp")]
use std::thread;

#[cfg(feature = "tcp")]
use parking_lot::Mutex;
#[cfg(feature = "tcp")]
use std::time::Duration;

use crate::package::resolver::parse_semver;

/// Package registry server.
///
/// Storage layout under `data_dir`:
/// ```text
/// <data-dir>/
///   <name>/
///     <version>.tar.gz
/// ```
///
/// The listener handle lives behind a `Mutex` so that `start(&self)` and
/// `stop(&self)` can manage the background thread through shared references.
#[cfg(feature = "tcp")]
pub struct RegistryServer {
    data_dir: PathBuf,
    auth_token: Option<String>,
    running: Arc<AtomicBool>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

#[cfg(feature = "tcp")]
impl RegistryServer {
    /// Maximum accepted tarball size (64 MiB).
    const MAX_BODY_SIZE: usize = 64 * 1024 * 1024;
    /// Per-connection read timeout.
    const READ_TIMEOUT: Duration = Duration::from_secs(10);

    pub fn new(data_dir: PathBuf, auth_token: Option<String>) -> Self {
        RegistryServer {
            data_dir,
            auth_token,
            running: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
    }

    /// Bind to `bind_addr` and spawn the listener thread.
    ///
    /// Fails with `ErrorKind::AlreadyExists` if the server is already running,
    /// or with the bind error if the address cannot be bound.
    pub fn start(&self, bind_addr: &str) -> std::io::Result<()> {
        let mut handle_guard = self.handle.lock();
        if handle_guard.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "registry server already running",
            ));
        }
        let listener = TcpListener::bind(bind_addr)?;
        let data_dir = self.data_dir.clone();
        let auth_token = self.auth_token.clone();
        let running = self.running.clone();
        let handle = thread::Builder::new()
            .name("nulang-registry-listener".into())
            .spawn(move || {
                Self::listener_loop(listener, data_dir, auth_token, running);
            })?;
        *handle_guard = Some(handle);
        Ok(())
    }

    /// Signal shutdown and join the listener thread. Safe to call multiple
    /// times and from `Drop`.
    pub fn stop(&self) {
        self.running.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.lock().take() {
            let _ = handle.join();
        }
    }

    fn listener_loop(
        listener: TcpListener,
        data_dir: PathBuf,
        auth_token: Option<String>,
        running: Arc<AtomicBool>,
    ) {
        listener.set_nonblocking(true).ok();
        loop {
            if running.load(Ordering::Relaxed) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Self::READ_TIMEOUT));
                    Self::handle_connection(stream, &data_dir, &auth_token);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    }

    fn handle_connection(mut stream: TcpStream, data_dir: &Path, auth_token: &Option<String>) {
        // Read the request head (headers plus any body bytes already received).
        let mut head: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let (method, path, headers, body_offset) = loop {
            match stream.read(&mut chunk) {
                Ok(0) => return, // EOF before a complete request
                Ok(n) => {
                    head.extend_from_slice(&chunk[..n]);
                    let mut httparse_headers = [httparse::EMPTY_HEADER; 64];
                    let mut req = httparse::Request::new(&mut httparse_headers);
                    match req.parse(&head) {
                        Ok(httparse::Status::Complete(body_offset)) => {
                            let method = req.method.unwrap_or("").to_string();
                            let path = req.path.unwrap_or("/").to_string();
                            let headers: Vec<(String, String)> = req
                                .headers
                                .iter()
                                .map(|h| {
                                    (
                                        h.name.to_string(),
                                        String::from_utf8_lossy(h.value).to_string(),
                                    )
                                })
                                .collect();
                            break (method, path, headers, body_offset);
                        }
                        Ok(httparse::Status::Partial) => continue, // need more bytes
                        Err(_) => {
                            Self::write_response(&mut stream, 400, "text/plain", b"Bad request");
                            return;
                        }
                    }
                }
                Err(_) => return,
            }
        };

        // Read the remainder of the body according to Content-Length.
        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let mut body = head[body_offset..].to_vec();
        while body.len() < content_length.min(Self::MAX_BODY_SIZE) {
            let mut chunk = [0u8; 4096];
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
        if content_length > Self::MAX_BODY_SIZE {
            Self::write_response(&mut stream, 413, "text/plain", b"Payload too large");
            return;
        }

        Self::dispatch(
            &mut stream,
            &method,
            &path,
            &headers,
            &body,
            data_dir,
            auth_token,
        );
    }

    fn dispatch(
        stream: &mut TcpStream,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
        data_dir: &Path,
        auth_token: &Option<String>,
    ) {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let route = match segments.as_slice() {
            ["api", "v1", "packages", name] => Some((*name, None)),
            ["api", "v1", "packages", name, version] => Some((*name, Some(*version))),
            _ => None,
        };
        let Some((name, version)) = route else {
            Self::write_response(stream, 404, "text/plain", b"Not found");
            return;
        };
        if !valid_segment(name) || version.is_some_and(|v| !valid_segment(v)) {
            Self::write_response(stream, 404, "text/plain", b"Not found");
            return;
        }

        match (method, version) {
            ("PUT", Some(version)) => {
                if !authorized(headers, auth_token) {
                    Self::write_response(stream, 401, "text/plain", b"Unauthorized");
                    return;
                }
                Self::handle_put(stream, data_dir, name, version, body);
            }
            ("GET", Some(version)) => Self::handle_get_tarball(stream, data_dir, name, version),
            ("GET", None) => Self::handle_list_versions(stream, data_dir, name),
            _ => Self::write_response(stream, 405, "text/plain", b"Method not allowed"),
        }
    }

    /// PUT /api/v1/packages/<name>/<version> — store a tarball.
    fn handle_put(stream: &mut TcpStream, data_dir: &Path, name: &str, version: &str, body: &[u8]) {
        let dir = data_dir.join(name);
        let file = dir.join(format!("{}.tar.gz", version));
        if file.exists() {
            Self::write_response(stream, 409, "text/plain", b"Version already exists");
            return;
        }
        if let Err(_) = std::fs::create_dir_all(&dir) {
            Self::write_response(stream, 500, "text/plain", b"Internal server error");
            return;
        }
        match std::fs::write(&file, body) {
            Ok(()) => Self::write_response(stream, 201, "text/plain", b"Created"),
            Err(_) => Self::write_response(stream, 500, "text/plain", b"Internal server error"),
        }
    }

    /// GET /api/v1/packages/<name>/<version> — return the stored tarball.
    fn handle_get_tarball(stream: &mut TcpStream, data_dir: &Path, name: &str, version: &str) {
        let file = data_dir.join(name).join(format!("{}.tar.gz", version));
        match std::fs::read(&file) {
            Ok(bytes) => {
                Self::write_response(stream, 200, "application/octet-stream", &bytes);
            }
            Err(_) => Self::write_response(stream, 404, "text/plain", b"Not found"),
        }
    }

    /// GET /api/v1/packages/<name> — list published versions as JSON.
    fn handle_list_versions(stream: &mut TcpStream, data_dir: &Path, name: &str) {
        let dir = data_dir.join(name);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                Self::write_response(stream, 404, "text/plain", b"Not found");
                return;
            }
        };
        let mut versions: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|f| f.ends_with(".tar.gz"))
            .map(|f| f.trim_end_matches(".tar.gz").to_string())
            .collect();
        sort_versions(&mut versions);
        let payload = serde_json::json!({ "name": name, "versions": versions });
        let body = payload.to_string();
        Self::write_response(stream, 200, "application/json", body.as_bytes());
    }

    fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
        let status_text = match status {
            200 => "OK",
            201 => "Created",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            413 => "Payload Too Large",
            500 => "Internal Server Error",
            _ => "Unknown",
        };
        let out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status,
            status_text,
            content_type,
            body.len()
        );
        let _ = stream.write_all(out.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }
}

#[cfg(feature = "tcp")]
impl Drop for RegistryServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Validate a package name/version path segment: rejects empty segments,
/// `.`/`..`, and anything outside `[A-Za-z0-9._-]`, which blocks path
/// traversal and absolute paths before they reach the filesystem.
#[cfg(feature = "tcp")]
fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Check `Authorization: Bearer <token>` against the configured token.
/// When no token is configured, all requests are authorized.
#[cfg(feature = "tcp")]
fn authorized(headers: &[(String, String)], auth_token: &Option<String>) -> bool {
    match auth_token {
        None => true,
        Some(expected) => headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("authorization")
                && v.split_once(' ').is_some_and(|(scheme, token)| {
                    scheme.eq_ignore_ascii_case("Bearer") && token.trim() == expected
                })
        }),
    }
}

/// Sort versions semver-aware so `0.10.0` sorts after `0.9.0`. Keys that
/// are not valid semver (e.g. tags published before validation, or
/// prereleases, which the resolver cannot consume) sort after all valid
/// versions in lexicographic order, keeping the listing deterministic.
fn sort_versions(versions: &mut [String]) {
    versions.sort_by(|a, b| match (parse_semver(a), parse_semver(b)) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sort_versions_semver_aware() {
        let mut versions = vec![
            "0.10.0".to_string(),
            "0.9.0".to_string(),
            "1.0.0".to_string(),
            "0.9.10".to_string(),
        ];
        sort_versions(&mut versions);
        assert_eq!(versions, vec!["0.9.0", "0.9.10", "0.10.0", "1.0.0"]);
    }

    #[test]
    fn test_sort_versions_invalid_last() {
        let mut versions = vec!["latest".to_string(), "1.0.0".to_string(), "v2".to_string()];
        sort_versions(&mut versions);
        assert_eq!(versions, vec!["1.0.0", "latest", "v2"]);
    }
}

#[cfg(not(feature = "tcp"))]
#[allow(dead_code)] // fields kept for API parity; never read without `tcp`
pub struct RegistryServer {
    data_dir: PathBuf,
    auth_token: Option<String>,
}

#[cfg(not(feature = "tcp"))]
impl RegistryServer {
    pub fn new(data_dir: PathBuf, auth_token: Option<String>) -> Self {
        RegistryServer {
            data_dir,
            auth_token,
        }
    }

    /// Stub: the `tcp` feature is disabled, so the server cannot start.
    pub fn start(&self, _bind_addr: &str) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "registry server disabled (feature 'tcp' not enabled)",
        ))
    }

    pub fn stop(&self) {}
}

#[cfg(not(feature = "tcp"))]
impl Drop for RegistryServer {
    fn drop(&mut self) {
        self.stop();
    }
}
