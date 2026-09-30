use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::{DurableEffectCoordinator, DurableEffectDispatchDecision};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, JsonFileStore, PersistedValue, PersistenceStore,
    WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};

const ACTOR_ID: u64 = 61;
const EFFECT_ACTOR_ID: u64 = 62;
const CHILD_ENV: &str = "NU_JSON_DURABILITY_HARD_KILL_CHILD";
const STORE_ENV: &str = "NU_JSON_DURABILITY_HARD_KILL_STORE";

fn transition() -> DurableTransition {
    let mut state = HashMap::new();
    state.insert("committed".to_string(), PersistedValue::Int(1));

    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence: 1,
        expected_previous_sequence: 0,
        command: None,
        snapshot: Some(ActorSnapshot {
            actor_id: ACTOR_ID,
            sequence: 1,
            state,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::Custom {
            sequence: 1,
            name: "committed".to_string(),
            args: vec![PersistedValue::Int(1)],
        }],
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    }
}

fn effect_spec() -> DurableEffectSpec {
    let site = effect_site_id(
        "json-durability-hard-kill",
        EffectSiteOwnerKind::Behavior,
        "JsonHardKill.run",
        "Provider.ask",
        0,
    );
    DurableEffectSpec::new(
        DurableEffectId::derive_from_site(EFFECT_ACTOR_ID, "turn:1", site, 0),
        "Provider.ask",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    )
}

fn transition_child() {
    let store_dir = std::env::var_os(STORE_ENV).expect("child store path must be provided");
    let mut store = JsonFileStore::new(store_dir).expect("child must open JSON durable store");
    let commit = store
        .commit_transition(transition())
        .expect("atomic JSON transition must commit");
    println!("NU_JSON_TRANSITION_ACK {}", commit.sequence);
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

fn effect_child() {
    let store_dir = std::env::var_os(STORE_ENV).expect("child store path must be provided");
    let mut store = JsonFileStore::new(store_dir).expect("child must open JSON durable store");
    let spec = effect_spec();
    let effect_id = spec.id;

    let mut coordinator = DurableEffectCoordinator::new(&mut store, EFFECT_ACTOR_ID, 1);
    assert_eq!(
        coordinator.begin(spec, b"request").unwrap(),
        DurableEffectDispatchDecision::DispatchWithDeduplication {
            operation_id: effect_id
        }
    );
    coordinator
        .complete(effect_id, b"request", b"provider-result".to_vec())
        .unwrap();

    println!("NU_JSON_EFFECT_ACK 2");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

fn kill_after_ack(test_name: &str, mode: &str, prefix: &str, store_dir: &std::path::Path) -> u64 {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD_ENV, mode)
        .env(STORE_ENV, store_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("hard-kill child must start");

    let stdout = child.stdout.take().expect("child stdout must be piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let acknowledged = loop {
        line.clear();
        let read = reader.read_line(&mut line).expect("must read child output");
        assert_ne!(read, 0, "child exited before durable acknowledgement");
        if let Some(value) = line.trim().strip_prefix(prefix) {
            break value
                .trim()
                .parse::<u64>()
                .expect("ACK must carry sequence");
        }
    };

    child.kill().expect("parent must hard-kill child");
    let status = child.wait().expect("parent must reap child");
    assert!(!status.success(), "child must not exit gracefully");
    acknowledged
}

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-json-hard-kill-{name}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn acknowledged_json_atomic_transition_survives_immediate_hard_kill() {
    if std::env::var(CHILD_ENV).ok().as_deref() == Some("transition") {
        transition_child();
        return;
    }

    let dir = fresh_dir("transition");
    assert_eq!(
        kill_after_ack(
            "acknowledged_json_atomic_transition_survives_immediate_hard_kill",
            "transition",
            "NU_JSON_TRANSITION_ACK ",
            &dir,
        ),
        1
    );

    let reopened = JsonFileStore::new(&dir).unwrap();
    assert_eq!(reopened.latest_sequence(ACTOR_ID), 1);
    assert_eq!(
        reopened
            .load_snapshot(ACTOR_ID)
            .unwrap()
            .state
            .get("committed"),
        Some(&PersistedValue::Int(1))
    );
    assert!(matches!(
        reopened.read_workflow_events(ACTOR_ID).as_slice(),
        [WorkflowEvent::Custom {
            sequence: 1,
            name,
            ..
        }] if name == "committed"
    ));

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn completed_json_effect_receipt_survives_hard_kill_without_redispatch() {
    if std::env::var(CHILD_ENV).ok().as_deref() == Some("effect") {
        effect_child();
        return;
    }

    let dir = fresh_dir("effect");
    assert_eq!(
        kill_after_ack(
            "completed_json_effect_receipt_survives_hard_kill_without_redispatch",
            "effect",
            "NU_JSON_EFFECT_ACK ",
            &dir,
        ),
        2
    );

    let mut reopened = JsonFileStore::new(&dir).unwrap();
    assert_eq!(reopened.latest_sequence(EFFECT_ACTOR_ID), 2);
    let before = reopened.latest_sequence(EFFECT_ACTOR_ID);
    let mut coordinator = DurableEffectCoordinator::new(&mut reopened, EFFECT_ACTOR_ID, 1);
    assert_eq!(
        coordinator.begin(effect_spec(), b"request").unwrap(),
        DurableEffectDispatchDecision::ReplayRecordedResult(b"provider-result".to_vec())
    );
    drop(coordinator);
    assert_eq!(
        reopened.latest_sequence(EFFECT_ACTOR_ID),
        before,
        "recovery must replay the committed receipt without appending or redispatching"
    );

    std::fs::remove_dir_all(dir).unwrap();
}
