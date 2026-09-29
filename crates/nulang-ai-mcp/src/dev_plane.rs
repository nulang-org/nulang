//! Dev Plane authorization client for hosted forge execution.

use crate::forge::{CheckRun, ForgeBackend, ForgeCommand, ForgeError, ForgeResponse};
use async_trait::async_trait;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevPlaneRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevPlaneResponse {
    pub status: u16,
    pub body: String,
}

pub type DevPlaneHttpRequest = DevPlaneRequest;
pub type DevPlaneHttpResponse = DevPlaneResponse;

#[async_trait]
pub trait DevPlaneTransport: Send + Sync {
    async fn send(&self, request: DevPlaneRequest) -> Result<DevPlaneResponse, String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeAuthorizationDecision {
    pub allowed: bool,
    pub required_approval: bool,
    pub effect: String,
    pub reason: String,
    pub risk_level: String,
    pub repository: AuthorizedRepository,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedRepository {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub default_branch: String,
}

pub type DevPlaneAuthResponse = ForgeAuthorizationDecision;
pub type DevPlaneRepository = AuthorizedRepository;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevPlaneAuthRequest {
    pub request_id: String,
    pub task_id: String,
    pub run_id: String,
    pub operation: crate::forge::ForgeOperation,
}

impl Serialize for DevPlaneAuthRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("DevPlaneAuthRequest", 4)?;
        state.serialize_field("request_id", &self.request_id)?;
        state.serialize_field("task_id", &self.task_id)?;
        state.serialize_field("run_id", &self.run_id)?;
        state.serialize_field("operation", &self.operation.to_string())?;
        state.end()
    }
}

pub struct DevPlaneAuthorizer<T> {
    transport: T,
    workload: String,
    secret: Vec<u8>,
    task_id: String,
    run_id: String,
}

pub type DevPlaneClient<T> = DevPlaneAuthorizer<T>;

#[derive(Serialize)]
struct DevPlaneExecuteRequest<'a> {
    request_id: &'a str,
    task_id: &'a str,
    run_id: &'a str,
    command: &'a ForgeCommand,
}

