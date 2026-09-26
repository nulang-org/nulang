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
use crate::package::resolver::parse_semver;
#[cfg(feature = "tcp")]
use parking_lot::Mutex;
#[cfg(feature = "tcp")]
use std::time::Duration;

/// Package registry server.
///
/// Storage layout under `data_dir`:
/// ```text
/// <data-dir>/
///   <name>/
///     <version>.tar.gz | <version>.tar.zst
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
        let content_length_header = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.as_str());
        if method == "PUT" && content_length_header.is_none() {
            Self::write_response(&mut stream, 411, "text/plain", b"Length required");
            return;
        }
        let content_length = match content_length_header {
            Some(value) => match value.parse::<usize>() {
                Ok(value) => value,
                Err(_) => {
                    Self::write_response(&mut stream, 400, "text/plain", b"Bad request");
                    return;
                }
            },
            None => 0,
        };
        if content_length > Self::MAX_BODY_SIZE {
            Self::write_response(&mut stream, 413, "text/plain", b"Payload too large");
            return;
        }

        let mut body = head[body_offset..].to_vec();
        while body.len() < content_length {
            let mut chunk = [0u8; 4096];
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
        if body.len() < content_length {
            Self::write_response(&mut stream, 400, "text/plain", b"Truncated request body");
            return;
        }
        body.truncate(content_length);

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
                Self::handle_put(stream, data_dir, name, version, headers, body);
            }
            ("GET", Some(version)) => Self::handle_get_tarball(stream, data_dir, name, version),
            ("GET", None) => Self::handle_list_versions(stream, data_dir, name),
            _ => Self::write_response(stream, 405, "text/plain", b"Method not allowed"),
        }
    }

    /// PUT /api/v1/packages/<name>/<version> — store an immutable package archive.
    fn handle_put(
        stream: &mut TcpStream,
        data_dir: &Path,
        name: &str,
        version: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) {
        let extension = match archive_extension_for_upload(headers, body) {
            Ok(extension) => extension,
            Err(message) => {
                Self::write_response(stream, 415, "text/plain", message.as_bytes());
                return;
            }
        };

        let dir = data_dir.join(name);
        if std::fs::create_dir_all(&dir).is_err() {
            Self::write_response(stream, 500, "text/plain", b"Internal server error");
            return;
        }

        let reservation = match reserve_package_version(&dir, version) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::write_response(stream, 409, "text/plain", b"Version already exists");
                return;
            }
            Err(_) => {
                Self::write_response(stream, 500, "text/plain", b"Internal server error");
                return;
            }
        };

        if PACKAGE_ARCHIVE_EXTENSIONS
            .iter()
            .any(|archive_extension| dir.join(format!("{version}{archive_extension}")).exists())
        {
            drop(reservation);
            Self::write_response(stream, 409, "text/plain", b"Version already exists");
            return;
        }

        let file = dir.join(format!("{version}{extension}"));
        let write_result = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
            .and_then(|mut output| output.write_all(body));

        match write_result {
            Ok(()) => {
                drop(reservation);
                Self::write_response(stream, 201, "text/plain", b"Created");
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                drop(reservation);
                Self::write_response(stream, 409, "text/plain", b"Version already exists");
            }
            Err(_) => {
                let _ = std::fs::remove_file(&file);
                let _ = std::fs::remove_file(version_lock_path(&dir, version));
                drop(reservation);
                Self::write_response(stream, 500, "text/plain", b"Internal server error");
            }
        }
    }

    /// GET /api/v1/packages/<name>/<version> — return zstd when present,
    /// otherwise fall back to the historical gzip archive.
    fn handle_get_tarball(stream: &mut TcpStream, data_dir: &Path, name: &str, version: &str) {
        let dir = data_dir.join(name);
        for extension in PACKAGE_ARCHIVE_EXTENSIONS {
            let file = dir.join(format!("{version}{extension}"));
            if let Ok(bytes) = std::fs::read(&file) {
                let content_type = if extension == ".tar.zst" {
                    "application/zstd"
                } else {
                    "application/gzip"
                };
                Self::write_response(stream, 200, content_type, &bytes);
                return;
            }
        }
        Self::write_response(stream, 404, "text/plain", b"Not found");
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
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_type()
                    .map(|kind| kind.is_file())
                    .unwrap_or(false)
            })
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter_map(|filename| archive_version_from_filename(&filename).map(str::to_owned))
            .collect();
        versions.sort();
        versions.dedup();
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
            411 => "Length Required",
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
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

