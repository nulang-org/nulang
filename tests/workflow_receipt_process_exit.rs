#![cfg(feature = "sqlite")]

//! Abrupt process exit at committed workflow/effect boundaries. This checks
//! process-local state loss, not OS power loss or a crash during a disk write.
use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::{DurableEffectCoordinator, DurableEffectDispatchDecision};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    DurableTransition, JournalEntry, LibsqlStore, Runtime, WorkflowActivationId,
    DURABLE_TRANSITION_VERSION,
};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const CHILD_DIR: &str = "NULANG_WORKFLOW_RECEIPT_CHILD_DIR";
const CHILD_PHASE: &str = "NULANG_WORKFLOW_RECEIPT_CHILD_PHASE";
const EXIT_CUTPOINT: i32 = 73;
const REQUEST: &[u8] = b"stable-prompt";
const RECEIPT: &[u8] = b"committed-provider-receipt";

struct TestDir(PathBuf);
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn spec(activation: WorkflowActivationId) -> DurableEffectSpec {
    DurableEffectSpec::new(
        DurableEffectId::derive(
            activation.actor_id,
            &format!("workflow-activation:{}", activation.command_sequence),
            0,
            "Inference.ask",
        ),
        "Inference.ask",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    )
}

#[test]
fn child_workflow_cutpoint() {
    let Some(dir) = std::env::var_os(CHILD_DIR).map(PathBuf::from) else {
        return;
    };
    let phase = std::env::var(CHILD_PHASE).unwrap();
    let mut rt = Runtime::new();
    rt.persistence = Box::new(LibsqlStore::new(&dir.join("workflow.db")).unwrap());
    let actor_id = rt
        .try_spawn_workflow_actor("ReceiptCrash", Box::new(Vec::new), HashMap::new())
        .unwrap();
    let epoch = rt.actors[&actor_id].activation_epoch;
    let previous = rt.persistence.latest_sequence(actor_id);
    let sequence = previous + 1;
    rt.persistence
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: epoch,
            sequence,
            expected_previous_sequence: previous,
            command: Some(JournalEntry {
                sequence,
                behavior_id: 0,
                payload: vec![],
            }),
            snapshot: None,
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .unwrap();
    let activation = WorkflowActivationId::new(actor_id, sequence);
    let spec = spec(activation);
    let mut coordinator = DurableEffectCoordinator::new(rt.persistence.as_mut(), actor_id, epoch);
    assert_eq!(
        coordinator.begin(spec.clone(), REQUEST).unwrap(),
        DurableEffectDispatchDecision::DispatchWithDeduplication {
            operation_id: spec.id
        }
    );
    match phase.as_str() {
        "prepared" => {}
        "completed" => {
            assert_eq!(
                coordinator
                    .complete(spec.id, REQUEST, RECEIPT.to_vec())
                    .unwrap(),
                RECEIPT
            );
        }
        _ => panic!("unexpected cutpoint"),
    }
    fs::write(dir.join("identity"), format!("{actor_id} {sequence}")).unwrap();
    // No Rust destructors run, including Runtime and libSQL store cleanup.
    std::process::exit(EXIT_CUTPOINT);
}

#[test]
fn unfinished_workflow_and_effect_identity_survive_abrupt_exit_and_repeated_reopens() {
    for phase in ["prepared", "completed"] {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = TestDir(std::env::temp_dir().join(format!(
            "nulang-workflow-receipt-{phase}-{}-{nonce}",
            std::process::id()
        )));
        fs::create_dir(&dir.0).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_workflow_cutpoint", "--nocapture"])
            .env(CHILD_DIR, &dir.0)
            .env(CHILD_PHASE, phase)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(EXIT_CUTPOINT),
            "child failed before cutpoint: {} {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        let identity = fs::read_to_string(dir.0.join("identity")).unwrap();
        let mut fields = identity
            .split_whitespace()
            .map(|v| v.parse::<u64>().unwrap());
        let actor_id = fields.next().unwrap();
        let activation = WorkflowActivationId::new(actor_id, fields.next().unwrap());
        let spec = spec(activation);
        for reopen in 0..3 {
            let mut rt = Runtime::new();
            rt.persistence = Box::new(LibsqlStore::new(&dir.0.join("workflow.db")).unwrap());
            assert_eq!(rt.recover_actor(actor_id), Some(actor_id));
            assert_eq!(
                rt.actors[&actor_id].current_workflow_activation,
                Some(activation)
            );
            assert_eq!(rt.actors[&actor_id].mailbox.len(), 1);
            let before = rt.persistence.latest_sequence(actor_id);
            let epoch = rt.actors[&actor_id].activation_epoch;
            let mut coordinator =
                DurableEffectCoordinator::new(rt.persistence.as_mut(), actor_id, epoch);
            if phase == "prepared" && reopen == 0 {
                assert_eq!(
                    coordinator.begin(spec.clone(), REQUEST).unwrap(),
                    DurableEffectDispatchDecision::DispatchWithDeduplication {
                        operation_id: spec.id
                    }
                );
                assert_eq!(
                    coordinator
                        .complete(spec.id, REQUEST, RECEIPT.to_vec())
                        .unwrap(),
                    RECEIPT
                );
                assert_eq!(rt.persistence.latest_sequence(actor_id), before + 1);
            } else {
                assert_eq!(
                    coordinator.begin(spec.clone(), REQUEST).unwrap(),
                    DurableEffectDispatchDecision::ReplayRecordedResult(RECEIPT.to_vec())
                );
                assert_eq!(rt.persistence.latest_sequence(actor_id), before);
            }
        }
    }
}
