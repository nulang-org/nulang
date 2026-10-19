//! MCP tool bindings for the provider-neutral forge gateway.
//!
//! The host injects a trusted `ForgeSession` when registering tools. Caller
//! arguments are only command data; authority/session fields are rejected.

use crate::forge::{
    ForgeBackend, ForgeCommand, ForgeGateway, ForgeSession, MergeMethod, RepositoryRef, ReviewEvent,
};
use crate::{ToolHandler, ToolRegistry, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub async fn register_forge_tools<B>(
    registry: &ToolRegistry,
    gateway: Arc<ForgeGateway<B>>,
    session: ForgeSession,
) where
    B: ForgeBackend + 'static,
{
    let session = Arc::new(session);
    for (name, description, parameters, kind) in tool_specs() {
        registry
            .register(
                ToolSpec {
                    name: name.to_string(),
                    description: description.to_string(),
                    parameters,
                },
                Arc::new(ForgeToolHandler {
                    gateway: gateway.clone(),
                    session: session.clone(),
                    kind,
                }),
            )
            .await;
    }
}

#[derive(Clone, Copy)]
enum ToolKind {
    ReadFile,
    CreateBranch,
    WriteFile,
    CreateChange,
    ReviewChange,
    MergeChange,
    ListChecks,
}

struct ForgeToolHandler<B> {
    gateway: Arc<ForgeGateway<B>>,
    session: Arc<ForgeSession>,
    kind: ToolKind,
}

#[async_trait]
impl<B> ToolHandler for ForgeToolHandler<B>
where
    B: ForgeBackend + 'static,
{
    async fn call(&self, args: Value) -> Result<Value, String> {
        reject_authority_fields(&args)?;
        let command = parse_command(self.kind, args)?;
        let response = self
            .gateway
            .execute(&self.session, command)
            .await
            .map_err(|err| err.to_string())?;
        serde_json::to_value(response).map_err(|err| err.to_string())
    }
}

fn reject_authority_fields(args: &Value) -> Result<(), String> {
    let object = args
        .as_object()
        .ok_or_else(|| "forge tool arguments must be a JSON object".to_string())?;
    for forbidden in [
        "session",
        "grants",
        "operations",
        "expires_at_unix_secs",
        "subject",
    ] {
        if object.contains_key(forbidden) {
            return Err(format!(
                "forge authority is host-managed; caller field {forbidden:?} is not allowed"
            ));
        }
    }
    Ok(())
}

fn parse_command(kind: ToolKind, args: Value) -> Result<ForgeCommand, String> {
    let object = args
        .as_object()
        .ok_or_else(|| "forge tool arguments must be a JSON object".to_string())?;
    let repository = parse_repository(object)?;

    match kind {
        ToolKind::ReadFile => Ok(ForgeCommand::ReadFile {
            repository,
            path: required_string(object, "path")?,
            reference: optional_string(object, "reference")?,
        }),
        ToolKind::CreateBranch => Ok(ForgeCommand::CreateBranch {
            repository,
            name: required_string(object, "name")?,
            from: required_string(object, "from")?,
        }),
        ToolKind::WriteFile => {
            let content = required_string(object, "content")?.into_bytes();
            Ok(ForgeCommand::WriteFile {
                repository,
                path: required_string(object, "path")?,
                branch: required_string(object, "branch")?,
                content,
                message: required_string(object, "message")?,
                expected_blob_sha: optional_string(object, "expected_blob_sha")?,
            })
        }
        ToolKind::CreateChange => Ok(ForgeCommand::CreateChange {
            repository,
            head: required_string(object, "head")?,
            base: required_string(object, "base")?,
            title: required_string(object, "title")?,
            body: optional_string(object, "body")?.unwrap_or_default(),
        }),
        ToolKind::ReviewChange => Ok(ForgeCommand::ReviewChange {
            repository,
            number: required_u64(object, "number")?,
            event: parse_review_event(&required_string(object, "event")?)?,
            body: optional_string(object, "body")?.unwrap_or_default(),
            commit_id: optional_string(object, "commit_id")?,
        }),
        ToolKind::MergeChange => Ok(ForgeCommand::MergeChange {
            repository,
            number: required_u64(object, "number")?,
            method: parse_merge_method(
                optional_string(object, "method")?
                    .as_deref()
                    .unwrap_or("merge"),
            )?,
            expected_head_sha: optional_string(object, "expected_head_sha")?,
        }),
        ToolKind::ListChecks => Ok(ForgeCommand::ListChecks {
            repository,
            reference: required_string(object, "reference")?,
        }),
    }
}

fn parse_repository(object: &Map<String, Value>) -> Result<RepositoryRef, String> {
    RepositoryRef::new(
        required_string(object, "owner")?,
        required_string(object, "repo")?,
    )
    .map_err(|err| err.to_string())
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing or empty string field {key:?}"))
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("field {key:?} must be a string")),
    }
}