#[derive(Deserialize)]
struct DevPlaneErrorResponse {
    error: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevPlaneReconciliationEvidence {
    pub provider: String,
    pub repository: String,
    pub operation: String,
    pub command_type: String,
    pub source: String,
    #[serde(default)]
    pub head_sha: Option<String>,
    #[serde(default)]
    pub blob_sha: Option<String>,
    #[serde(default)]
    pub change_number: Option<u64>,
    #[serde(default)]
    pub review_id: Option<u64>,
    #[serde(default)]
    pub merged: Option<bool>,
    #[serde(default)]
    pub checks: Vec<CheckRun>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevPlaneReconcileResponse {
    pub status: String,
    pub retryable: bool,
    #[serde(default)]
    pub response: Option<ForgeResponse>,
    pub evidence: DevPlaneReconciliationEvidence,
    #[serde(default)]
    pub reason: Option<String>,
}

impl<T> DevPlaneAuthorizer<T> {
    pub fn new(
        transport: T,
        workload: impl Into<String>,
        secret: impl AsRef<[u8]>,
        task_id: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Result<Self, ForgeError> {
        let workload = workload.into();
        let task_id = task_id.into();
        let run_id = run_id.into();
        let secret = secret.as_ref().to_vec();

        if workload.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "Dev Plane workload id must be non-empty".to_string(),
            ));
        }
        if secret.len() < 32 {
            return Err(ForgeError::InvalidInput(
                "Dev Plane workload secret must be at least 32 bytes".to_string(),
            ));
        }
        if task_id.trim().is_empty() || run_id.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "Dev Plane task_id and run_id must be non-empty".to_string(),
            ));
        }

        Ok(Self {
            transport,
            workload,
            secret,
            task_id,
            run_id,
        })
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T> DevPlaneAuthorizer<T>
where
    T: DevPlaneTransport,
{
    pub async fn authorize_at(
        &self,
        operation: crate::forge::ForgeOperation,
        unix_secs: u64,
    ) -> Result<ForgeAuthorizationDecision, ForgeError> {
        let request_id = derive_request_id(
            "authorize",
            &self.task_id,
            &self.run_id,
            operation.to_string().as_bytes(),
        );
        self.authorize_with_request_id_at(&request_id, operation, unix_secs)
            .await
    }

    pub async fn authorize_with_request_id_at(
        &self,
        request_id: &str,
        operation: crate::forge::ForgeOperation,
        unix_secs: u64,
    ) -> Result<ForgeAuthorizationDecision, ForgeError> {
        validate_request_id(request_id)?;
        let request = sign_authorize_request(
            &self.workload,
            &self.secret,
            unix_secs,
            &DevPlaneAuthRequest {
                request_id: request_id.to_string(),
                task_id: self.task_id.clone(),
                run_id: self.run_id.clone(),
                operation,
            },
        )?;

        let response = self
            .transport
            .send(request)
            .await
            .map_err(ForgeError::Backend)?;

        if !(200..300).contains(&response.status) {
            return Err(ForgeError::Backend(format!(
                "Dev Plane returned HTTP {}: {}",
                response.status,
                truncate(&response.body, 512)
            )));
        }

        serde_json::from_str(&response.body)
            .map_err(|err| ForgeError::Protocol(format!("invalid Dev Plane JSON response: {err}")))
    }

    pub fn request_id_for_command(&self, command: &ForgeCommand) -> Result<String, ForgeError> {
        let payload = serde_json::to_vec(command)
            .map_err(|err| ForgeError::Protocol(format!("serialize forge command: {err}")))?;
        Ok(derive_request_id(
            "authorize",
            &self.task_id,
            &self.run_id,
            &payload,
        ))
    }

    pub fn execution_request_id_for_command(
        &self,
        command: &ForgeCommand,
    ) -> Result<String, ForgeError> {
        let payload = serde_json::to_vec(command)
            .map_err(|err| ForgeError::Protocol(format!("serialize forge command: {err}")))?;
        Ok(derive_request_id(
            "execute",
            &self.task_id,
            &self.run_id,
            &payload,
        ))
    }

    pub async fn authorize_command_at(
        &self,
        command: &ForgeCommand,
        unix_secs: u64,
    ) -> Result<ForgeAuthorizationDecision, ForgeError> {
        let request_id = self.request_id_for_command(command)?;
        self.authorize_command_with_request_id_at(&request_id, command, unix_secs)
            .await
    }

    pub async fn authorize_command_with_request_id_at(
        &self,
        request_id: &str,
        command: &ForgeCommand,
        unix_secs: u64,
    ) -> Result<ForgeAuthorizationDecision, ForgeError> {
        let decision = self
            .authorize_with_request_id_at(request_id, command.operation(), unix_secs)
            .await?;
        let expected = command.repository().full_name();
        if decision.repository.full_name != expected {
            return Err(ForgeError::Protocol(format!(
                "Dev Plane repository mismatch: expected {expected}, got {}",
                decision.repository.full_name
            )));
        }
        Ok(decision)
    }

    pub async fn execute_command_at(
        &self,
        command: &ForgeCommand,
        unix_secs: u64,
    ) -> Result<ForgeResponse, ForgeError> {
        let request_id = self.execution_request_id_for_command(command)?;
        self.execute_command_with_request_id_at(&request_id, command, unix_secs)
            .await
    }

    pub async fn execute_command_with_request_id_at(
        &self,
        request_id: &str,
        command: &ForgeCommand,
        unix_secs: u64,
    ) -> Result<ForgeResponse, ForgeError> {
        validate_request_id(request_id)?;
        let body = serde_json::to_vec(&DevPlaneExecuteRequest {
            request_id,
            task_id: &self.task_id,
            run_id: &self.run_id,
            command,
        })
        .map_err(|err| {
            ForgeError::Protocol(format!("serialize Dev Plane execution request: {err}"))
        })?;

        let path = "/api/v1/internal/forge/execute";
        let timestamp = unix_secs.to_string();
        let mut headers = BTreeMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers.insert("X-Dev-Plane-Workload".to_string(), self.workload.clone());
        headers.insert("X-Dev-Plane-Timestamp".to_string(), timestamp.clone());
        headers.insert(
            "X-Dev-Plane-Signature".to_string(),
            sign_request(
                &self.workload,
                &timestamp,
                "POST",
                path,
                &body,
                &self.secret,
            )?,
        );

        let response = self
            .transport
            .send(DevPlaneRequest {
                method: "POST".to_string(),
                path: path.to_string(),
                headers,
                body,
            })
            .await
            .map_err(ForgeError::Backend)?;

        if (200..300).contains(&response.status) {
            return serde_json::from_str(&response.body).map_err(|err| {
                ForgeError::Protocol(format!("invalid Dev Plane execution response: {err}"))
            });
        }

        let parsed = serde_json::from_str::<DevPlaneErrorResponse>(&response.body).ok();
        let repository = command.repository().full_name();
        let operation = command.operation();
        let code = parsed.as_ref().map(|v| v.error.as_str()).unwrap_or("");
        let reason = parsed
            .as_ref()
            .map(|v| {
                if v.reason.is_empty() {
                    v.error.clone()
                } else {
                    v.reason.clone()
                }
            })
            .unwrap_or_else(|| truncate(&response.body, 512));

        match code {
            "approval_required" => Err(ForgeError::ApprovalRequired {
                repository,
                operation,
                reason,
            }),
            "policy_denied" | "approval_rejected" | "approval_expired" => {
                Err(ForgeError::PolicyDenied {
                    repository,
                    operation,
                    reason,
                })
            }
            value if value.contains("uncertain") => Err(ForgeError::Uncertain {
                repository,
                operation,
                reason,
            }),
            _ => Err(ForgeError::Backend(format!(
                "Dev Plane returned HTTP {}: {}",
                response.status,
                truncate(&response.body, 512)
            ))),
        }
    }

    pub async fn reconcile_command_at(
        &self,
        command: &ForgeCommand,
        unix_secs: u64,
    ) -> Result<DevPlaneReconcileResponse, ForgeError> {
        let request_id = self.execution_request_id_for_command(command)?;
        self.reconcile_command_with_request_id_at(&request_id, command, unix_secs)
            .await
    }

    pub async fn reconcile_command_with_request_id_at(
        &self,
        request_id: &str,
        command: &ForgeCommand,
        unix_secs: u64,
    ) -> Result<DevPlaneReconcileResponse, ForgeError> {
        validate_request_id(request_id)?;
        let body = serde_json::to_vec(&DevPlaneExecuteRequest {
            request_id,
            task_id: &self.task_id,
            run_id: &self.run_id,
            command,
        })
        .map_err(|err| {
            ForgeError::Protocol(format!("serialize Dev Plane reconciliation request: {err}"))
        })?;

        let path = "/api/v1/internal/forge/reconcile";
        let timestamp = unix_secs.to_string();
        let mut headers = BTreeMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers.insert("X-Dev-Plane-Workload".to_string(), self.workload.clone());
        headers.insert("X-Dev-Plane-Timestamp".to_string(), timestamp.clone());
        headers.insert(
            "X-Dev-Plane-Signature".to_string(),
            sign_request(
                &self.workload,
                &timestamp,
                "POST",
                path,
                &body,
                &self.secret,
            )?,
        );

        let response = self
            .transport
            .send(DevPlaneRequest {
                method: "POST".to_string(),
                path: path.to_string(),
                headers,
                body,
            })
            .await
            .map_err(ForgeError::Backend)?;

        if response.status == 200 || response.status == 409 {
            if let Ok(reconciliation) =
                serde_json::from_str::<DevPlaneReconcileResponse>(&response.body)
            {
                return Ok(reconciliation);
            }
        }

        Err(ForgeError::Backend(format!(
            "Dev Plane reconciliation returned HTTP {}: {}",
            response.status,
            truncate(&response.body, 512)
        )))
    }
}

