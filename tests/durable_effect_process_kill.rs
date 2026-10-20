#![cfg(feature = "sqlite")]

/*!
Real OS-process termination and restart gate for the durable-effect coordinator.

This tests the Rust trusted-host coordinator over file-backed SQLite. It does
NOT claim that the ordinary .nula VM PerformAsync callback uses this path.
A child test process commits at a selected crash boundary and parks; the
parent confirms the checkpoint marker and forcibly kills it. A *fresh*
test process reopens SQLite and the idempotent provider's on-disk state.
*/

use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_runtime::{
    DurableEffectCoordinator, DurableEffectDispatchDecision, DurableEffectRuntimeError,
};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{LibsqlStore, PersistenceStore};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ACTOR_ID: u64 = 713;
const REQUEST: &[u8] = b"order=42&cents=100";
const RECEIPT: &[u8] = b"provider-approved-order-42";
const STAGE_ENV: &str = "NULANG_PROCESS_KILL_STAGE";
const ROOT_ENV: &str = "NULANG_PROCESS_KILL_ROOT";
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

fn spec(execution_key: &str) -> DurableEffectSpec {
    let site = effect_site_id(
        "durable-effect-process-kill",
        EffectSiteOwnerKind::Behavior,
        "Checkout.charge",
        "Payment.charge",
        0,
    );
    DurableEffectSpec::new(
        DurableEffectId::derive_from_site(ACTOR_ID, execution_key, site, 0),
        "Payment.charge",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    )
}

