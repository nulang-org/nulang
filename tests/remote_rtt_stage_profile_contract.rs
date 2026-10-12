const PROFILE: &str = include_str!("remote_rtt_stage_profile.rs");
const WORKFLOW: &str = include_str!("../.github/workflows/remote-rtt-stage-profile-pr.yml");

#[test]
fn remote_rtt_profile_reports_poll_stage_budget() {
    for required in [
        "REMOTE_RTT_STAGE_PROFILE_START",
        "process_network_ns",
        "run_scheduler_ns",
        "polls",
        "yield_count",
        "send_ns",
        "Runtime::new()",
        "enable_distribution",
        "send_distributed",
    ] {
        assert!(
            PROFILE.contains(required),
            "remote RTT profile must contain {required:?}"
        );
    }
}

#[test]
fn remote_rtt_workflow_uses_comparative_feature_profile() {
    let release_step = WORKFLOW
        .split("- name: Release stage budget")
        .nth(1)
        .expect("remote RTT workflow must contain release stage budget step");

    for required in ["--no-default-features", "--features tcp"] {
        assert!(
            release_step.contains(required),
            "remote RTT release step must preserve comparative feature profile token {required:?}"
        );
    }
}