/// Hosted forge backend: Nulang sends the provider-neutral command to Dev Plane,
/// which owns policy, replay state, provider credentials, and the side effect.
pub struct DevPlaneExecutionBackend<T> {
    authorizer: DevPlaneAuthorizer<T>,
}

impl<T> DevPlaneExecutionBackend<T> {
    pub fn new(authorizer: DevPlaneAuthorizer<T>) -> Self {
        Self { authorizer }
    }

    pub fn authorizer(&self) -> &DevPlaneAuthorizer<T> {
        &self.authorizer
    }
}

impl<T> DevPlaneExecutionBackend<T>
where
    T: DevPlaneTransport,
{
    pub async fn execute_at(
        &self,
        command: ForgeCommand,
        unix_secs: u64,
    ) -> Result<ForgeResponse, ForgeError> {
        self.authorizer
            .execute_command_at(&command, unix_secs)
            .await
    }

    pub async fn reconcile_at(
        &self,
        command: &ForgeCommand,
        unix_secs: u64,
    ) -> Result<DevPlaneReconcileResponse, ForgeError> {
        self.authorizer
            .reconcile_command_at(command, unix_secs)
            .await
    }
}

#[async_trait]
impl<T> ForgeBackend for DevPlaneExecutionBackend<T>
where
    T: DevPlaneTransport,
{
    async fn execute(&self, command: ForgeCommand) -> Result<ForgeResponse, ForgeError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ForgeError::Backend("system clock is before Unix epoch".to_string()))?
            .as_secs();
        self.execute_at(command, now).await
    }
}

