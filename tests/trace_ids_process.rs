//! Cross-process uniqueness regression for W3C trace root IDs.
//!
//! Each invocation of the helper runs in a fresh test executable process:
//! a process-local incrementing counter with a fixed seed will collide.

use std::process::Command;

use nulang::runtime::TraceContext;

const CHILD_FLAG: &str = "NULANG_TRACE_ROOT_TEST_CHILD";
const PREFIX: &str = "TRACE_ROOT_ID=";

#[test]
fn trace_root_child_process_helper() {
    if std::env::var_os(CHILD_FLAG).is_some() {
        println!("{PREFIX}{:032x}", TraceContext::root().trace_id());
    }
}

fn root_id_from_fresh_process() -> String {
    let output = Command::new(std::env::current_exe().expect("test executable path"))
        .args([
            "--exact",
            "trace_root_child_process_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_FLAG, "1")
        .output()
        .expect("spawn isolated trace-root test process");

    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout)
        .expect("child stdout UTF-8")
        .lines()
        .find_map(|line| line.split_once(PREFIX).map(|(_, id)| id.trim().to_owned()))
        .expect("child must print a trace root ID")
}

#[test]
fn trace_root_ids_differ_across_independent_processes() {
    let first = root_id_from_fresh_process();
    let second = root_id_from_fresh_process();
    assert_ne!(
        first, second,
        "identical first trace IDs in independent processes break W3C trace correlation"
    );
}
