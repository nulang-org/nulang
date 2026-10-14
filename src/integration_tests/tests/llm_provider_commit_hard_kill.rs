//! A compiled Inference.ask workflow hard-killed after the cooperating
//! provider commits, but before its response reaches the Nulang scheduler.
//!
//! Provider-side deduplication here keys on the deterministic request bytes,
//! NOT on a Nulang DurableEffectId. This tests one concrete at-least-once
//! crash window without claiming receipt-backed exactly-once execution.

use super::*;
use crate::runtime::LibsqlStore;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DIR_ENV: &str = "NULANG_LLM_PROVIDER_COMMIT_KILL_DIR";
const READY: &str = "NULANG_LLM_PROVIDER_COMMITTED:";
const TERMINAL_DIR_ENV: &str = "NULANG_LLM_TERMINAL_CHECKPOINT_KILL_DIR";
const TERMINAL_READY: &str = "NULANG_LLM_TERMINAL_CHECKPOINT:";
const SOURCE: &str = r#"
    workflow ProviderCommitKill {
        step ask_step { self.answer = perform Inference.ask("hello") }
    }
    let w = spawn ProviderCommitKill {} in { w }
"#;
const RESULT: &str = "provider-durable-response";

/// A cooperating test provider models an external server whose idempotency
/// ledger is fsynced independently of the Nulang actor's SQLite database.
/// The digest is of the request, not a Nulang-issued operation identity.
#[derive(Clone)]
struct LedgerProvider {
    directory: PathBuf,
    crash_at_commit: bool,
    actor_id: u64,
    calls: Arc<AtomicUsize>,
    new_mutations: Arc<AtomicUsize>,
}

impl LedgerProvider {
    fn new(directory: PathBuf, crash_at_commit: bool, actor_id: u64) -> Self {
        Self {
            directory,
            crash_at_commit,
            actor_id,
            calls: Arc::new(AtomicUsize::new(0)),
            new_mutations: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn ledger_path(&self, request: &nulang_ai::LlmRequest) -> PathBuf {
        let bytes = serde_json::to_vec(request).expect("serializable LLM request");
        let key = blake3::hash(&bytes).to_hex().to_string();
        self.directory.join(format!("provider-{key}.receipt"))
    }
}

#[async_trait::async_trait]
impl nulang_ai::LlmClient for LedgerProvider {
    async fn complete(
        &self,
        request: nulang_ai::LlmRequest,
    ) -> Result<nulang_ai::LlmResponse, nulang_ai::LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let path = self.ledger_path(&request);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(RESULT.as_bytes()).expect("provider write");
                file.sync_all().expect("fsync provider ledger");
                File::open(&self.directory)
                    .expect("open ledger directory")
                    .sync_all()
                    .expect("fsync ledger directory");
                self.new_mutations.fetch_add(1, Ordering::SeqCst);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                assert_eq!(
                    fs::read_to_string(&path).expect("read committed provider receipt"),
                    RESULT,
                    "request identity must resolve to the original provider response"
                );
            }
            Err(error) => panic!("failed to open provider ledger: {error}"),
        }

        if self.crash_at_commit {
            println!("{READY}{}", self.actor_id);
            std::io::stdout()
                .flush()
                .expect("flush provider-commit readiness");
            loop {
                std::thread::park();
            }
        }

        Ok(nulang_ai::LlmResponse {
            content: Some(RESULT.to_string()),
            tool_calls: Vec::new(),
            model: "durable-mock-provider".to_string(),
            finish_reason: "stop".to_string(),
            usage: nulang_ai::TokenUsage::default(),
        })
    }
}

#[test]
fn child_provider_commits_while_compiled_workflow_is_suspended() {
    let Ok(directory) = std::env::var(DIR_ENV) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let (module, _) = compile_source(SOURCE).expect("compile actual workflow bytecode");
    let rt = Rc::new(RefCell::new(Runtime::new()));
    rt.borrow_mut().persistence =
        Box::new(LibsqlStore::new(directory.join("actor.db")).expect("open SQLite store"));

    let reference = {
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
        vm.run().expect("spawn real compiled workflow")
    };
    let actor_id = reference.as_actor_id().expect("actor id");
    rt.borrow_mut().set_llm_client(Box::new(LedgerProvider::new(
        directory.clone(),
        true,
        actor_id,
    )));
    rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
    loop {
        let next = rt.borrow_mut().scheduler.dequeue();
        match next {
            Some(id) => rt.borrow_mut().step_actor(id),
            None => break,
        }
    }

    {
        let current = rt.borrow();
        let actor = current.actors.get(&actor_id).expect("running actor");
        assert!(actor.suspended_execution.is_some());
        assert!(actor.llm_inflight);
        assert_eq!(
            actor
                .get_state_field("step_index")
                .and_then(|value| value.as_int()),
            Some(0)
        );
        let snapshot = current
            .persistence
            .load_snapshot(actor_id)
            .expect("SQLite workflow checkpoint");
        assert_eq!(snapshot.actor_id, actor_id);
        assert!(
            !current
                .persistence
                .read_workflow_events(actor_id)
                .iter()
                .any(|event| matches!(event, WorkflowEvent::StepCompleted { .. })),
            "Nulang cannot have recorded a terminal workflow response yet"
        );
    }
    // The provider worker signals READY only AFTER fsyncing the provider's
    // independent ledger and then blocks before delivering its LLM response.
    loop {
        std::thread::park();
    }
}

