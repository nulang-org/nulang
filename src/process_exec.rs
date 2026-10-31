//! Bounded, direct (no implicit shell) execution for trusted standalone scripts.

use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

/// A hard ceiling prevents a script from asking the host to buffer unlimited output.
pub const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
pub struct ExecOptions {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

/// Trusted-host process context. It does not constrain filesystem or secret access.
#[derive(Debug, Default, Clone)]
pub struct ExecContext {
    /// Child working directory; None inherits the host's directory.
    pub cwd: Option<PathBuf>,
    /// Child-only environment overrides; other environment variables are inherited.
    pub env: Vec<(String, String)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ExecOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExecError {
    InvalidOptions,
    Spawn(String),
    Wait(String),
    TimedOut,
    OutputLimitExceeded,
}

/// Execute one process directly, without a shell. Only call from a trusted host
/// context: argv parsing is not an OS sandbox or an authorization decision.
///
/// Both pipes are drained concurrently; the combined captured output is bounded.
/// An exceeded limit or deadline terminates and reaps the direct child.
pub fn exec(program: &str, args: &[String], options: ExecOptions) -> Result<ExecOutput, ExecError> {
    exec_in(program, args, options, &ExecContext::default())
}

/// Execute with a child-specific working directory and environment overrides.
/// This operation does NOT isolate the child's filesystem or environment.
pub fn exec_in(
    program: &str,
    args: &[String],
    options: ExecOptions,
    context: &ExecContext,
) -> Result<ExecOutput, ExecError> {
    if program.is_empty()
        || options.timeout.is_zero()
        || options.timeout > MAX_TIMEOUT
        || options.max_output_bytes == 0
        || options.max_output_bytes > MAX_CAPTURE_BYTES
    {
        return Err(ExecError::InvalidOptions);
    }

    if context.env.len() > 64
        || context.env.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > 256
                || key.contains(['=', '\0'])
                || value.len() > 8192
                || value.contains('\0')
        })
        || context
            .cwd
            .as_ref()
            .is_some_and(|dir| dir.as_os_str().is_empty())
    {
        return Err(ExecError::InvalidOptions);
    }

