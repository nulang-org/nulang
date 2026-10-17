//! A real subprocess-kill proof of the experimental compiled durable-effect
//! adapter against file-backed libSQL. Provider-side create_new supplies the
//! cooperative idempotency needed for the ambiguous crash window.
//! This does NOT exercise the VM's automatic PerformAsync dispatch or leases.

#![cfg(all(feature = "sqlite", unix))]

use nulang::bytecode::CodeModule;
use nulang::durable_effect_runtime::DurableEffectCoordinator;
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::runtime::{LibsqlStore, PersistenceStore};
use nulang::typechecker::TypeChecker;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ACTOR_ID: u64 = 42;
const REQUEST: &[u8] = b"order=42&cents=100";
const EXECUTION_KEY: &str = "order:42:payment";

fn compiled_source() -> CodeModule {
    let source =
        "actor Payments { behavior charge(order: String) { perform Inference.ask(order) } }";
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut types = TypeChecker::new();
    types.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &types.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).unwrap();
    let module = mir_codegen::compile_mir(&mut mir, "compiled-durable-dispatch").unwrap();
    assert_eq!(module.effect_sites.len(), 1);
    module
}

/// An external system cooperating through the stable idempotency key.
/// create_new makes a second mutation distinguishable from a retry that
/// retrieves the previously durable provider-side result.
fn idempotent_provider(path: &Path, key: &str, request: &[u8]) -> io::Result<Vec<u8>> {
    assert_eq!(request, REQUEST);
    let durable_mutation = format!("{key}=payment-receipt");
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(durable_mutation.as_bytes())?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let recorded = fs::read(path)?;
            if recorded != durable_mutation.as_bytes() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "idempotency key differs from the committed provider operation",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(b"payment-receipt".to_vec())
}

fn acknowledge_and_park(path: &Path, stage: &str) -> ! {
    let temp = path.with_extension("ack.tmp");
    let mut file = fs::File::create(&temp).expect("create ACK temp file");
    file.write_all(stage.as_bytes()).expect("write ACK");
    file.sync_all().expect("sync ACK");
    drop(file);
    fs::rename(&temp, path).expect("publish ACK");
    loop {
        thread::park();
    }
}

#[test]
#[ignore = "crash fixture is spawned explicitly by its parent test"]
fn durable_effect_crash_fixture_child() {
    let Some(db_path) = std::env::var_os("NULANG_DURABLE_EFFECT_CRASH_DB") else {
        return;
    };
    let stage = std::env::var("NULANG_DURABLE_EFFECT_CRASH_STAGE")
        .expect("parent must select a crash window");
    let ack_path = PathBuf::from(
        std::env::var_os("NULANG_DURABLE_EFFECT_CRASH_ACK").expect("parent ACK path"),
    );
    let provider_path = PathBuf::from(
        std::env::var_os("NULANG_DURABLE_EFFECT_CRASH_PROVIDER").expect("parent provider path"),
    );

    let module = compiled_source();
    let pc = module.effect_sites[0].pc;
    let mut store = LibsqlStore::new(Path::new(&db_path)).expect("open file-backed libSQL");
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);

    let receipt = coordinator
        .execute_compiled_async_with_deduplication(
            &module,
            pc,
            EXECUTION_KEY,
            0,
            REQUEST,
            |key, request| match stage.as_str() {
                "prepared" => acknowledge_and_park(&ack_path, "prepared"),
                "provider_committed" => {
                    idempotent_provider(&provider_path, key, request)?;
                    acknowledge_and_park(&ack_path, "provider_committed");
                }
                "receipt_committed" => idempotent_provider(&provider_path, key, request),
                other => panic!("unknown crash fixture stage: {other}"),
            },
        )
        .expect("durable provider completion");

    assert_eq!(stage, "receipt_committed");
    assert_eq!(receipt, b"payment-receipt");
    acknowledge_and_park(&ack_path, "receipt_committed");
}

