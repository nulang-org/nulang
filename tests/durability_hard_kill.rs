use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::{
    DurableEffectCoordinator, DurableEffectDispatchDecision,
};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, JsonFileStore, PersistedValue, PersistenceStore,
    WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};

const ACTOR_ID: u64 = 29;
const EFFECT_ACTOR_ID: u64 = 31;
const CHILD_ENV: &str = "NU_DURABILITY_HARD_KILL_CHILD";
const STORE_ENV: &str = "NU_DURABILITY_HARD_KILL_STORE";

fn transition(sequence: u64, expected_previous_sequence: u64) -> DurableTransition {
    let mut state = HashMap::new();
    state.insert(
        "acknowledged_sequence".to_string(),
        PersistedValue::Int(sequence as i64),
    );

    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence,
        expected_previous_sequence,
        command: None,
        snapshot: Some(ActorSnapshot {
            actor_id: ACTOR_ID,
            sequence,
            state,
            waiting_signal: None,
            crdt_snapshot: None,
            crdt_field_map: None,
            authority_tokens: Default::default(),
        }),
        workflow_events: vec![WorkflowEvent::Custom {
            sequence,
            name: "acknowledged".to_string(),
            args: vec![PersistedValue::Int(sequence as i64)],
        }],
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    }
}

fn child_writer() {
    let store_dir = std::env::var_os(STORE_ENV).expect("child store path must be provided");
    let mut store = JsonFileStore::new(store_dir).expect("child must open JSON durable store");

    let commit = store
        .commit_transition(transition(1, 0))
        .expect("acknowledged transition must commit");

    assert_eq!(commit.sequence, 1);
    println!("NU_DURABILITY_ACK {}", commit.sequence);
    std::io::stdout()
        .flush()
        .expect("child must flush acknowledgement");

    // The parent deliberately terminates us after observing the ACK. Staying
    // alive here ensures a successful test always exercises abrupt process
    // termination rather than a normal child exit.
    loop {
        std::thread::park();
    }
}

fn effect_spec() -> DurableEffectSpec {
    let site = effect_site_id(
        "durability-hard-kill",
        EffectSiteOwnerKind::Behavior,
        "HardKillActor.run",
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

fn effect_child_writer() {
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
    assert_eq!(
        coordinator
            .complete(effect_id, b"request", b"provider-result".to_vec())
            .unwrap(),
        b"provider-result"
    );

    println!("NU_EFFECT_ACK 2");
    std::io::stdout()
        .flush()
        .expect("child must flush effect acknowledgement");

    loop {
        std::thread::park();
    }
}

fn hard_kill_after_ack(
    test_name: &str,
    child_mode: &str,
    ack_prefix: &str,
    store_dir: &std::path::Path,
) -> String {
    let current_test_binary = std::env::current_exe().expect("test binary path must be available");
    let mut child = Command::new(current_test_binary)
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(CHILD_ENV, child_mode)
        .env(STORE_ENV, store_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("durability child process must start");

    let stdout = child
        .stdout
        .take()
        .expect("durability child stdout must be piped");
    let mut reader = BufReader::new(stdout);
    let mut acknowledgement = None;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .expect("parent must read child acknowledgement");
        if bytes == 0 {
            break;
        }
        if let Some(value) = line.trim().strip_prefix(ack_prefix) {
            acknowledgement = Some(value.trim().to_owned());
            break;
        }
    }

    let acknowledgement =
        acknowledgement.expect("child exited before acknowledging its durable commit");

    child
        .kill()
        .expect("parent must be able to hard-kill durability child");
    let status = child.wait().expect("parent must reap durability child");
    assert!(
        !status.success(),
        "durability child must terminate abruptly rather than exit normally"
    );

    acknowledgement
}

fn fresh_dir() -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after UNIX epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "nulang-hard-kill-{}-{unique}",
        std::process::id()
    ))
}

#[test]
fn acknowledged_json_atomic_transition_survives_immediate_hard_kill() {
    if std::env::var(CHILD_ENV).ok().as_deref() == Some("transition") {
        child_writer();
        return;
    }

    let store_dir = fresh_dir();
    let acknowledged_sequence = hard_kill_after_ack(
        "acknowledged_json_atomic_transition_survives_immediate_hard_kill",
        "transition",
        "NU_DURABILITY_ACK ",
        &store_dir,
    )
    .parse::<u64>()
    .expect("transition acknowledgement must contain a sequence");
    assert_eq!(acknowledged_sequence, 1);

    let reopened = JsonFileStore::new(&store_dir).expect("parent must reopen JSON durable store");

    let tail = reopened
        .load_durable_tail(ACTOR_ID)
        .expect("durable tail recovery must not fail")
        .expect("acknowledged durable tail must survive hard kill");
    assert_eq!(tail.activation_epoch, 1);
    assert_eq!(tail.sequence, 1);
    assert_eq!(reopened.latest_sequence(ACTOR_ID), 1);

    let snapshot = reopened
        .load_snapshot(ACTOR_ID)
        .expect("acknowledged snapshot must survive hard kill");
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(
        snapshot.state.get("acknowledged_sequence"),
        Some(&PersistedValue::Int(1))
    );

    let events = reopened.read_workflow_events(ACTOR_ID);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        WorkflowEvent::Custom {
            sequence: 1,
            name,
            args
        } if name == "acknowledged" && args == &vec![PersistedValue::Int(1)]
    ));

    std::fs::remove_dir_all(&store_dir).expect("test store cleanup must succeed");
}

#[test]
fn completed_durable_effect_receipt_survives_hard_kill_and_replays_without_dispatch() {
    if std::env::var(CHILD_ENV).ok().as_deref() == Some("effect") {
        effect_child_writer();
        return;
    }

    let store_dir = fresh_dir();
    let acknowledged_sequence = hard_kill_after_ack(
        "completed_durable_effect_receipt_survives_hard_kill_and_replays_without_dispatch",
        "effect",
        "NU_EFFECT_ACK ",
        &store_dir,
    )
    .parse::<u64>()
    .expect("effect acknowledgement must contain the durable sequence");
    assert_eq!(acknowledged_sequence, 2);

    let mut reopened =
        JsonFileStore::new(&store_dir).expect("parent must reopen JSON durable-effect store");
    assert_eq!(reopened.latest_sequence(EFFECT_ACTOR_ID), 2);

    let spec = effect_spec();
    let mut coordinator = DurableEffectCoordinator::new(&mut reopened, EFFECT_ACTOR_ID, 1);
    assert_eq!(
        coordinator.begin(spec, b"request").unwrap(),
        DurableEffectDispatchDecision::ReplayRecordedResult(b"provider-result".to_vec()),
        "a completed durable effect must replay its committed receipt rather than request provider dispatch"
    );
    assert_eq!(
        reopened.latest_sequence(EFFECT_ACTOR_ID),
        2,
        "replay must not append another effect transition"
    );

    std::fs::remove_dir_all(&store_dir).expect("effect test store cleanup must succeed");
}
