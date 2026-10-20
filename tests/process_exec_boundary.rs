//! Contract tests for the trusted-host, deny-by-default process boundary.
//! These exercise actual child processes, not mock Command builders.
//! None of the APIs here are wired into actor-backed effects.

#![cfg(unix)]

use nulang::process_exec::{execute, ExecError, ExecLimits, ProcessPolicy, ProcessSpec};
use std::time::Duration;

fn shell(script: &str) -> ProcessSpec {
    ProcessSpec::new(
        "/bin/sh",
        std::env::current_dir().expect("working directory"),
        ["-c", script],
    )
    .expect("valid absolute executable and cwd")
}

#[test]
fn denies_commands_without_an_exact_grant() {
    let allowed = shell("printf permitted");
    let different_args = shell("printf not-permitted");
    let mut policy = ProcessPolicy::new();
    policy.allow_exact(allowed.clone());

    assert!(matches!(
        execute(&policy, &different_args, ExecLimits::default()),
        Err(ExecError::Denied)
    ));

    let output = execute(&policy, &allowed, ExecLimits::default()).unwrap();
    assert_eq!(output.stdout, b"permitted");
    assert!(output.status.success());
}

#[test]
fn stdout_stderr_and_nonzero_exit_are_distinct() {
    let command = shell("printf out; printf err >&2; exit 7");
    let mut policy = ProcessPolicy::new();
    policy.allow_exact(command.clone());

    let output = execute(&policy, &command, ExecLimits::default()).unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"out");
    assert_eq!(output.stderr, b"err");
}

#[test]
fn does_not_inherit_host_environment_by_default() {
    let command = shell("printf '%s' \"$HOME\"");
    let mut policy = ProcessPolicy::new();
    policy.allow_exact(command.clone());

    let output = execute(&policy, &command, ExecLimits::default()).unwrap();
    assert_eq!(output.stdout, b"");
}

#[test]
fn argument_vector_does_not_interpolate_metacharacters() {
    let literal = "$(printf pwned); && | \"quoted\"";
    let command = ProcessSpec::new(
        "/bin/sh",
        std::env::current_dir().unwrap(),
        ["-c", "printf '%s' \"$1\"", "literal-arg", literal],
    )
    .unwrap();
    let mut policy = ProcessPolicy::new();
    policy.allow_exact(command.clone());

    let output = execute(&policy, &command, ExecLimits::default()).unwrap();
    assert_eq!(output.stdout, literal.as_bytes());
}

#[test]
fn rejects_output_larger_than_the_capture_budget() {
    let command = shell("printf 123456789");
    let mut policy = ProcessPolicy::new();
    policy.allow_exact(command.clone());

    assert!(matches!(
        execute(
            &policy,
            &command,
            ExecLimits::new(Duration::from_secs(3), 4).unwrap()
        ),
        Err(ExecError::OutputLimitExceeded)
    ));
}

#[test]
fn counts_stdout_and_stderr_against_one_shared_budget() {
    let command = shell("printf abcd; printf efgh >&2");
    let mut policy = ProcessPolicy::new();
    policy.allow_exact(command.clone());

    assert!(matches!(
        execute(
            &policy,
            &command,
            ExecLimits::new(Duration::from_secs(3), 6).unwrap()
        ),
        Err(ExecError::OutputLimitExceeded)
    ));
}

#[test]
fn terminates_an_overlong_command() {
    let command = shell("sleep 2");
    let mut policy = ProcessPolicy::new();
    policy.allow_exact(command.clone());

    assert!(matches!(
        execute(
            &policy,
            &command,
            ExecLimits::new(Duration::from_millis(50), 1024).unwrap()
        ),
        Err(ExecError::TimedOut)
    ));
}

#[test]
fn rejects_relative_executables_and_zero_limits() {
    assert!(matches!(
        ProcessSpec::new("echo", "/tmp", ["hello"]),
        Err(ExecError::InvalidSpec(_))
    ));
    assert!(matches!(
        ExecLimits::new(Duration::ZERO, 1024),
        Err(ExecError::InvalidLimits)
    ));
    assert!(matches!(
        ExecLimits::new(Duration::from_secs(1), 0),
        Err(ExecError::InvalidLimits)
    ));
}