fn required_u64(object: &Map<String, Value>, key: &str) -> Result<u64, String> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing unsigned integer field {key:?}"))
}

fn parse_review_event(value: &str) -> Result<ReviewEvent, String> {
    match value {
        "approve" => Ok(ReviewEvent::Approve),
        "request_changes" => Ok(ReviewEvent::RequestChanges),
        "comment" => Ok(ReviewEvent::Comment),
        _ => Err(format!("unsupported review event {value:?}")),
    }
}

fn parse_merge_method(value: &str) -> Result<MergeMethod, String> {
    match value {
        "merge" => Ok(MergeMethod::Merge),
        "rebase" => Ok(MergeMethod::Rebase),
        "rebase_merge" => Ok(MergeMethod::RebaseMerge),
        "squash" => Ok(MergeMethod::Squash),
        "fast_forward_only" => Ok(MergeMethod::FastForwardOnly),
        _ => Err(format!("unsupported merge method {value:?}")),
    }
}

fn tool_specs() -> Vec<(&'static str, &'static str, Value, ToolKind)> {
    let schema = |mut properties: Map<String, Value>, required: &[&str]| {
        properties.insert("owner".into(), json!({"type":"string"}));
        properties.insert("repo".into(), json!({"type":"string"}));
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        })
    };

    vec![
        (
            "forge.read_file",
            "Read one file from an authorized repository.",
            schema(
                Map::from_iter([
                    ("path".into(), json!({"type":"string"})),
                    ("reference".into(), json!({"type":"string"})),
                ]),
                &["owner", "repo", "path"],
            ),
            ToolKind::ReadFile,
        ),
        (
            "forge.create_branch",
            "Create a branch from an explicit source revision.",
            schema(
                Map::from_iter([
                    ("name".into(), json!({"type":"string"})),
                    ("from".into(), json!({"type":"string"})),
                ]),
                &["owner", "repo", "name", "from"],
            ),
            ToolKind::CreateBranch,
        ),
        (
            "forge.write_file",
            "Create or update a file on an authorized branch.",
            schema(
                Map::from_iter([
                    ("path".into(), json!({"type":"string"})),
                    ("branch".into(), json!({"type":"string"})),
                    ("content".into(), json!({"type":"string"})),
                    ("message".into(), json!({"type":"string"})),
                    ("expected_blob_sha".into(), json!({"type":"string"})),
                ]),
                &["owner", "repo", "path", "branch", "content", "message"],
            ),
            ToolKind::WriteFile,
        ),
        (
            "forge.create_change",
            "Open a pull/merge request from head to base.",
            schema(
                Map::from_iter([
                    ("head".into(), json!({"type":"string"})),
                    ("base".into(), json!({"type":"string"})),
                    ("title".into(), json!({"type":"string"})),
                    ("body".into(), json!({"type":"string"})),
                ]),
                &["owner", "repo", "head", "base", "title"],
            ),
            ToolKind::CreateChange,
        ),
        (
            "forge.review_change",
            "Submit a review on an existing change.",
            schema(
                Map::from_iter([
                    ("number".into(), json!({"type":"integer","minimum":1})),
                    (
                        "event".into(),
                        json!({"type":"string","enum":["approve","request_changes","comment"]}),
                    ),
                    ("body".into(), json!({"type":"string"})),
                    ("commit_id".into(), json!({"type":"string"})),
                ]),
                &["owner", "repo", "number", "event"],
            ),
            ToolKind::ReviewChange,
        ),
        (
            "forge.merge_change",
            "Merge an authorized change, optionally pinned to an expected head SHA.",
            schema(
                Map::from_iter([
                    ("number".into(), json!({"type":"integer","minimum":1})),
                    (
                        "method".into(),
                        json!({"type":"string","enum":["merge","rebase","rebase_merge","squash","fast_forward_only"]}),
                    ),
                    ("expected_head_sha".into(), json!({"type":"string"})),
                ]),
                &["owner", "repo", "number"],
            ),
            ToolKind::MergeChange,
        ),
        (
            "forge.list_checks",
            "List status/check evidence for a revision.",
            schema(
                Map::from_iter([("reference".into(), json!({"type":"string"}))]),
                &["owner", "repo", "reference"],
            ),
            ToolKind::ListChecks,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{CheckRun, ForgeError, ForgeGrant, ForgeOperation, ForgeResponse};
    use crate::{JsonRpcRequest, McpServer};
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    struct FakeBackend {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ForgeBackend for FakeBackend {
        async fn execute(&self, command: ForgeCommand) -> Result<ForgeResponse, ForgeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match command {
                ForgeCommand::ListChecks { .. } => Ok(ForgeResponse::Checks(vec![CheckRun {
                    id: "1".into(),
                    context: "ci".into(),
                    state: "success".into(),
                    target_url: None,
                }])),
                _ => Ok(ForgeResponse::Checks(Vec::new())),
            }
        }
    }

    fn repo() -> RepositoryRef {
        RepositoryRef::new("nulang-org", "nulang").unwrap()
    }

    #[test]
    fn host_registration_exposes_only_command_fields() {
        let registry = ToolRegistry::new();
        let session = ForgeSession::new(
            "coder",
            vec![ForgeGrant::new(repo(), [ForgeOperation::RepoRead])],
        )
        .unwrap();
        block_on_immediate(register_forge_tools(
            &registry,
            Arc::new(ForgeGateway::new(FakeBackend {
                calls: AtomicUsize::new(0),
            })),
            session,
        ));

        let tools = block_on_immediate(registry.list_tools());
        let read = tools
            .iter()
            .find(|tool| tool.name == "forge.read_file")
            .unwrap();
        let properties = read.parameters["properties"].as_object().unwrap();
        assert!(!properties.contains_key("session"));
        assert!(!properties.contains_key("grants"));
        assert!(!properties.contains_key("operations"));
    }

    #[test]
    fn caller_cannot_inject_authority_fields() {
        let registry = Arc::new(ToolRegistry::new());
        let backend = Arc::new(ForgeGateway::new(FakeBackend {
            calls: AtomicUsize::new(0),
        }));
        let session = ForgeSession::new("reader", vec![]).unwrap();
        block_on_immediate(register_forge_tools(&registry, backend, session));
        let server = McpServer::new(registry);

        let response = block_on_immediate(server.handle_request(JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "tools/call".into(),
            params: Some(json!({
                "name":"forge.read_file",
                "arguments":{
                    "owner":"nulang-org",
                    "repo":"nulang",
                    "path":"README.md",
                    "session":{"operations":["repo.read"]}
                }
            })),
        }));

        assert!(response.result.is_none());
        assert!(response
            .error
            .unwrap()
            .message
            .contains("authority is host-managed"));
    }

    #[test]
    fn coder_session_cannot_merge_even_when_tool_is_visible() {
        let registry = Arc::new(ToolRegistry::new());
        let backend = Arc::new(ForgeGateway::new(FakeBackend {
            calls: AtomicUsize::new(0),
        }));
        let session = ForgeSession::new(
            "coder",
            vec![ForgeGrant::new(
                repo(),
                [
                    ForgeOperation::RepoRead,
                    ForgeOperation::BranchCreate,
                    ForgeOperation::CommitWrite,
                    ForgeOperation::ChangeCreate,
                ],
            )],
        )
        .unwrap();
        block_on_immediate(register_forge_tools(&registry, backend, session));
        let server = McpServer::new(registry);

        let response = block_on_immediate(server.handle_request(JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(2)),
            method: "tools/call".into(),
            params: Some(json!({
                "name":"forge.merge_change",
                "arguments":{"owner":"nulang-org","repo":"nulang","number":42}
            })),
        }));

        assert!(response.result.is_none());
        assert!(response.error.unwrap().message.contains("authority denied"));
    }

    #[test]
    fn permitted_check_read_reaches_backend() {
        let registry = Arc::new(ToolRegistry::new());
        let session = ForgeSession::new(
            "reviewer",
            vec![ForgeGrant::new(repo(), [ForgeOperation::CheckRead])],
        )
        .unwrap();
        block_on_immediate(register_forge_tools(
            &registry,
            Arc::new(ForgeGateway::new(FakeBackend {
                calls: AtomicUsize::new(0),
            })),
            session,
        ));
        let server = McpServer::new(registry);

        let response = block_on_immediate(server.handle_request(JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(3)),
            method: "tools/call".into(),
            params: Some(json!({
                "name":"forge.list_checks",
                "arguments":{"owner":"nulang-org","repo":"nulang","reference":"deadbeef"}
            })),
        }));

        assert!(response.error.is_none());
        assert!(response.result.is_some());
    }
}
