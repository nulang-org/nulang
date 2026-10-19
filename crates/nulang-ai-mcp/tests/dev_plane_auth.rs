use async_trait::async_trait;
use nulang_ai_mcp::dev_plane::{
    DevPlaneAuthorizer, DevPlaneRequest, DevPlaneResponse, DevPlaneTransport,
};
use nulang_ai_mcp::forge::{ForgeCommand, ForgeError, RepositoryRef};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

struct FakeTransport {
    response: DevPlaneResponse,
    seen: Mutex<Vec<DevPlaneRequest>>,
}

#[async_trait]
impl DevPlaneTransport for FakeTransport {
    async fn send(&self, request: DevPlaneRequest) -> Result<DevPlaneResponse, String> {
        self.seen.lock().unwrap().push(request);
        Ok(self.response.clone())
    }
}

fn read_command() -> ForgeCommand {
    ForgeCommand::ReadFile {
        repository: RepositoryRef::new("nulang-org", "nulang").unwrap(),
        path: "README.md".into(),
        reference: Some("main".into()),
    }
}

fn allow_response() -> DevPlaneResponse {
    DevPlaneResponse {
        status: 200,
        body: r#"{"allowed":true,"required_approval":false,"effect":"allow","reason":"ok","risk_level":"low","repository":{"id":"repo-1","owner":"nulang-org","name":"nulang","full_name":"nulang-org/nulang","default_branch":"main"}}"#.into(),
    }
}

#[test]
fn canonical_signature_matches_dev_plane_contract() {
    let transport = FakeTransport {
        response: allow_response(),
        seen: Mutex::new(Vec::new()),
    };
    let client = DevPlaneAuthorizer::new(
        transport,
        "nulang-cloud",
        "0123456789abcdef0123456789abcdef",
        "task-1",
        "run-1",
    )
    .unwrap();

    let decision = block_on_immediate(client.authorize_command_with_request_id_at(
        "req-0000000000000001",
        &read_command(),
        1_800_000_000,
    ))
    .unwrap();
    assert!(decision.allowed);

    let seen = client.transport().seen.lock().unwrap();
    let request = seen.last().unwrap();
    assert_eq!(
        String::from_utf8(request.body.clone()).unwrap(),
        r#"{"request_id":"req-0000000000000001","task_id":"task-1","run_id":"run-1","operation":"repo.read"}"#
    );
    assert_eq!(
        request.headers["X-Dev-Plane-Signature"],
        "sha256=38687a65e8fb059b7744e5bce9afef84733693d906a520730defbb80e276347f"
    );
}

#[test]
fn canonical_repository_mismatch_fails_closed() {
    let transport = FakeTransport {
        response: DevPlaneResponse {
            status: 200,
            body: r#"{"allowed":true,"required_approval":false,"effect":"allow","reason":"ok","risk_level":"low","repository":{"id":"repo-2","owner":"evil","name":"other","full_name":"evil/other","default_branch":"main"}}"#.into(),
        },
        seen: Mutex::new(Vec::new()),
    };
    let client = DevPlaneAuthorizer::new(
        transport,
        "nulang-cloud",
        "0123456789abcdef0123456789abcdef",
        "task-1",
        "run-1",
    )
    .unwrap();

    let err = block_on_immediate(client.authorize_command_at(&read_command(), 1_800_000_000))
        .unwrap_err();
    assert!(matches!(err, ForgeError::Protocol(_)));
}

#[test]
fn weak_secret_is_rejected() {
    let transport = FakeTransport {
        response: allow_response(),
        seen: Mutex::new(Vec::new()),
    };
    assert!(
        DevPlaneAuthorizer::new(transport, "nulang-cloud", "short", "task-1", "run-1").is_err()
    );
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
fn default_command_request_id_is_deterministic_and_host_derived() {
    let transport = FakeTransport {
        response: allow_response(),
        seen: Mutex::new(Vec::new()),
    };
    let client = DevPlaneAuthorizer::new(
        transport,
        "nulang-cloud",
        "0123456789abcdef0123456789abcdef",
        "task-1",
        "run-1",
    )
    .unwrap();

    block_on_immediate(client.authorize_command_at(&read_command(), 1_800_000_000)).unwrap();
    block_on_immediate(client.authorize_command_at(&read_command(), 1_800_000_001)).unwrap();

    let seen = client.transport().seen.lock().unwrap();
    let first: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&seen[1].body).unwrap();
    let first_id = first["request_id"].as_str().expect("request_id");
    let second_id = second["request_id"].as_str().expect("request_id");

    assert_eq!(first_id, second_id);
    assert!(first_id.starts_with("forge:authorize:"));
    assert!(first.get("repository").is_none());
    assert!(first.get("grants").is_none());
}