/// Transitional hosted/standalone guard that requires a fresh Dev Plane
/// authorization decision before a provider backend is invoked.
///
/// The outer `ForgeGateway` still enforces the Nulang-local `ForgeSession`.
/// This wrapper adds Dev Plane's independently derived task/run/repository
/// authority. Hosted deployments should ultimately replace the local provider
/// backend with a Dev Plane execution backend so provider credentials never
/// leave Dev Plane.
pub struct DevPlaneGuardedBackend<T, B> {
    authorizer: DevPlaneAuthorizer<T>,
    backend: B,
}

impl<T, B> DevPlaneGuardedBackend<T, B> {
    pub fn new(authorizer: DevPlaneAuthorizer<T>, backend: B) -> Self {
        Self {
            authorizer,
            backend,
        }
    }

    pub fn authorizer(&self) -> &DevPlaneAuthorizer<T> {
        &self.authorizer
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }
}

impl<T, B> DevPlaneGuardedBackend<T, B>
where
    T: DevPlaneTransport,
    B: ForgeBackend,
{
    pub async fn execute_at(
        &self,
        command: ForgeCommand,
        unix_secs: u64,
    ) -> Result<ForgeResponse, ForgeError> {
        let decision = self
            .authorizer
            .authorize_command_at(&command, unix_secs)
            .await?;

        if !decision.allowed {
            let repository = command.repository().full_name();
            let operation = command.operation();
            if decision.required_approval {
                return Err(ForgeError::ApprovalRequired {
                    repository,
                    operation,
                    reason: decision.reason,
                });
            }
            return Err(ForgeError::PolicyDenied {
                repository,
                operation,
                reason: decision.reason,
            });
        }

        self.backend.execute(command).await
    }
}

#[async_trait]
impl<T, B> ForgeBackend for DevPlaneGuardedBackend<T, B>
where
    T: DevPlaneTransport,
    B: ForgeBackend,
{
    async fn execute(&self, command: ForgeCommand) -> Result<ForgeResponse, ForgeError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ForgeError::Backend("system clock is before Unix epoch".to_string()))?
            .as_secs();
        self.execute_at(command, now).await
    }
}