fn run_until_ack_then_kill(dir: &Path, stage: &str) {
    let db_path = dir.join("durable.db");
    let ack_path = dir.join("child.ack");
    let provider_path = dir.join("provider.mutation");

    let mut child = Command::new(std::env::current_exe().expect("integration test executable"))
        .args(["--exact", "durable_effect_crash_fixture_child", "--ignored", "--nocapture"])
        .env("NULANG_DURABLE_EFFECT_CRASH_DB", &db_path)
        .env("NULANG_DURABLE_EFFECT_CRASH_STAGE", stage)
        .env("NULANG_DURABLE_EFFECT_CRASH_ACK", &ack_path)
        .env("NULANG_DURABLE_EFFECT_CRASH_PROVIDER", &provider_path)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash fixture");

    let mut observed = false;
    for _ in 0..2_000 {
        if let Ok(value) = fs::read_to_string(&ack_path) {
            assert_eq!(value, stage, "crash ACK must identify the durable boundary");
            observed = true;
            break;
        }
        if let Some(status) = child.try_wait().expect("poll crash fixture") {
            panic!("crash fixture exited before ACK: {status}");
        }
        thread::sleep(Duration::from_millis(5));
    }

    // Even on timeout, terminate the child to prevent orphaned test fixtures.
    child.kill().expect("kill crash fixture");
    let status = child.wait().expect("reap crash fixture");
    assert!(observed, "fixture never acknowledged stage {stage}");
    assert!(!status.success(), "fixture was expected to be killed");
}

fn verify_crash_recovery(stage: &str) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-effect-kill-{}-{stage}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create isolated test directory");
    run_until_ack_then_kill(&dir, stage);

    let provider_path = dir.join("provider.mutation");
    let mut store = LibsqlStore::new(dir.join("durable.db")).expect("reopen durable history");
    let expected_tail = if stage == "receipt_committed" { 2 } else { 1 };
    assert_eq!(
        store.latest_sequence(ACTOR_ID),
        expected_tail,
        "crash must preserve exactly the acknowledged durable boundary"
    );
    assert_eq!(
        provider_path.exists(),
        stage != "prepared",
        "provider side effect must match the acknowledged crash window"
    );

    let module = compiled_source();
    let pc = module.effect_sites[0].pc;
    let mut calls = 0;
    let receipt = {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        coordinator
            .execute_compiled_async_with_deduplication(
                &module,
                pc,
                EXECUTION_KEY,
                0,
                REQUEST,
                |key, request| {
                    assert_ne!(stage, "receipt_committed", "replay must not redispatch");
                    calls += 1;
                    idempotent_provider(&provider_path, key, request)
                },
            )
            .expect("recover and replay exact durable effect")
    };

    assert_eq!(receipt, b"payment-receipt");
    assert_eq!(calls, usize::from(stage != "receipt_committed"));
    assert_eq!(store.latest_sequence(ACTOR_ID), 2);
    assert!(provider_path.exists());

    // A second restart/observation cannot invoke the provider or add history.
    drop(store);
    let mut reopened = LibsqlStore::new(dir.join("durable.db")).expect("reopen completed effect");
    let mut coordinator = DurableEffectCoordinator::new(&mut reopened, ACTOR_ID, 1);
    assert_eq!(
        coordinator
            .execute_compiled_async_with_deduplication(
                &module,
                pc,
                EXECUTION_KEY,
                0,
                REQUEST,
                |_, _| panic!("terminal receipt must never invoke provider again"),
            )
            .expect("replay terminal receipt"),
        receipt
    );
    assert_eq!(reopened.latest_sequence(ACTOR_ID), 2);
    drop(coordinator);
    drop(reopened);
    fs::remove_dir_all(dir).expect("remove isolated test directory");
}

#[test]
fn kill_after_prepared_intent_recovers_and_dispatches_once() {
    verify_crash_recovery("prepared");
}

#[test]
fn kill_after_provider_commit_retries_with_provider_idempotency() {
    verify_crash_recovery("provider_committed");
}

#[test]
fn kill_after_terminal_receipt_replays_without_provider_call() {
    verify_crash_recovery("receipt_committed");
}
