//! Gitea adapter for the provider-neutral forge gateway.
//!
//! The adapter deliberately does not hold credentials. `HttpTransport` is a
//! host boundary: the Cloud/runtime layer owns base URL selection, TLS, auth,
//! token refresh, retry policy, and audit logging.

use crate::forge::{
    BranchRef, ChangeRef, CheckRun, CommitRef, FileContent, ForgeBackend, ForgeCommand, ForgeError,
    ForgeResponse, MergeMethod, MergeResult, ReviewEvent, ReviewRef,
};
use async_trait::async_trait;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, String>;
}

pub struct GiteaBackend<T> {
    transport: T,
}

impl<T> GiteaBackend<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }
}

#[async_trait]
impl<T> ForgeBackend for GiteaBackend<T>
where
    T: HttpTransport,
{
    async fn execute(&self, command: ForgeCommand) -> Result<ForgeResponse, ForgeError> {
        match command {
            ForgeCommand::ReadFile {
                repository,
                path,
                reference,
            } => {
                validate_file_path(&path)?;
                let mut query = Vec::new();
                if let Some(reference) = reference {
                    query.push(("ref".to_string(), reference));
                }
                let request = HttpRequest {
                    method: HttpMethod::Get,
                    path: format!(
                        "/api/v1/repos/{}/{}/contents/{}",
                        repository.owner,
                        repository.name,
                        encode_path(&path)
                    ),
                    query,
                    body: None,
                };
                let value = self.send_json(request).await?;
                let encoded = required_string(&value, &["content"])?;
                let encoding = value
                    .get("encoding")
                    .and_then(Value::as_str)
                    .unwrap_or("base64");
                let content = if encoding.eq_ignore_ascii_case("base64") {
                    decode_base64(&encoded)?
                } else {
                    encoded.into_bytes()
                };
                Ok(ForgeResponse::File(FileContent {
                    path: value
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or(&path)
                        .to_string(),
                    content,
                    sha: required_string(&value, &["sha"])?,
                }))
            }
            ForgeCommand::CreateBranch {
                repository,
                name,
                from,
            } => {
                require_text("branch name", &name)?;
                require_text("branch source", &from)?;
                let request = HttpRequest {
                    method: HttpMethod::Post,
                    path: format!(
                        "/api/v1/repos/{}/{}/branches",
                        repository.owner, repository.name
                    ),
                    query: Vec::new(),
                    body: Some(json!({
                        "new_branch_name": name,
                        "old_ref_name": from,
                    })),
                };
                let value = self.send_json(request).await?;
                Ok(ForgeResponse::Branch(BranchRef {
                    name: required_string(&value, &["name"])?,
                    commit_id: first_string(&value, &[&["commit", "id"], &["commit", "sha"]])?,
                }))
            }
            ForgeCommand::WriteFile {
                repository,
                path,
                branch,
                content,
                message,
                expected_blob_sha,
            } => {
                validate_file_path(&path)?;
                require_text("branch", &branch)?;
                require_text("commit message", &message)?;
                let method = if expected_blob_sha.is_some() {
                    HttpMethod::Put
                } else {
                    HttpMethod::Post
                };
                let mut body = json!({
                    "branch": branch,
                    "content": encode_base64(&content),
                    "message": message,
                });
                if let Some(sha) = expected_blob_sha {
                    body["sha"] = Value::String(sha);
                }
                let request = HttpRequest {
                    method,
                    path: format!(
                        "/api/v1/repos/{}/{}/contents/{}",
                        repository.owner,
                        repository.name,
                        encode_path(&path)
                    ),
                    query: Vec::new(),
                    body: Some(body),
                };
                let value = self.send_json(request).await?;
                Ok(ForgeResponse::Commit(CommitRef {
                    id: first_string(&value, &[&["commit", "sha"], &["commit", "id"]])?,
                }))
            }
            ForgeCommand::CreateChange {
                repository,
                head,
                base,
                title,
                body,
            } => {
                require_text("pull request head", &head)?;
                require_text("pull request base", &base)?;
                require_text("pull request title", &title)?;
                let request = HttpRequest {
                    method: HttpMethod::Post,
                    path: format!(
                        "/api/v1/repos/{}/{}/pulls",
                        repository.owner, repository.name
                    ),
                    query: Vec::new(),
                    body: Some(json!({
                        "head": head,
                        "base": base,
                        "title": title,
                        "body": body,
                    })),
                };
                let value = self.send_json(request).await?;
                Ok(ForgeResponse::Change(ChangeRef {
                    number: required_u64(&value, &["number"])?,
                    url: first_optional_string(&value, &[&["html_url"], &["url"]]),
                    head: first_string(&value, &[&["head", "ref"], &["head", "label"]])?,
                    base: first_string(&value, &[&["base", "ref"], &["base", "label"]])?,
                    state: required_string(&value, &["state"])?,
                }))
            }
            ForgeCommand::ReviewChange {
                repository,
                number,
                event,
                body,
                commit_id,
            } => {
                let mut payload = json!({
                    "body": body,
                    "event": review_event_name(event),
                });
                if let Some(commit_id) = commit_id {
                    payload["commit_id"] = Value::String(commit_id);
                }
                let request = HttpRequest {
                    method: HttpMethod::Post,
                    path: format!(
                        "/api/v1/repos/{}/{}/pulls/{}/reviews",
                        repository.owner, repository.name, number
                    ),
                    query: Vec::new(),
                    body: Some(payload),
                };
                let value = self.send_json(request).await?;
                Ok(ForgeResponse::Review(ReviewRef {
                    id: value.get("id").and_then(Value::as_u64),
                }))
            }
            ForgeCommand::MergeChange {
                repository,
                number,
                method,
                expected_head_sha,
            } => {
                let mut payload = json!({
                    "do": merge_method_name(method),
                });
                if let Some(head) = expected_head_sha {
                    payload["head_commit_id"] = Value::String(head);
                }
                let request = HttpRequest {
                    method: HttpMethod::Post,
                    path: format!(
                        "/api/v1/repos/{}/{}/pulls/{}/merge",
                        repository.owner, repository.name, number
                    ),
                    query: Vec::new(),
                    body: Some(payload),
                };
                self.send(request).await?;
                Ok(ForgeResponse::Merge(MergeResult { merged: true }))
            }
            ForgeCommand::ListChecks {
                repository,
                reference,
            } => {
                require_text("check reference", &reference)?;
                let request = HttpRequest {
                    method: HttpMethod::Get,
                    path: format!(
                        "/api/v1/repos/{}/{}/commits/{}/statuses",
                        repository.owner,
                        repository.name,
                        encode_component(&reference)
                    ),
                    query: Vec::new(),
                    body: None,
                };
                let value = self.send_json(request).await?;
                let items = value.as_array().ok_or_else(|| {
                    ForgeError::Protocol("Gitea status response must be an array".to_string())
                })?;
                let checks = items
                    .iter()
                    .map(parse_check)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ForgeResponse::Checks(checks))
            }
        }
    }
}