fn payment_spec() -> DurableEffectSpec {
    spec("order-42/charge")
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(stage: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "nulang-process-kill-{stage}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create unique fixture directory");
        Self { root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("durable.db")
    }

    fn provider_calls(&self) -> usize {
        fs::read_to_string(self.root.join("provider-attempts"))
            .map(|contents| contents.lines().count())
            .unwrap_or(0)
    }

    fn provider_mutations(&self) -> usize {
        usize::from(self.root.join("provider-mutation").exists())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// File-backed idempotent provider: durable mutation is uniquely keyed by
/// the logical operation ID, and each invocation is recorded independently.
/// This fixture deliberately has no external network and no concurrent calls.
fn provider(root: &Path, operation_id: DurableEffectId, request: &[u8]) -> Vec<u8> {
    let key = operation_id.idempotency_key();
    let mut attempts = OpenOptions::new()
        .append(true)
        .create(true)
        .open(root.join("provider-attempts"))
        .expect("open provider attempts");
    writeln!(attempts, "{key}").expect("record provider attempt");
    attempts.sync_all().expect("sync provider attempts");

    let identity = format!("{key}\n{}\n", String::from_utf8_lossy(request));
    let mutation_path = root.join("provider-mutation");
    match OpenOptions::new().write(true).create_new(true).open(&mutation_path) {
        Ok(mut file) => {
            file.write_all(identity.as_bytes()).expect("write provider mutation");
            file.sync_all().expect("sync provider mutation");
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            assert_eq!(
                fs::read_to_string(&mutation_path).expect("read existing mutation"),
                identity,
                "recovery must reuse the same operation ID and request"
            );
        }
        Err(error) => panic!("provider mutation failed: {error}"),
    }
    RECEIPT.to_vec()
}

fn expected_dispatch(
    decision: DurableEffectDispatchDecision,
    id: DurableEffectId,
) {
    assert_eq!(
        decision,
        DurableEffectDispatchDecision::DispatchWithDeduplication { operation_id: id }
    );
}

/// Worker runs only when invoked explicitly with a stage environment variable.
/// The parent uses the *same test executable* rather than depending on cargo's
/// location or a custom fixture binary in the CI workspace.
#[test]
fn durability_crash_worker() {
    let Ok(stage) = std::env::var(STAGE_ENV) else {
        return;
    };
    let root = PathBuf::from(std::env::var(ROOT_ENV).expect("fixture root"));
    let mut store = LibsqlStore::new(root.join("durable.db")).expect("open durable SQLite");
    let payment = payment_spec();

    match stage.as_str() {
        "crash_prepared" | "crash_provider" | "crash_receipt" => {
            {
                let mut owner = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
                expected_dispatch(owner.begin(payment.clone(), REQUEST).unwrap(), payment.id);
                if stage != "crash_prepared" {
                    let result = provider(&root, payment.id, REQUEST);
                    if stage == "crash_receipt" {
                        assert_eq!(owner.complete(payment.id, REQUEST, result).unwrap(), RECEIPT);
                    }
                }
            }
            let expected_sequence = if stage == "crash_receipt" { 2 } else { 1 };
            assert_eq!(store.latest_sequence(ACTOR_ID), expected_sequence);
            let mut ready = File::create(root.join("ready")).expect("create crash marker");
            ready.write_all(stage.as_bytes()).expect("write crash marker");
            ready.sync_all().expect("sync crash marker");
            // No graceful destructors: the parent sends a genuine OS kill
            // after observing the committed state marker.
            loop {
                std::thread::park();
            }
        }
        "recover" | "recover_fenced" => {
            let epoch = if stage == "recover_fenced" {
                // The fresh owner claims epoch 2 with an atomic durable
                // transition, without dispatching any external effect.
                let mut successor = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 2);
                let claim = spec("new-owner-claim");
                expected_dispatch(successor.begin(claim.clone(), b"claim").unwrap(), claim.id);
                assert_eq!(store.latest_sequence(ACTOR_ID), 2);

                // Epoch 1 cannot retry Prepared, nor publish a late receipt.
                let mut stale = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
                assert!(matches!(
                    stale.begin(payment.clone(), REQUEST),
                    Err(DurableEffectRuntimeError::Storage(ref error))
                        if error.kind() == io::ErrorKind::PermissionDenied
                ));
                assert!(matches!(
                    stale.complete(payment.id, REQUEST, RECEIPT.to_vec()),
                    Err(DurableEffectRuntimeError::Storage(ref error))
                        if error.kind() == io::ErrorKind::PermissionDenied
                ));
                assert_eq!(store.latest_sequence(ACTOR_ID), 2);
                2
            } else {
                1
            };

            let mut owner = DurableEffectCoordinator::new(&mut store, ACTOR_ID, epoch);
            let result = match owner.begin(payment.clone(), REQUEST).unwrap() {
                DurableEffectDispatchDecision::ReplayRecordedResult(result) => result,
                DurableEffectDispatchDecision::DispatchWithDeduplication { operation_id } => {
                    assert_eq!(operation_id, payment.id);
                    let result = provider(&root, operation_id, REQUEST);
                    owner.complete(operation_id, REQUEST, result).unwrap()
                }
                other => panic!("unexpected recovery decision: {other:?}"),
            };
            assert_eq!(result, RECEIPT);
            assert!(matches!(
                store.load_durable_effect(ACTOR_ID, payment.id).unwrap().unwrap().effect(),
                DurableEffectRecord::Completed { .. }
            ));
            assert_eq!(store.latest_sequence(ACTOR_ID), if epoch == 2 { 3 } else { 2 });
        }
        _ => panic!("unknown process-kill stage {stage}"),
    }
}

fn spawn_worker(stage: &str, fixture: &Fixture) -> Command {
    let mut cmd = Command::new(std::env::current_exe().expect("locate test executable"));
    cmd.arg("--exact")
        .arg("durability_crash_worker")
        .arg("--nocapture")
        .env(STAGE_ENV, stage)
        .env(ROOT_ENV, &fixture.root)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    cmd
}

fn kill_at_checkpoint(stage: &str, fixture: &Fixture) {
    let mut child = spawn_worker(stage, fixture).spawn().expect("start crash worker");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if fixture.root.join("ready").exists() {
            assert_eq!(
                fs::read_to_string(fixture.root.join("ready")).unwrap(),
                stage
            );
            break;
        }
        if let Some(status) = child.try_wait().expect("poll crash worker") {
            panic!("crash worker exited before checkpoint: {status}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("crash worker did not reach {stage} checkpoint");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("OS termination of crash worker");
    let status = child.wait().expect("reap terminated child");
    assert!(!status.success(), "worker must not exit normally");
}

fn restart_worker(stage: &str, fixture: &Fixture) {
    let output = spawn_worker(stage, fixture)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("start independent recovery process");
    assert!(
        output.status.success(),
        "recovery process failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn os_kill_restarts_all_three_receipt_crash_windows() {
    for (stage, expected_calls_before) in [
        ("crash_prepared", 0),
        ("crash_provider", 1),
        ("crash_receipt", 1),
    ] {
        let fixture = Fixture::new(stage);
        kill_at_checkpoint(stage, &fixture);
        assert_eq!(fixture.provider_calls(), expected_calls_before);
        assert_eq!(fixture.provider_mutations(), usize::from(expected_calls_before > 0));
        restart_worker("recover", &fixture);

        // Only a Prepared state re-dispatches; provider-side idempotency
        // ensures recovery after provider commit does not mutate twice.
        let expected_calls_after = if stage == "crash_provider" { 2 } else { 1 };
        assert_eq!(fixture.provider_calls(), expected_calls_after, "{stage}");
        assert_eq!(fixture.provider_mutations(), 1, "{stage}");
        let store = LibsqlStore::new(fixture.db_path()).unwrap();
        assert_eq!(store.latest_sequence(ACTOR_ID), 2, "{stage}");
    }
}

#[test]
fn os_kill_and_epoch_handoff_fence_stale_receipt_then_deduplicate() {
    let fixture = Fixture::new("stale-epoch");
    kill_at_checkpoint("crash_provider", &fixture);
    restart_worker("recover_fenced", &fixture);
    assert_eq!(fixture.provider_calls(), 2);
    assert_eq!(fixture.provider_mutations(), 1);
    let store = LibsqlStore::new(fixture.db_path()).unwrap();
    assert_eq!(store.latest_sequence(ACTOR_ID), 3);
}
