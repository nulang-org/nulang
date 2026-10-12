use async_trait::async_trait;
use nulang_ai_mcp::dev_plane::{
    DevPlaneAuthorizer, DevPlaneExecutionBackend, DevPlaneRequest, DevPlaneResponse,
    DevPlaneTransport,
};
use nulang_ai_mcp::forge::{FileContent, ForgeCommand, ForgeError, ForgeResponse, RepositoryRef};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

struct FakeTransport {
    responses: Mutex<Vec<DevPlaneResponse>>,
    seen: Mutex<Vec<DevPlaneRequest>>,
}

#[async_trait]
impl DevPlaneTransport for FakeTransport {
    async fn send(&self, request: DevPlaneRequest) -> Result<DevPlaneResponse, String> {
        self.seen.lock().unwrap().push(request);
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Err("no fake response".into());
        }
        Ok(responses.remove(0))
    }
}

fn backend(response: DevPlaneResponse) -> DevPlaneExecutionBackend<FakeTransport> {
    let client = DevPlaneAuthorizer::new(
        FakeTransport {
            responses: Mutex::new(vec![response]),
            seen: Mutex::new(Vec::new()),
        },
        "nulang-cloud",
        "0123456789abcdef0123456789abcdef",
        "task-1",
        "run-1",
    )
    .unwrap();
    DevPlaneExecutionBackend::new(client)
}

fn read_command() -> ForgeCommand {
    ForgeCommand::ReadFile {
        repository: RepositoryRef::new("nulang-org", "nulang").unwrap(),
        path: "README.md".into(),
        reference: Some("main".into()),
    }
}

#[test]
fn hosted_backend_sends_full_command_without_provider_credentials() {
    let backend = backend(DevPlaneResponse {
        status: 200,
        body: r#"{"type":"file","value":{"path":"README.md","content":[104,105],"sha":"abc"}}"#
            .into(),
    });

    let result = block_on_immediate(backend.execute_at(read_command(), 1_800_000_000)).unwrap();
    assert_eq!(
        result,
        ForgeResponse::File(FileContent {
            path: "README.md".into(),
            content: b"hi".to_vec(),
            sha: "abc".into(),
        })
    );

    let seen = backend.authorizer().transport().seen.lock().unwrap();
    let request = &seen[0];
    assert_eq!(request.path, "/api/v1/internal/forge/execute");
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["task_id"], "task-1");
    assert_eq!(body["run_id"], "run-1");
    assert_eq!(body["command"]["type"], "read_file");
    assert_eq!(body["command"]["repository"]["owner"], "nulang-org");
    assert!(body["request_id"]
        .as_str()
        .unwrap()
        .starts_with("forge:execute:"));
    assert!(body.get("token").is_none());
    assert!(body.get("credential").is_none());
    assert!(body.get("operation").is_none());
}

#[test]
fn hosted_backend_reuses_request_id_across_retries() {
    let client = DevPlaneAuthorizer::new(
        FakeTransport {
            responses: Mutex::new(vec![
                DevPlaneResponse {
                    status: 502,
                    body: r#"{"error":"forge provider execution failed"}"#.into(),
                },
                DevPlaneResponse {
                    status: 200,
                    body:
                        r#"{"type":"file","value":{"path":"README.md","content":[],"sha":"abc"}}"#
                            .into(),
                },
            ]),
            seen: Mutex::new(Vec::new()),
        },
        "nulang-cloud",
        "0123456789abcdef0123456789abcdef",
        "task-1",
        "run-1",
    )
    .unwrap();
    let backend = DevPlaneExecutionBackend::new(client);

    assert!(block_on_immediate(backend.execute_at(read_command(), 1_800_000_000)).is_err());
    block_on_immediate(backend.execute_at(read_command(), 1_800_000_001)).unwrap();

    let seen = backend.authorizer().transport().seen.lock().unwrap();
    let first: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&seen[1].body).unwrap();
    assert_eq!(first["request_id"], second["request_id"]);
}

#[test]
fn hosted_backend_maps_approval_required() {
    let backend = backend(DevPlaneResponse {
        status: 409,
        body: r#"{"error":"approval_required","reason":"branch mutation requires approval"}"#
            .into(),
    });
    let err = block_on_immediate(backend.execute_at(read_command(), 1_800_000_000)).unwrap_err();
    assert!(matches!(err, ForgeError::ApprovalRequired { .. }));
}

#[test]
fn hosted_backend_maps_policy_denied() {
    let backend = backend(DevPlaneResponse {
        status: 403,
        body: r#"{"error":"policy_denied","reason":"role denied"}"#.into(),
    });
    let err = block_on_immediate(backend.execute_at(read_command(), 1_800_000_000)).unwrap_err();
    assert!(matches!(err, ForgeError::PolicyDenied { .. }));
}

