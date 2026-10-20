//! An explicit, bounded process boundary for trusted Nulang embedders.
//!
//! This is deliberately NOT an actor effect. Calling the synchronous host
//! interface from a cooperative actor turn would block its scheduler, and
//! permission to spawn a process does not establish OS sandboxing. Hosts must
//! grant an exact executable/argument/cwd tuple before executing it.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// A command identity. Argument vectors are passed literally, never via a shell.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessSpec {
    executable: PathBuf,
    cwd: PathBuf,
    args: Vec<OsString>,
}

impl ProcessSpec {
    /// Both paths must be absolute. This avoids PATH resolution and avoids
    /// interpreting working directories relative to a changing parent cwd.
    pub fn new<I, S>(
        executable: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
        args: I,
    ) -> Result<Self, ExecError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let executable = executable.into();
        let cwd = cwd.into();
        if !executable.is_absolute() {
            return Err(ExecError::InvalidSpec("executable must be an absolute path"));
        }
        if !cwd.is_absolute() {
            return Err(ExecError::InvalidSpec("cwd must be an absolute path"));
        }
        Ok(Self {
            executable,
            cwd,
            args: args.into_iter().map(Into::into).collect(),
        })
    }
}

/// A host-supplied allowlist, empty by default. This is not a sandbox:
/// a command granted here can still access OS resources available to the host.
#[derive(Debug, Default, Clone)]
pub struct ProcessPolicy {
    allowed: BTreeSet<ProcessSpec>,
}

impl ProcessPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Only trusted host/bootstrap code should add grants. Every argument and
    /// the working directory are compared exactly at the invocation boundary.
    pub fn allow_exact(&mut self, spec: ProcessSpec) {
        self.allowed.insert(spec);
    }

    pub fn allows(&self, spec: &ProcessSpec) -> bool {
        self.allowed.contains(spec)
    }
}

/// These limits are mandatory. stdout and stderr share one capture budget.
#[derive(Debug, Clone, Copy)]
pub struct ExecLimits {
    timeout: Duration,
    max_output_bytes: usize,
}

impl ExecLimits {
    pub fn new(timeout: Duration, max_output_bytes: usize) -> Result<Self, ExecError> {
        if timeout.is_zero() || max_output_bytes == 0 || max_output_bytes > 64 * 1024 * 1024 {
            return Err(ExecError::InvalidLimits);
        }
        Ok(Self {
            timeout,
            max_output_bytes,
        })
    }
}

impl Default for ExecLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_output_bytes: 1024 * 1024,
        }
    }
}

/// Nonzero exit is a normal completed outcome, not a transport error.
#[derive(Debug)]
pub struct ExecOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum ExecError {
    Denied,
    InvalidSpec(&'static str),
    InvalidLimits,
    Spawn(io::Error),
    Poll(io::Error),
    Capture(io::Error),
    TimedOut,
    OutputLimitExceeded,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => write!(f, "process command not authorized"),
            Self::InvalidSpec(reason) => write!(f, "invalid process specification: {reason}"),
            Self::InvalidLimits => write!(f, "invalid process execution limits"),
            Self::Spawn(e) => write!(f, "cannot start process: {e}"),
            Self::Poll(e) => write!(f, "cannot await process: {e}"),
            Self::Capture(e) => write!(f, "cannot capture process output: {e}"),
            Self::TimedOut => write!(f, "process exceeded its deadline"),
            Self::OutputLimitExceeded => write!(f, "process output exceeded capture limit"),
        }
    }
}
impl std::error::Error for ExecError {}

enum PipeResult {
    Stdout(io::Result<Vec<u8>>),
    Stderr(io::Result<Vec<u8>>),
}

fn read_bounded(mut pipe: impl Read, max: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    // The extra byte distinguishes exactly-at-limit from overflowing output.
    pipe.by_ref()
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Stop a timed-out/overflowing command and reap the direct child.
/// On Unix, a dedicated process group lets us terminate descendants while
/// the group leader is still alive. This is best effort, not a sandbox.
fn stop_process(child: &mut Child, leader_running: bool) {
    #[cfg(unix)]
    if leader_running {
        if let Ok(pid) = i32::try_from(child.id()) {
            if pid > 0 {
                // The child was started in a new process group by execute.
                // SAFETY: negative pid targets that child's process group.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        }
    }
    #[cfg(not(unix))]
    let _ = leader_running;
    let _ = child.kill();
    let _ = child.wait();
}

/// Run a trusted command with exact host authority and bounded captured I/O.
///
/// argv is literal; there is no implicit sh -c, inherited environment, stdin
/// forwarding, or unbounded output buffer. This synchronous helper must not
/// be called on a cooperative actor scheduler thread. It does not safely
/// replay external side effects, and does not isolate an untrusted program.
pub fn execute(
    policy: &ProcessPolicy,
    spec: &ProcessSpec,
    limits: ExecLimits,
) -> Result<ExecOutput, ExecError> {
    // Check before spawning or inspecting executable metadata.
    if !policy.allows(spec) {
        return Err(ExecError::Denied);
    }
    if limits.timeout.is_zero()
        || limits.max_output_bytes == 0
        || limits.max_output_bytes > 64 * 1024 * 1024
    {
        return Err(ExecError::InvalidLimits);
    }

    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(ExecError::Spawn)?;
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");

    let (sender, receiver) = mpsc::channel();
    let output_limit = limits.max_output_bytes;
    let sender_err = sender.clone();
    thread::spawn(move || {
        let _ = sender.send(PipeResult::Stdout(read_bounded(stdout_pipe, output_limit)));
    });
    thread::spawn(move || {
        let _ = sender_err.send(PipeResult::Stderr(read_bounded(stderr_pipe, output_limit)));
    });

    let deadline = Instant::now() + limits.timeout;
    let mut stdout = None;
    let mut stderr = None;
    let mut status: Option<ExitStatus> = None;

    while status.is_none() || stdout.is_none() || stderr.is_none() {
        if Instant::now() >= deadline {
            stop_process(&mut child, status.is_none());
            return Err(ExecError::TimedOut);
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(done) => status = done,
                Err(error) => {
                    stop_process(&mut child, true);
                    return Err(ExecError::Poll(error));
                }
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            continue;
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(10))) {
            Ok(message) => {
                let (is_stdout, result) = match message {
                    PipeResult::Stdout(result) => (true, result),
                    PipeResult::Stderr(result) => (false, result),
                };
                let bytes = match result {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        stop_process(&mut child, status.is_none());
                        return Err(ExecError::Capture(error));
                    }
                };
                let other_bytes = stdout.as_ref().map_or(0, Vec::len)
                    + stderr.as_ref().map_or(0, Vec::len);
                if bytes.len().saturating_add(other_bytes) > limits.max_output_bytes {
                    stop_process(&mut child, status.is_none());
                    return Err(ExecError::OutputLimitExceeded);
                }
                if is_stdout {
                    stdout = Some(bytes);
                } else {
                    stderr = Some(bytes);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                stop_process(&mut child, status.is_none());
                return Err(ExecError::Capture(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "process output capture disconnected",
                )));
            }
        }
    }

    Ok(ExecOutput {
        status: status.expect("completed status"),
        stdout: stdout.expect("completed stdout"),
        stderr: stderr.expect("completed stderr"),
    })
}