pub fn sign_authorize_request(
    workload: &str,
    secret: &[u8],
    unix_secs: u64,
    request: &DevPlaneAuthRequest,
) -> Result<DevPlaneRequest, ForgeError> {
    if workload.trim().is_empty() {
        return Err(ForgeError::InvalidInput(
            "Dev Plane workload id must be non-empty".to_string(),
        ));
    }
    if secret.len() < 32 {
        return Err(ForgeError::InvalidInput(
            "Dev Plane workload secret must be at least 32 bytes".to_string(),
        ));
    }
    validate_request_id(&request.request_id)?;
    if request.task_id.trim().is_empty() || request.run_id.trim().is_empty() {
        return Err(ForgeError::InvalidInput(
            "Dev Plane task_id and run_id must be non-empty".to_string(),
        ));
    }

    let body = serde_json::to_vec(request)
        .map_err(|err| ForgeError::Protocol(format!("serialize Dev Plane request: {err}")))?;
    let path = "/api/v1/internal/forge/authorize";
    let timestamp = unix_secs.to_string();
    let mut headers = BTreeMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("X-Dev-Plane-Workload".to_string(), workload.to_string());
    headers.insert("X-Dev-Plane-Timestamp".to_string(), timestamp.clone());
    headers.insert(
        "X-Dev-Plane-Signature".to_string(),
        sign_request(workload, &timestamp, "POST", path, &body, secret)?,
    );

    Ok(DevPlaneRequest {
        method: "POST".to_string(),
        path: path.to_string(),
        headers,
        body,
    })
}

fn derive_request_id(scope: &str, task_id: &str, run_id: &str, payload: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    digest.update(scope.as_bytes());
    digest.update(b"\n");
    digest.update(task_id.as_bytes());
    digest.update(b"\n");
    digest.update(run_id.as_bytes());
    digest.update(b"\n");
    digest.update(payload);
    format!("forge:{scope}:{}", hex_lower(&digest.finalize()))
}

fn validate_request_id(request_id: &str) -> Result<(), ForgeError> {
    if !(16..=128).contains(&request_id.len())
        || !request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err(ForgeError::InvalidInput(
            "Dev Plane request_id must be 16-128 safe ASCII characters".to_string(),
        ));
    }
    Ok(())
}

fn sign_request(
    workload: &str,
    timestamp: &str,
    method: &str,
    request_uri: &str,
    body: &[u8],
    secret: &[u8],
) -> Result<String, ForgeError> {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest, Sha256};

    let body_hash = Sha256::digest(body);
    let canonical = format!(
        "{workload}\n{timestamp}\n{}\n{request_uri}\n{}",
        method.to_ascii_uppercase(),
        hex_lower(&body_hash)
    );

    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|_| ForgeError::InvalidInput("invalid Dev Plane workload secret".to_string()))?;
    mac.update(canonical.as_bytes());
    Ok(format!(
        "sha256={}",
        hex_lower(&mac.finalize().into_bytes())
    ))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{ForgeCommand, RepositoryRef};
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

    #[test]
    fn signed_authorization_request_contains_only_task_run_and_operation() {
        let transport = FakeTransport {
            response: DevPlaneResponse {
                status: 200,
                body: r#"{"allowed":true,"required_approval":false,"effect":"allow","reason":"ok","risk_level":"low","repository":{"id":"repo-1","owner":"nulang-org","name":"nulang","full_name":"nulang-org/nulang","default_branch":"main"}}"#.into(),
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

        let decision =
            block_on_immediate(client.authorize_command_at(&read_command(), 1_800_000_000))
                .unwrap();
        assert!(decision.allowed);

        let request = client
            .transport()
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v1/internal/forge/authorize");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["task_id"], "task-1");
        assert_eq!(body["run_id"], "run-1");
        assert_eq!(body["operation"], "repo.read");
        assert!(body.get("repository").is_none());
        assert!(body.get("grants").is_none());
        assert_eq!(request.headers["X-Dev-Plane-Workload"], "nulang-cloud");
        assert_eq!(request.headers["X-Dev-Plane-Timestamp"], "1800000000");
        assert!(request.headers["X-Dev-Plane-Signature"].starts_with("sha256="));
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
    fn non_success_http_status_is_backend_error() {
        let transport = FakeTransport {
            response: DevPlaneResponse {
                status: 401,
                body: r#"{"error":"invalid workload authorization"}"#.into(),
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
        assert!(matches!(err, ForgeError::Backend(_)));
    }

    #[test]
    fn weak_or_empty_configuration_is_rejected() {
        let transport = FakeTransport {
            response: DevPlaneResponse {
                status: 200,
                body: "{}".into(),
            },
            seen: Mutex::new(Vec::new()),
        };
        assert!(DevPlaneAuthorizer::new(
            transport,
            "",
            "0123456789abcdef0123456789abcdef",
            "task-1",
            "run-1"
        )
        .is_err());
    }
}