impl<T> GiteaBackend<T>
where
    T: HttpTransport,
{
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, ForgeError> {
        let response = self
            .transport
            .send(request)
            .await
            .map_err(ForgeError::Backend)?;
        if (200..300).contains(&response.status) {
            Ok(response)
        } else {
            Err(ForgeError::Backend(format!(
                "Gitea returned HTTP {}: {}",
                response.status,
                truncate(&response.body, 512)
            )))
        }
    }

    async fn send_json(&self, request: HttpRequest) -> Result<Value, ForgeError> {
        let response = self.send(request).await?;
        serde_json::from_str(&response.body)
            .map_err(|err| ForgeError::Protocol(format!("invalid Gitea JSON response: {err}")))
    }
}

fn review_event_name(event: ReviewEvent) -> &'static str {
    match event {
        ReviewEvent::Approve => "APPROVED",
        ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        ReviewEvent::Comment => "COMMENT",
    }
}

fn merge_method_name(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "merge",
        MergeMethod::Rebase => "rebase",
        MergeMethod::RebaseMerge => "rebase-merge",
        MergeMethod::Squash => "squash",
        MergeMethod::FastForwardOnly => "fast-forward-only",
    }
}

fn parse_check(value: &Value) -> Result<CheckRun, ForgeError> {
    let id = match value.get("id") {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => {
            return Err(ForgeError::Protocol(
                "Gitea status is missing id".to_string(),
            ))
        }
    };
    let state = first_string(value, &[&["status"], &["state"]])?;
    Ok(CheckRun {
        id,
        context: value
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        state,
        target_url: value
            .get("target_url")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn required_string(value: &Value, path: &[&str]) -> Result<String, ForgeError> {
    nested(value, path)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            ForgeError::Protocol(format!(
                "Gitea response is missing string field {}",
                path.join(".")
            ))
        })
}

fn required_u64(value: &Value, path: &[&str]) -> Result<u64, ForgeError> {
    nested(value, path).and_then(Value::as_u64).ok_or_else(|| {
        ForgeError::Protocol(format!(
            "Gitea response is missing integer field {}",
            path.join(".")
        ))
    })
}

fn first_string(value: &Value, paths: &[&[&str]]) -> Result<String, ForgeError> {
    first_optional_string(value, paths).ok_or_else(|| {
        let names = paths
            .iter()
            .map(|path| path.join("."))
            .collect::<Vec<_>>()
            .join(" or ");
        ForgeError::Protocol(format!("Gitea response is missing {names}"))
    })
}

fn first_optional_string(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| {
        nested(value, path)
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn nested<'a>(mut value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    for component in path {
        value = value.get(*component)?;
    }
    Some(value)
}

fn validate_file_path(path: &str) -> Result<(), ForgeError> {
    if path.is_empty() || path.starts_with('/') {
        return Err(ForgeError::InvalidInput(
            "repository file path must be relative and non-empty".to_string(),
        ));
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(ForgeError::InvalidInput(
            "repository file path contains an unsafe segment".to_string(),
        ));
    }
    if path.chars().any(char::is_control) {
        return Err(ForgeError::InvalidInput(
            "repository file path contains a control character".to_string(),
        ));
    }
    Ok(())
}

fn require_text(label: &str, value: &str) -> Result<(), ForgeError> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        Err(ForgeError::InvalidInput(format!(
            "{label} must be non-empty and contain no control characters"
        )))
    } else {
        Ok(())
    }
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_base64(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut index = 0;
    while index + 3 <= input.len() {
        let chunk = ((input[index] as u32) << 16)
            | ((input[index + 1] as u32) << 8)
            | input[index + 2] as u32;
        out.push(BASE64[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(BASE64[((chunk >> 12) & 0x3f) as usize] as char);
        out.push(BASE64[((chunk >> 6) & 0x3f) as usize] as char);
        out.push(BASE64[(chunk & 0x3f) as usize] as char);
        index += 3;
    }

    match input.len() - index {
        1 => {
            let chunk = (input[index] as u32) << 16;
            out.push(BASE64[((chunk >> 18) & 0x3f) as usize] as char);
            out.push(BASE64[((chunk >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let chunk = ((input[index] as u32) << 16) | ((input[index + 1] as u32) << 8);
            out.push(BASE64[((chunk >> 18) & 0x3f) as usize] as char);
            out.push(BASE64[((chunk >> 12) & 0x3f) as usize] as char);
            out.push(BASE64[((chunk >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }

    out
}

fn decode_base64(value: &str) -> Result<Vec<u8>, ForgeError> {
    let compact = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if compact.len() % 4 != 0 {
        return Err(ForgeError::Protocol(
            "invalid base64 length in Gitea file response".to_string(),
        ));
    }

    let mut out = Vec::with_capacity(compact.len() / 4 * 3);
    for quartet in compact.chunks_exact(4) {
        let a = base64_value(quartet[0])? as u32;
        let b = base64_value(quartet[1])? as u32;
        let c_pad = quartet[2] == b'=';
        let d_pad = quartet[3] == b'=';
        if c_pad && !d_pad {
            return Err(ForgeError::Protocol(
                "invalid base64 padding in Gitea file response".to_string(),
            ));
        }

        let c = if c_pad {
            0
        } else {
            base64_value(quartet[2])? as u32
        };
        let d = if d_pad {
            0
        } else {
            base64_value(quartet[3])? as u32
        };
        let chunk = (a << 18) | (b << 12) | (c << 6) | d;
        out.push(((chunk >> 16) & 0xff) as u8);
        if !c_pad {
            out.push(((chunk >> 8) & 0xff) as u8);
        }
        if !d_pad {
            out.push((chunk & 0xff) as u8);
        }
    }
    Ok(out)
}

fn base64_value(byte: u8) -> Result<u8, ForgeError> {
    let value = match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => {
            return Err(ForgeError::Protocol(
                "invalid base64 data in Gitea file response".to_string(),
            ))
        }
    };
    Ok(value)
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{ForgeCommand, MergeMethod, RepositoryRef};
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake, Waker};

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn block_on_immediate<F: Future>(future: F) -> F::Output {
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test future unexpectedly yielded"),
        }
    }

    struct FakeTransport {
        response: HttpResponse,
        seen: Mutex<Vec<HttpRequest>>,
    }

    impl FakeTransport {
        fn new(status: u16, body: impl Into<String>) -> Self {
            Self {
                response: HttpResponse {
                    status,
                    body: body.into(),
                },
                seen: Mutex::new(Vec::new()),
            }
        }

        fn last_request(&self) -> HttpRequest {
            self.seen.lock().unwrap().last().unwrap().clone()
        }
    }

    #[async_trait]
    impl HttpTransport for FakeTransport {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, String> {
            self.seen.lock().unwrap().push(request);
            Ok(self.response.clone())
        }
    }

    fn repo() -> RepositoryRef {
        RepositoryRef::new("acme", "widgets").unwrap()
    }

    #[test]
    fn read_file_decodes_base64_and_encodes_path() {
        let backend = GiteaBackend::new(FakeTransport::new(
            200,
            r#"{"path":"docs/hello world.md","sha":"abc","encoding":"base64","content":"aGVsbG8="}"#,
        ));

        let response = block_on_immediate(backend.execute(ForgeCommand::ReadFile {
            repository: repo(),
            path: "docs/hello world.md".to_string(),
            reference: Some("feature/one".to_string()),
        }))
        .unwrap();

        assert_eq!(
            response,
            ForgeResponse::File(FileContent {
                path: "docs/hello world.md".to_string(),
                content: b"hello".to_vec(),
                sha: "abc".to_string(),
            })
        );
        let request = backend.transport().last_request();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(
            request.path,
            "/api/v1/repos/acme/widgets/contents/docs/hello%20world.md"
        );
        assert_eq!(
            request.query,
            vec![("ref".to_string(), "feature/one".to_string())]
        );
    }

    #[test]
    fn create_branch_uses_gitea_branch_endpoint() {
        let backend = GiteaBackend::new(FakeTransport::new(
            201,
            r#"{"name":"agent/task-1","commit":{"id":"deadbeef"}}"#,
        ));

        let response = block_on_immediate(backend.execute(ForgeCommand::CreateBranch {
            repository: repo(),
            name: "agent/task-1".to_string(),
            from: "main".to_string(),
        }))
        .unwrap();

        assert_eq!(
            response,
            ForgeResponse::Branch(BranchRef {
                name: "agent/task-1".to_string(),
                commit_id: "deadbeef".to_string(),
            })
        );
        let request = backend.transport().last_request();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.path, "/api/v1/repos/acme/widgets/branches");
        assert_eq!(
            request.body,
            Some(json!({
                "new_branch_name": "agent/task-1",
                "old_ref_name": "main",
            }))
        );
    }

    #[test]
    fn write_file_uses_post_for_create_and_base64_content() {
        let backend =
            GiteaBackend::new(FakeTransport::new(201, r#"{"commit":{"sha":"cafebabe"}}"#));

        let response = block_on_immediate(backend.execute(ForgeCommand::WriteFile {
            repository: repo(),
            path: "src/lib.rs".to_string(),
            branch: "agent/task-1".to_string(),
            content: b"hello".to_vec(),
            message: "agent: update".to_string(),
            expected_blob_sha: None,
        }))
        .unwrap();

        assert_eq!(
            response,
            ForgeResponse::Commit(CommitRef {
                id: "cafebabe".to_string(),
            })
        );
        let request = backend.transport().last_request();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.body.unwrap()["content"], "aGVsbG8=");
    }

    #[test]
    fn write_file_uses_put_and_expected_sha_for_update() {
        let backend =
            GiteaBackend::new(FakeTransport::new(200, r#"{"commit":{"sha":"cafebabe"}}"#));

        block_on_immediate(backend.execute(ForgeCommand::WriteFile {
            repository: repo(),
            path: "src/lib.rs".to_string(),
            branch: "agent/task-1".to_string(),
            content: b"next".to_vec(),
            message: "agent: update".to_string(),
            expected_blob_sha: Some("oldsha".to_string()),
        }))
        .unwrap();

        let request = backend.transport().last_request();
        assert_eq!(request.method, HttpMethod::Put);
        assert_eq!(request.body.unwrap()["sha"], "oldsha");
    }

    #[test]
    fn merge_is_a_separate_endpoint_and_preserves_expected_head() {
        let backend = GiteaBackend::new(FakeTransport::new(200, ""));

        let response = block_on_immediate(backend.execute(ForgeCommand::MergeChange {
            repository: repo(),
            number: 17,
            method: MergeMethod::Squash,
            expected_head_sha: Some("deadbeef".to_string()),
        }))
        .unwrap();

        assert_eq!(response, ForgeResponse::Merge(MergeResult { merged: true }));
        let request = backend.transport().last_request();
        assert_eq!(request.path, "/api/v1/repos/acme/widgets/pulls/17/merge");
        assert_eq!(
            request.body,
            Some(json!({
                "do": "squash",
                "head_commit_id": "deadbeef",
            }))
        );
    }

    #[test]
    fn unsafe_file_paths_fail_before_transport() {
        let backend = GiteaBackend::new(FakeTransport::new(200, "{}"));

        let error = block_on_immediate(backend.execute(ForgeCommand::ReadFile {
            repository: repo(),
            path: "../secrets".to_string(),
            reference: None,
        }))
        .unwrap_err();

        assert!(matches!(error, ForgeError::InvalidInput(_)));
        assert!(backend.transport().seen.lock().unwrap().is_empty());
    }

    #[test]
    fn base64_roundtrip_covers_padding_cases() {
        for value in [b"".as_slice(), b"a", b"ab", b"abc", b"hello world"] {
            let encoded = encode_base64(value);
            assert_eq!(decode_base64(&encoded).unwrap(), value);
        }
    }
}