#[test]
fn child_terminal_checkpoint_after_compiled_inference() {
    let Ok(directory) = std::env::var(TERMINAL_DIR_ENV) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let (module, _) = compile_source(SOURCE).expect("compile actual workflow bytecode");
    let rt = Rc::new(RefCell::new(Runtime::new()));
    rt.borrow_mut().persistence =
        Box::new(LibsqlStore::new(directory.join("actor.db")).expect("open SQLite store"));

    let reference = {
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
        vm.run().expect("spawn compiled workflow")
    };
    let actor_id = reference.as_actor_id().expect("actor id");
    let provider = LedgerProvider::new(directory.clone(), false, actor_id);
    rt.borrow_mut().set_llm_client(Box::new(provider.clone()));
    rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
    rt.borrow_mut().run_scheduler();

    {
        let current = rt.borrow();
        let actor = current.actors.get(&actor_id).expect("completed actor");
        assert_eq!(
            actor
                .get_state_field("step_index")
                .and_then(|value| value.as_int()),
            Some(1),
            "step must finish before child announces persisted checkpoint"
        );
        let snapshot = current
            .persistence
            .load_snapshot(actor_id)
            .expect("terminal SQLite checkpoint");
        assert_eq!(
            snapshot.state.get("step_index"),
            Some(&crate::runtime::PersistedValue::Int(1)),
            "the committed checkpoint must contain the completed step"
        );
        assert_eq!(
            current
                .persistence
                .read_workflow_events(actor_id)
                .iter()
                .filter(|event| matches!(event, WorkflowEvent::StepCompleted { .. }))
                .count(),
            1,
            "terminal step event must already exist on disk"
        );
    }
    assert_eq!(provider.new_mutations.load(Ordering::SeqCst), 1);
    println!("{TERMINAL_READY}{actor_id}");
    std::io::stdout()
        .flush()
        .expect("flush terminal-checkpoint readiness");
    loop {
        std::thread::park();
    }
}

fn kill_after_stage(
    directory: &Path,
    child_test: &str,
    child_dir_env: &str,
    ready_marker: &str,
) -> u64 {
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg(child_test)
        .arg("--nocapture")
        .env(child_dir_env, directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start compiled-workflow child");

    let stdout = child.stdout.take().expect("capture provider readiness");
    let (send, recv) = mpsc::channel();
    let expected_ready = ready_marker.to_string();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if let Some(id) = line.split(expected_ready.as_str()).nth(1) {
                let _ = send.send(id.trim().parse::<u64>());
                return;
            }
        }
    });

    let acknowledged = recv.recv_timeout(Duration::from_secs(45));
    let kill_result = child.kill();
    let exit_status = child.wait().expect("reap child");
    reader.join().expect("join provider readiness reader");
    let actor_id = acknowledged
        .expect("provider did not acknowledge an fsynced commit")
        .expect("invalid actor id in provider-commit acknowledgment");
    kill_result.expect("child must remain alive until hard kill");
    assert!(!exit_status.success(), "child must be hard-killed");
    actor_id
}

