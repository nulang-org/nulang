//! Opt-in, fail-closed container test harness.
//!
//! This is a local development runner, not a hardened multi-tenant sandbox.
//! Its command and OCI image are provided by a trusted host, never a model.
//! The source tree is mounted read-only with network disabled. Test outputs
//! are held in private host receipts and checked by a separate verifier.

use crate::{EvidenceVerifier, Worker, WorkerExecution};
use nulang_ai_core::{AcceptanceEvidence, Task, TaskStatus};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(40);

#[derive(Clone, Debug)]
pub struct SandboxConfig {
    workspace: PathBuf,
    receipt_dir: PathBuf,
    image: String,
    argv: Vec<String>,
    criterion: String,
    timeout: Duration,
}

impl SandboxConfig {
    /// A pinned OCI image and host-chosen argv are required. Commands are not
    /// inferred from task descriptions or model output.
    pub fn new(
        workspace: impl AsRef<Path>,
        receipt_dir: impl AsRef<Path>,
        image: impl Into<String>,
        argv: Vec<String>,
        criterion: impl Into<String>,
    ) -> Result<Self, String> {
        let workspace = workspace.as_ref().canonicalize().map_err(|e| e.to_string())?;
        if !workspace.is_dir() {
            return Err("workspace must be a directory".into());
        }
        // Docker --mount uses comma-separated key=value fields. Reject paths
        // that could be reinterpreted as additional mount options.
        let mount_source = workspace.to_str().ok_or("workspace must be UTF-8")?;
        if mount_source.chars().any(|ch| matches!(ch, ',' | '\n' | '\r')) {
            return Err("workspace contains unsafe Docker mount delimiters".into());
        }
        let image = image.into();
        let Some((name, digest)) = image.rsplit_once("@sha256:") else {
            return Err("OCI image must be pinned with @sha256:<64 hex digits>".into());
        };
        if name.is_empty() || digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid pinned OCI image digest".into());
        }
        if argv.is_empty() || argv.iter().any(|part| part.is_empty()) {
            return Err("host must supply a nonempty executable and argv".into());
        }
        let criterion = criterion.into();
        if criterion.trim().is_empty() {
            return Err("acceptance criterion cannot be empty".into());
        }
        let receipt_dir_input = receipt_dir.as_ref();
        let existed = receipt_dir_input.exists();
        fs::create_dir_all(receipt_dir_input).map_err(|e| e.to_string())?;
        let receipt_dir = receipt_dir_input.canonicalize().map_err(|e| e.to_string())?;
        if receipt_dir.starts_with(&workspace) {
            return Err("receipts must live outside the mounted workspace".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&receipt_dir)
                .map_err(|e| e.to_string())?
                .permissions()
                .mode();
            if existed && (mode & 0o077) != 0 {
                return Err("existing receipt directory must be private (mode 0700)".into());
            }
            if !existed {
                fs::set_permissions(&receipt_dir, fs::Permissions::from_mode(0o700))
                    .map_err(|e| e.to_string())?;
            }
        }

        Ok(Self {
            workspace,
            receipt_dir,
            image,
            argv,
            criterion,
            timeout: Duration::from_secs(900),
        })
    }

    /// Reject zero or excessively long timeouts.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, String> {
        if timeout.is_zero() || timeout > Duration::from_secs(3600) {
            return Err("sandbox timeout must be between 1 second and 1 hour".into());
        }
        self.timeout = timeout;
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPlan {
    pub workspace: PathBuf,
    pub image: String,
    pub argv: Vec<String>,
    pub timeout: Duration,
}

impl SandboxPlan {
    pub fn from_config(config: &SandboxConfig) -> Self {
        Self {
            workspace: config.workspace.clone(),
            image: config.image.clone(),
            argv: config.argv.clone(),
            timeout: config.timeout,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait ContainerDriver: Send + Sync {
    fn run(&self, plan: &SandboxPlan) -> Result<SandboxOutcome, String>;
}

/// Local Docker/Podman-compatible CLI adapter. The executable is selected
/// by a trusted host; arguments are passed directly, with no shell expansion.
pub struct DockerDriver {
    executable: PathBuf,
}

impl Default for DockerDriver {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("docker"),
        }
    }
}

impl DockerDriver {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self { executable: executable.into() }
    }