#[test]
fn hosted_backend_maps_uncertain_outcome_separately() {
    let backend = backend(DevPlaneResponse {
        status: 409,
        body: r#"{"error":"forge request outcome is uncertain and requires reconciliation"}"#
            .into(),
    });
    let err = block_on_immediate(backend.execute_at(read_command(), 1_800_000_000)).unwrap_err();
    assert!(matches!(err, ForgeError::Uncertain { .. }));
}

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

#[test]
fn hosted_backend_reconciliation_reuses_execution_identity() {
    let client = DevPlaneAuthorizer::new(
        FakeTransport {
            responses: Mutex::new(vec![
                DevPlaneResponse {
                    status: 200,
                    body: r#"{"type":"file","value":{"path":"README.md","content":[],"sha":"abc"}}"#.into(),
                },
                DevPlaneResponse {
                    status: 200,
                    body: r#"{"status":"applied","retryable":false,"response":{"type":"file","value":{"path":"README.md","content":[],"sha":"abc"}},"evidence":{"provider":"gitea","repository":"nulang-org/nulang","operation":"repo.read","command_type":"read_file","source":"provider_response","head_sha":"abc"}}"#.into(),
                },
            ]),
            seen: Mutex::new(Vec::new()),
        },
        "nulang-cloud",
        "0123456789abcdef0123456789abcdef",
        "task-1",
        "run-1",
    )
    .unwrap();
    let backend = DevPlaneExecutionBackend::new(client);
    let command = read_command();

    block_on_immediate(backend.execute_at(command.clone(), 1_800_000_000)).unwrap();
    let reconciled = block_on_immediate(backend.reconcile_at(&command, 1_800_000_001)).unwrap();

    assert_eq!(reconciled.status, "applied");
    assert!(!reconciled.retryable);
    assert_eq!(reconciled.evidence.provider, "gitea");

    let seen = backend.authorizer().transport().seen.lock().unwrap();
    let execute_body: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    let reconcile_body: serde_json::Value = serde_json::from_slice(&seen[1].body).unwrap();
    assert_eq!(execute_body, reconcile_body);
    assert_eq!(seen[1].path, "/api/v1/internal/forge/reconcile");
    assert_eq!(
        execute_body["request_id"], reconcile_body["request_id"],
        "reconciliation must preserve the exact execution identity"
    );
}

#[test]
fn hosted_backend_reconciliation_reports_retryable_absence() {
    let backend = backend(DevPlaneResponse {
        status: 200,
        body: r#"{"status":"not_applied","retryable":true,"evidence":{"provider":"gitea","repository":"nulang-org/nulang","operation":"branch.create","command_type":"create_branch","source":"reconciliation","detail":"branch absent"}}"#.into(),
    });
    let command = ForgeCommand::CreateBranch {
        repository: RepositoryRef::new("nulang-org", "nulang").unwrap(),
        name: "agent/task-1".into(),
        from: "main".into(),
    };

    let decision = block_on_immediate(backend.reconcile_at(&command, 1_800_000_000)).unwrap();
    assert_eq!(decision.status, "not_applied");
    assert!(decision.retryable);
    assert_eq!(decision.evidence.detail.as_deref(), Some("branch absent"));
}

#[test]
fn hosted_backend_reconciliation_preserves_ambiguous_outcome() {
    let backend = backend(DevPlaneResponse {
        status: 409,
        body: r#"{"status":"ambiguous","retryable":false,"evidence":{"provider":"gitea","repository":"nulang-org/nulang","operation":"commit.write","command_type":"write_file","source":"reconciliation","detail":"target differs"},"reason":"target differs"}"#.into(),
    });

    let decision =
        block_on_immediate(backend.reconcile_at(&read_command(), 1_800_000_000)).unwrap();
    assert_eq!(decision.status, "ambiguous");
    assert!(!decision.retryable);
    assert_eq!(decision.reason.as_deref(), Some("target differs"));
}

#[test]
fn hosted_backend_maps_rejected_and_expired_approval_to_policy_denied() {
    for (code, reason) in [
        ("approval_rejected", "forge execution approval was rejected"),
        (
            "approval_expired",
            "forge execution approval expired before execution",
        ),
    ] {
        let backend = backend(DevPlaneResponse {
            status: 403,
            body: format!(r#"{{"error":"{code}","reason":"{reason}"}}"#),
        });

        let err =
            block_on_immediate(backend.execute_at(read_command(), 1_800_000_000)).unwrap_err();
        match err {
            ForgeError::PolicyDenied { reason: got, .. } => assert_eq!(got, reason),
            other => panic!("expected PolicyDenied for {code}, got {other:?}"),
        }
    }
}