#[test]
fn compiled_inference_provider_commit_receipt_loss_deduplicates_on_recovery() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-llm-provider-commit-kill-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).expect("create isolated directory");
    let actor_id = kill_after_stage(
        &directory,
        "integration_tests::tests::llm_provider_commit_hard_kill::child_provider_commits_while_compiled_workflow_is_suspended",
        DIR_ENV,
        READY,
    );

    let (module, _) = compile_source(SOURCE).expect("recompile recovery bytecode");
    let meta = module
        .actor_metadata
        .first()
        .expect("workflow actor metadata");
    let mut offsets = vec![0; module.behaviors.len()];
    for &index in &meta.behavior_indices {
        if let Some(behavior) = module.behaviors.get(index) {
            offsets[index] = behavior.code_offset;
        }
    }

    let runtime = Rc::new(RefCell::new(Runtime::new()));
    runtime.borrow_mut().persistence = Box::new(
        LibsqlStore::new(directory.join("actor.db")).expect("reopen SQLite after SIGKILL"),
    );
    let provider = LedgerProvider::new(directory.clone(), false, actor_id);
    runtime
        .borrow_mut()
        .set_llm_client(Box::new(provider.clone()));
    runtime.borrow_mut().register_recovery_module(
        actor_id,
        module.clone(),
        offsets,
        vec![None; module.behaviors.len()],
    );
    assert_eq!(
        runtime.borrow_mut().recover_actor(actor_id),
        Some(actor_id),
        "the accepted compiled workflow command must survive the crash"
    );
    runtime.borrow_mut().run_scheduler();

    {
        let recovered = runtime.borrow();
        let actor = recovered.actors.get(&actor_id).expect("recovered workflow");
        assert_eq!(
            actor
                .get_state_field("step_index")
                .and_then(|value| value.as_int()),
            Some(1),
            "accepted step remained incomplete: provider calls={}, new mutations={}, suspended={}, llm_inflight={}, mailbox depth={}",
            provider.calls.load(Ordering::SeqCst),
            provider.new_mutations.load(Ordering::SeqCst),
            actor.suspended_execution.is_some(),
            actor.llm_inflight,
            actor.mailbox.len(),
        );
        assert_eq!(
            recovered.actor_state_string(actor_id, "answer").as_deref(),
            Some(RESULT)
        );
        let completions = recovered
            .persistence
            .read_workflow_events(actor_id)
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    WorkflowEvent::StepCompleted { step_name, .. } if step_name == "ask_step"
                )
            })
            .count();
        assert_eq!(completions, 1, "terminal workflow event must occur once");
    }

    // Current Inference.ask is at-least-once; the provider must deduplicate.
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "recovery currently redrives an in-flight Inference.ask once"
    );
    assert_eq!(
        provider.new_mutations.load(Ordering::SeqCst),
        0,
        "the second invocation must read the first process's fsynced ledger"
    );
    let provider_receipts = fs::read_dir(&directory)
        .expect("list independent provider ledger")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".receipt"))
        .count();
    assert_eq!(
        provider_receipts, 1,
        "only one provider mutation may commit"
    );

    drop(runtime);
    fs::remove_dir_all(directory).expect("remove isolated provider/SQLite state");
}

#[test]
fn compiled_inference_after_terminal_checkpoint_does_not_redispatch() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-llm-terminal-kill-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).expect("create isolated directory");
    let actor_id = kill_after_stage(
        &directory,
        "integration_tests::tests::llm_provider_commit_hard_kill::child_terminal_checkpoint_after_compiled_inference",
        TERMINAL_DIR_ENV,
        TERMINAL_READY,
    );

    let (module, _) = compile_source(SOURCE).expect("recompile recovery bytecode");
    let meta = module
        .actor_metadata
        .first()
        .expect("workflow actor metadata");
    let mut offsets = vec![0; module.behaviors.len()];
    for &index in &meta.behavior_indices {
        if let Some(behavior) = module.behaviors.get(index) {
            offsets[index] = behavior.code_offset;
        }
    }

    let runtime = Rc::new(RefCell::new(Runtime::new()));
    runtime.borrow_mut().persistence = Box::new(
        LibsqlStore::new(directory.join("actor.db")).expect("reopen SQLite after SIGKILL"),
    );
    let provider = LedgerProvider::new(directory.clone(), false, actor_id);
    runtime
        .borrow_mut()
        .set_llm_client(Box::new(provider.clone()));
    runtime.borrow_mut().register_recovery_module(
        actor_id,
        module.clone(),
        offsets,
        vec![None; module.behaviors.len()],
    );
    assert_eq!(
        runtime.borrow_mut().recover_actor(actor_id),
        Some(actor_id),
        "a completed and checkpointed compiled workflow should recover"
    );
    runtime.borrow_mut().run_scheduler();

    {
        let recovered = runtime.borrow();
        let actor = recovered.actors.get(&actor_id).expect("recovered workflow");
        assert_eq!(
            actor
                .get_state_field("step_index")
                .and_then(|value| value.as_int()),
            Some(1)
        );
        // This workflow has no declared persistent state for `self.answer`.
        // The implicit Local value is not part of the durable snapshot.
        // The committed step index/event and provider deduplication are the
        // recovery guarantees this terminal-checkpoint test can assert.
        let snapshot = recovered
            .persistence
            .load_snapshot(actor_id)
            .expect("terminal checkpoint after recovery");
        assert_eq!(
            snapshot.state.get("step_index"),
            Some(&crate::runtime::PersistedValue::Int(1)),
            "completed step index must survive the hard kill"
        );
        assert!(
            !snapshot.state.contains_key("answer"),
            "implicit Local state must not be misrepresented as durable"
        );
        assert_eq!(
            recovered
                .persistence
                .read_workflow_events(actor_id)
                .iter()
                .filter(|event| matches!(
                    event,
                    WorkflowEvent::StepCompleted { step_name, .. } if step_name == "ask_step"
                ))
                .count(),
            1,
            "recovery must not create a second terminal completion"
        );
    }
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        0,
        "a committed terminal workflow must not issue another provider request"
    );
    assert_eq!(provider.new_mutations.load(Ordering::SeqCst), 0);

    drop(runtime);
    fs::remove_dir_all(directory).expect("remove isolated provider/SQLite state");
}