#[cfg(feature = "tcp")]
const PACKAGE_ARCHIVE_EXTENSIONS: [&str; 2] = [".tar.zst", ".tar.gz"];

#[cfg(feature = "tcp")]
fn archive_extension_for_upload(
    headers: &[(String, String)],
    body: &[u8],
) -> Result<&'static str, &'static str> {
    use crate::package::archive::{detect_archive_compression, ArchiveCompression};

    let content_type = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase()
        });

    let expected = match content_type.as_deref() {
        None | Some("application/gzip") => ArchiveCompression::Gzip,
        Some("application/zstd") => ArchiveCompression::Zstd,
        Some(_) => return Err("Unsupported package archive content type"),
    };

    let actual = detect_archive_compression(body).ok_or("Unknown package archive compression")?;
    if actual != expected {
        return Err("Package archive content type does not match payload");
    }

    Ok(expected.extension())
}

#[cfg(feature = "tcp")]
const PACKAGE_VERSION_LOCK_SUFFIX: &str = ".publish.lock";

#[cfg(feature = "tcp")]
fn version_lock_path(dir: &Path, version: &str) -> PathBuf {
    dir.join(format!("{version}{PACKAGE_VERSION_LOCK_SUFFIX}"))
}

#[cfg(feature = "tcp")]
fn reserve_package_version(dir: &Path, version: &str) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(version_lock_path(dir, version))
}

#[cfg(feature = "tcp")]
fn archive_version_from_filename(filename: &str) -> Option<&str> {
    PACKAGE_ARCHIVE_EXTENSIONS
        .iter()
        .find_map(|extension| filename.strip_suffix(extension))
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

#[cfg(feature = "tcp")]
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
    #[cfg(feature = "tcp")]
    use super::*;

    #[test]
    #[cfg(feature = "tcp")]
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
    #[cfg(feature = "tcp")]
    fn test_sort_versions_invalid_last() {
        let mut versions = vec!["latest".to_string(), "1.0.0".to_string(), "v2".to_string()];
        sort_versions(&mut versions);
        assert_eq!(versions, vec!["1.0.0", "latest", "v2"]);
    }

    #[test]
    #[cfg(feature = "tcp")]
    fn test_archive_extension_follows_content_type() {
        let zstd = vec![("Content-Type".to_string(), "application/zstd".to_string())];
        let gzip = vec![("content-type".to_string(), "application/gzip".to_string())];
        let absent = Vec::new();

        assert_eq!(
            archive_extension_for_upload(&zstd, &[0x28, 0xb5, 0x2f, 0xfd]).unwrap(),
            ".tar.zst"
        );
        assert_eq!(
            archive_extension_for_upload(&gzip, &[0x1f, 0x8b]).unwrap(),
            ".tar.gz"
        );
        assert_eq!(
            archive_extension_for_upload(&absent, &[0x1f, 0x8b]).unwrap(),
            ".tar.gz"
        );
    }

    #[test]
    #[cfg(feature = "tcp")]
    fn test_archive_upload_rejects_mime_magic_mismatch() {
        let zstd = vec![("Content-Type".to_string(), "application/zstd".to_string())];
        let gzip_bytes = [0x1f, 0x8b, 0x08, 0x00];

        assert!(archive_extension_for_upload(&zstd, &gzip_bytes).is_err());
    }

    #[test]
    #[cfg(feature = "tcp")]
    fn test_archive_upload_rejects_unsupported_content_type() {
        let unsupported = vec![(
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        )];
        let gzip_bytes = [0x1f, 0x8b, 0x08, 0x00];

        assert!(archive_extension_for_upload(&unsupported, &gzip_bytes).is_err());
    }

    #[test]
    #[cfg(feature = "tcp")]
    fn test_version_reservation_is_atomic() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nulang-registry-reservation-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let first = reserve_package_version(&dir, "1.0.0").unwrap();
        let second = reserve_package_version(&dir, "1.0.0").unwrap_err();
        assert_eq!(second.kind(), std::io::ErrorKind::AlreadyExists);

        drop(first);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(feature = "tcp")]
    fn test_archive_filename_parses_both_formats() {
        assert_eq!(
            archive_version_from_filename("1.2.3.tar.zst"),
            Some("1.2.3")
        );
        assert_eq!(archive_version_from_filename("1.2.3.tar.gz"), Some("1.2.3"));
        assert_eq!(archive_version_from_filename("README.md"), None);
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