    pub fn arguments(plan: &SandboxPlan, container_name: &str) -> Vec<String> {
        vec![
            "run".into(),
            "--rm".into(),
            format!("--name={container_name}"),
            "--pull=never".into(),
            "--network=none".into(),
            "--read-only".into(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--pids-limit=64".into(),
            "--memory=1g".into(),
            "--cpus=1".into(),
            "--user=65534:65534".into(),
            "--workdir=/workspace".into(),
            format!(
                "--mount=type=bind,src={},dst=/workspace,readonly",
                plan.workspace.display()
            ),
            "--tmpfs=/tmp:rw,nosuid,nodev,size=512m".into(),
            "--env=HOME=/tmp".into(),
            "--env=CARGO_TARGET_DIR=/tmp/cargo-target".into(),
            format!("--entrypoint={}", plan.argv[0]),
            plan.image.clone(),
        ]
        .into_iter()
        .chain(plan.argv.iter().skip(1).cloned())
        .collect()
    }

    fn remove_container(&self, name: &str) -> Result<(), String> {
        // A killed Docker CLI does not prove the daemon stopped the container.
        let mut child = Command::new(&self.executable)
            .args(["rm", "-f", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    return Err(format!("container cleanup returned {status}"));
                }
                Err(err) => return Err(err.to_string()),
                Ok(None) if start.elapsed() >= Duration::from_secs(5) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("container cleanup timed out".into());
                }
                Ok(None) => std::thread::sleep(POLL_INTERVAL),
            }
        }
    }
}

impl ContainerDriver for DockerDriver {
    fn run(&self, plan: &SandboxPlan) -> Result<SandboxOutcome, String> {
        let name = format!("nulang-ai-{}", Uuid::new_v4());
        let mut stdout = tempfile::tempfile().map_err(|e| e.to_string())?;
        let mut stderr = tempfile::tempfile().map_err(|e| e.to_string())?;
        let mut child = Command::new(&self.executable)
            .args(Self::arguments(plan, &name))
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(stderr.try_clone().map_err(|e| e.to_string())?))
            .spawn()
            .map_err(|e| e.to_string())?;
        let start = Instant::now();
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code().ok_or_else(|| "sandbox terminated by signal".to_string()),
                Err(err) => break Err(err.to_string()),
                Ok(None) => {
                    let bytes = stdout.metadata().map(|m| m.len()).unwrap_or(OUTPUT_LIMIT + 1)
                        .saturating_add(stderr.metadata().map(|m| m.len()).unwrap_or(OUTPUT_LIMIT + 1));
                    if bytes > OUTPUT_LIMIT {
                        break Err("sandbox output limit exceeded".into());
                    }
                    if start.elapsed() >= plan.timeout {
                        break Err("sandbox execution timed out".into());
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        };
        let exit_code = match result {
            Ok(code) => code,
            Err(reason) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match self.remove_container(&name) {
                    Ok(()) => reason,
                    Err(cleanup) => format!(
                        "{reason}; cleanup unconfirmed for container {name}: {cleanup}"
                    ),
                });
            }
        };
        let total_bytes = stdout.metadata().map_err(|e| e.to_string())?.len()
            .saturating_add(stderr.metadata().map_err(|e| e.to_string())?.len());
        if total_bytes > OUTPUT_LIMIT {
            return Err("sandbox output limit exceeded".into());
        }
        stdout.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        stderr.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let mut out = String::new();
        let mut err = String::new();
        stdout.take(OUTPUT_LIMIT).read_to_string(&mut out).map_err(|e| e.to_string())?;
        stderr.take(OUTPUT_LIMIT).read_to_string(&mut err).map_err(|e| e.to_string())?;
        Ok(SandboxOutcome { exit_code, stdout: out, stderr: err })
    }
}

#[derive(Clone, Debug)]
struct Receipt {
    task_id: Uuid,
    criterion: String,
    log_file: PathBuf,
    digest: [u8; 32],
}

type ReceiptRegistry = Arc<Mutex<HashMap<String, Receipt>>>;

pub struct SandboxHarness {
    config: SandboxConfig,
    driver: Arc<dyn ContainerDriver>,
    receipts: ReceiptRegistry,
}

impl SandboxHarness {
    pub fn docker(config: SandboxConfig) -> Self {
        Self::with_driver(config, Arc::new(DockerDriver::default()))
    }