    let mut command = Command::new(program);
    if let Some(cwd) = &context.cwd {
        command.current_dir(cwd);
    }
    command.envs(context.env.iter().map(|(key, value)| (key, value)));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|err| ExecError::Spawn(err.to_string()))?;

    let total = Arc::new(AtomicUsize::new(0));
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = read_bounded(
        child.stdout.take().expect("stdout was piped"),
        options.max_output_bytes,
        Arc::clone(&total),
        Arc::clone(&overflow),
    );
    let stderr = read_bounded(
        child.stderr.take().expect("stderr was piped"),
        options.max_output_bytes,
        total,
        Arc::clone(&overflow),
    );

    let started = Instant::now();
    let mut terminated = None;
    let status = loop {
        if overflow.load(Ordering::Acquire) {
            terminated = Some(ExecError::OutputLimitExceeded);
            break None;
        }
        if started.elapsed() >= options.timeout {
            terminated = Some(ExecError::TimedOut);
            break None;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(err) => {
                terminated = Some(ExecError::Wait(err.to_string()));
                break None;
            }
        }
    };
    // A completed direct child may leave background descendants holding its
    // pipe descriptors open. The deadline must include draining both pipes.
    while terminated.is_none() && (!stdout.is_finished() || !stderr.is_finished()) {
        if overflow.load(Ordering::Acquire) {
            terminated = Some(ExecError::OutputLimitExceeded);
        } else if started.elapsed() >= options.timeout {
            terminated = Some(ExecError::TimedOut);
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
    if terminated.is_some() {
        // Each Unix process owns a fresh process group, so terminate children
        // that inherited stdout/stderr along with the direct subprocess.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = stdout
        .join()
        .map_err(|_| ExecError::Wait("stdout reader panicked".into()))?
        .map_err(|e| ExecError::Wait(e.to_string()))?;
    let stderr = stderr
        .join()
        .map_err(|_| ExecError::Wait("stderr reader panicked".into()))?
        .map_err(|e| ExecError::Wait(e.to_string()))?;
    if let Some(error) = terminated {
        return Err(error);
    }
    if overflow.load(Ordering::Acquire) {
        return Err(ExecError::OutputLimitExceeded);
    }
    Ok(ExecOutput {
        exit_code: status.and_then(|s| s.code()),
        stdout,
        stderr,
    })
}

fn read_bounded<R: Read + Send + 'static>(
    mut stream: R,
    max_bytes: usize,
    total: Arc<AtomicUsize>,
    overflow: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let count = stream.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let previous = total.fetch_add(count, Ordering::AcqRel);
            let allowed = max_bytes.saturating_sub(previous).min(count);
            captured.extend_from_slice(&chunk[..allowed]);
            if allowed < count {
                overflow.store(true, Ordering::Release);
                break;
            }
        }
        Ok(captured)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ExecOptions {
        ExecOptions {
            timeout: Duration::from_secs(2),
            max_output_bytes: 4096,
        }
    }

    #[cfg(unix)]
    #[test]
    fn argv_is_not_interpreted_as_shell() {
        let args = ["%s", "hello world; echo injected", "$(echo secret)"].map(str::to_owned);
        let result = exec("printf", &args, opts()).unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout, b"hello world; echo injected$(echo secret)");
        assert!(result.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_retains_stderr_and_status() {
        let args = ["-c", "printf failure >&2; exit 7"].map(str::to_owned);
        let result = exec("sh", &args, opts()).unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert_eq!(result.stderr, b"failure");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_direct_child() {
        let args = ["-c", "exec sleep 2"].map(str::to_owned);
        let error = exec(
            "sh",
            &args,
            ExecOptions {
                timeout: Duration::from_millis(50),
                ..opts()
            },
        )
        .unwrap_err();
        assert_eq!(error, ExecError::TimedOut);
    }

    #[cfg(unix)]
    #[test]
    fn inherited_pipe_fds_cannot_hold_capture_open_past_timeout() {
        // The shell exits successfully, but its background child inherits the
        // stdout/stderr descriptors. Reading to EOF must respect our deadline.
        let args = ["-c", "sleep 1 & exit 0"].map(str::to_owned);
        let started = Instant::now();
        let err = exec(
            "sh",
            &args,
            ExecOptions {
                timeout: Duration::from_millis(50),
                ..opts()
            },
        )
        .unwrap_err();
        assert_eq!(err, ExecError::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unbounded_output() {
        let args = ["-c", "while :; do printf 1234567890; done"].map(str::to_owned);
        let error = exec(
            "sh",
            &args,
            ExecOptions {
                max_output_bytes: 128,
                ..opts()
            },
        )
        .unwrap_err();
        assert_eq!(error, ExecError::OutputLimitExceeded);
    }

    #[cfg(unix)]
    #[test]
    fn process_context_sets_cwd_without_changing_parent() {
        let home = std::env::current_dir().unwrap();
        let context = ExecContext {
            cwd: Some(std::env::temp_dir()),
            env: vec![],
        };
        let result = exec_in("pwd", &[], opts(), &context).unwrap();
        assert_eq!(
            String::from_utf8(result.stdout).unwrap().trim(),
            std::fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .to_string_lossy()
        );
        assert_eq!(std::env::current_dir().unwrap(), home);
    }

    #[cfg(unix)]
    #[test]
    fn process_context_sets_env_without_polluting_parent() {
        let key = "NULANG_PROCESS_EXEC_CONTEXT_REGRESSION";
        let context = ExecContext {
            cwd: None,
            env: vec![(key.to_string(), "literal $(do_not_expand)".into())],
        };
        let result = exec_in("printenv", &[key.to_string()], opts(), &context).unwrap();
        assert_eq!(result.stdout, b"literal $(do_not_expand)\n");
        assert!(std::env::var_os(key).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn process_context_rejects_invalid_env_and_cwd() {
        for name in ["", "A=B", "a\0b"] {
            let context = ExecContext {
                cwd: None,
                env: vec![(name.into(), "ok".into())],
            };
            assert_eq!(
                exec_in("true", &[], opts(), &context),
                Err(ExecError::InvalidOptions)
            );
        }
        let context = ExecContext {
            cwd: None,
            env: vec![("GOOD".into(), "nul\0bad".into())],
        };
        assert_eq!(
            exec_in("true", &[], opts(), &context),
            Err(ExecError::InvalidOptions)
        );
        let context = ExecContext {
            cwd: Some(std::env::temp_dir().join("nulang-missing-dir-67161914")),
            env: vec![],
        };
        assert!(matches!(
            exec_in("true", &[], opts(), &context),
            Err(ExecError::Spawn(_))
        ));
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        let err = exec("nulang-command-that-does-not-exist-17383", &[], opts()).unwrap_err();
        assert!(matches!(err, ExecError::Spawn(_)));
    }

    #[test]
    fn invalid_options_fail_before_execution() {
        let err = exec(
            "printf",
            &[],
            ExecOptions {
                timeout: Duration::ZERO,
                ..opts()
            },
        )
        .unwrap_err();
        assert_eq!(err, ExecError::InvalidOptions);
    }
}
