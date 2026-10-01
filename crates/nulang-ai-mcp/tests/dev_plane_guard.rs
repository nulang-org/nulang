use async_trait::async_trait;
use nulang_ai_mcp::dev_plane::{
    DevPlaneAuthorizer, DevPlaneGuardedBackend, DevPlaneRequest, DevPlaneResponse,
    DevPlaneTransport,
};
use nulang_ai_mcp::forge::{ForgeBackend, ForgeCommand, ForgeError, ForgeResponse, RepositoryRef};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
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

struct RecordingBackend {
    calls: AtomicUsize,
}

#[async_trait]
impl ForgeBackend for RecordingBackend {
    async fn execute(&self, _command: ForgeCommand) -> Result<ForgeResponse, ForgeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ForgeResponse::Checks(Vec::new()))
    }
}

fn read_command() -> ForgeCommand {
    ForgeCommand::ReadFile {
        repository: RepositoryRef::new("nulang-org", "nulang").unwrap(),
        path: "README.md".into(),
        reference: Some("main".into()),
    }
}

fn response(
    effect: &str,
    allowed: bool,
    required_approval: bool,
    repository: &str,
) -> DevPlaneResponse {
    let (owner, name) = repository.split_once('/').unwrap();
    DevPlaneResponse {
        status: 200,
        body: format!(
            r#"{{"allowed":{allowed},"required_approval":{required_approval},"effect":"{effect}","reason":"decision reason","risk_level":"medium","repository":{{"id":"repo-1","owner":"{owner}","name":"{name}","full_name":"{repository}","default_branch":"main"}}}}"#
        ),
    }
}

fn guard(response: DevPlaneResponse) -> DevPlaneGuardedBackend<FakeTransport, RecordingBackend> {
    let authorizer = DevPlaneAuthorizer::new(
        FakeTransport {
            response,
            seen: Mutex::new(Vec::new()),
        },
        "nulang-cloud",
        "0123456789abcdef0123456789abcdef",
        "task-1",
        "run-1",
    )
    .unwrap();
    DevPlaneGuardedBackend::new(
        authorizer,
        RecordingBackend {
            calls: AtomicUsize::new(0),
        },
    )
}

#[test]
fn allowed_decision_reaches_provider_once() {
    let guard = guard(response("allow", true, false, "nulang-org/nulang"));
    let result = block_on_immediate(guard.execute_at(read_command(), 1_800_000_000));
    assert!(result.is_ok());
    assert_eq!(guard.backend().calls.load(Ordering::SeqCst), 1);
}

#[test]
fn approval_required_stops_before_provider() {
    let guard = guard(response("ask", false, true, "nulang-org/nulang"));
    let error = block_on_immediate(guard.execute_at(read_command(), 1_800_000_000)).unwrap_err();
    assert!(matches!(error, ForgeError::ApprovalRequired { .. }));
    assert_eq!(guard.backend().calls.load(Ordering::SeqCst), 0);
}

#[test]
fn policy_deny_stops_before_provider() {
    let guard = guard(response("deny", false, false, "nulang-org/nulang"));
    let error = block_on_immediate(guard.execute_at(read_command(), 1_800_000_000)).unwrap_err();
    assert!(matches!(error, ForgeError::PolicyDenied { .. }));
    assert_eq!(guard.backend().calls.load(Ordering::SeqCst), 0);
}

#[test]
fn canonical_repository_mismatch_stops_before_provider() {
    let guard = guard(response("allow", true, false, "evil/other"));
    let error = block_on_immediate(guard.execute_at(read_command(), 1_800_000_000)).unwrap_err();
    assert!(matches!(error, ForgeError::Protocol(_)));
    assert_eq!(guard.backend().calls.load(Ordering::SeqCst), 0);
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