    /// Host-injected backend; primarily used with deterministic fake drivers
    /// for unit tests. Only trusted hosts may supply an execution driver.
    pub fn with_driver(config: SandboxConfig, driver: Arc<dyn ContainerDriver>) -> Self {
        Self { config, driver, receipts: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    pub fn into_parts(self) -> (SandboxWorker, SandboxVerifier) {
        let worker = SandboxWorker {
            config: self.config.clone(),
            driver: self.driver,
            receipts: self.receipts.clone(),
        };
        let verifier = SandboxVerifier {
            criterion: self.config.criterion,
            receipts: self.receipts,
        };
        (worker, verifier)
    }
}

pub struct SandboxWorker {
    config: SandboxConfig,
    driver: Arc<dyn ContainerDriver>,
    receipts: ReceiptRegistry,
}

impl Worker for SandboxWorker {
    fn agent_id(&self) -> &str {
        "sandbox-test-worker"
    }

    fn execute(&self, task: &Task) -> WorkerExecution {
        if task.status != TaskStatus::Running
            || task.acceptance_criteria != vec![self.config.criterion.clone()]
            || task.required_capabilities.is_empty()
            || task.required_capabilities.iter().any(|cap| cap != "test")
        {
            return WorkerExecution::Blocked {
                reason: "sandbox test worker only accepts running test-only tasks with its configured criterion".into(),
            };
        }

        let output = match self.driver.run(&SandboxPlan::from_config(&self.config)) {
            Ok(output) => output,
            Err(reason) => return WorkerExecution::Failed { reason },
        };
        if output.exit_code != 0 {
            return WorkerExecution::Failed {
                reason: format!("sandbox command returned exit code {}", output.exit_code),
            };
        }
        if (output.stdout.len() as u64).saturating_add(output.stderr.len() as u64) > OUTPUT_LIMIT {
            return WorkerExecution::Failed { reason: "sandbox output limit exceeded".into() };
        }
        let id = Uuid::new_v4();
        let artifact_ref = format!("nulang-sandbox://{id}");
        let path = self.config.receipt_dir.join(format!("{id}.log"));
        let transcript = format!("stdout:\n{}\nstderr:\n{}", output.stdout, output.stderr);
        let digest: [u8; 32] = Sha256::digest(transcript.as_bytes()).into();
        let write_result = (|| -> Result<(), String> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&path).map_err(|e| e.to_string())?;
            f.write_all(transcript.as_bytes()).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
            Ok(())
        })();
        if let Err(reason) = write_result {
            let _ = fs::remove_file(path);
            return WorkerExecution::Failed { reason };
        }
        let receipt = Receipt {
            task_id: task.id,
            criterion: self.config.criterion.clone(),
            log_file: path,
            digest,
        };
        match self.receipts.lock() {
            Ok(mut registry) => {
                registry.insert(artifact_ref.clone(), receipt);
            }
            Err(_) => return WorkerExecution::Failed {
                reason: "receipt registry lock poisoned".into(),
            },
        }

        WorkerExecution::Executed {
            evidence: vec![AcceptanceEvidence {
                criterion: self.config.criterion.clone(),
                artifact_ref,
            }],
        }
    }
}

pub struct SandboxVerifier {
    criterion: String,
    receipts: ReceiptRegistry,
}

impl EvidenceVerifier for SandboxVerifier {
    fn verify(&self, task: &Task, evidence: &[AcceptanceEvidence]) -> Result<(), String> {
        if evidence.len() != 1 || evidence[0].criterion != self.criterion {
            return Err("sandbox verifier requires its exact acceptance criterion".into());
        }
        let registry = self.receipts.lock().map_err(|_| "receipt registry lock poisoned")?;
        let receipt = registry
            .get(&evidence[0].artifact_ref)
            .ok_or("unknown sandbox receipt")?;
        if receipt.task_id != task.id || receipt.criterion != evidence[0].criterion {
            return Err("sandbox receipt belongs to another task or criterion".into());
        }
        let contents = fs::read(&receipt.log_file).map_err(|e| e.to_string())?;
        let digest: [u8; 32] = Sha256::digest(&contents).into();
        if receipt.digest != digest {
            return Err("sandbox receipt transcript changed after execution".into());
        }
        Ok(())
    }
}
