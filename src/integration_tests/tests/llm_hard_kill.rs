//! Hard-process-kill integration acceptance test for a compiled Inference.ask
//! workflow. This covers the suspended-before-response crash window on SQLite.
//!
//! It is deliberately NOT a receipt replay / exactly-once provider test:
//! Inference.ask currently has an at-least-once external delivery boundary,
//! and the existing durable-effect coordinator's subprocess fixture covers
//! separate lower-level Prepared/Completed crash windows.

use super::*;
use crate::runtime::LibsqlStore;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHILD_DIR: &str = "NULANG_COMPILED_LLM_KILL_DIR";
const READY: &str = "NULANG_COMPILED_LLM_READY:";
const SOURCE: &str = r#"
    workflow LlmFlowHardKill {
        step ask_step { self.answer = perform Inference.ask("hello") }
    }
    let w = spawn LlmFlowHardKill {} in { w }
"#;

/// Invoked in a separate test-binary process by the parent test. The child
/// must park AFTER it has durably checkpointed a suspended compiled workflow,
/// and must be terminated by SIGKILL, not by dropping its runtime.
#[test]
fn child_compiled_llm_suspended_before_response() {
    let Ok(directory) = std::env::var(CHILD_DIR) else {
        return;
    };

    let db_path = PathBuf::from(directory).join("actor.db");
    let (module, _) = compile_source(SOURCE).expect("compile real Nulang workflow");
    let rt = Rc::new(RefCell::new(Runtime::new()));
    rt.borrow_mut().persistence = Box::new(LibsqlStore::new(&db_path).expect("open SQLite"));
    rt.borrow_mut()
        .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("child-response")));

    let actor_ref = {
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
        vm.run().expect("spawn compiled workflow")
    };
    let actor_id = actor_ref.as_actor_id().expect("spawn returns an actor id");
    rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);

    // Execute the compiled behavior until it suspends. Do not pump the async
    // provider-completion channel: the provider result must remain unobserved.
    loop {
        let next = rt.borrow_mut().scheduler.dequeue();
        match next {
            Some(id) => rt.borrow_mut().step_actor(id),
            None => break,
        }
    }

    {
        let live = rt.borrow();
        let actor = live.actors.get(&actor_id).expect("live workflow");
        assert!(
            actor.suspended_execution.is_some(),
            "compiled LLM call must suspend"
        );
        assert!(actor.llm_inflight, "LLM call must be in flight");
        assert_eq!(
            actor.get_state_field("step_index").and_then(|v| v.as_int()),
            Some(0),
            "in-flight LLM step is not terminal"
        );
        let snapshot = live
            .persistence
            .load_snapshot(actor_id)
            .expect("SQLite must contain a recoverable actor snapshot");
        assert_eq!(snapshot.actor_id, actor_id);
    }

    // The parent kills only after the checked persistent boundary is visible.
    println!("{READY}{actor_id}");
    std::io::stdout()
        .flush()
        .expect("flush readiness acknowledgment");
    loop {
        std::thread::park();
    }
}

#[test]
fn sigkill_of_compiled_inference_workflow_recovers_without_losing_step() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-compiled-llm-kill-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).expect("create isolated SQLite directory");
    let db_path = directory.join("actor.db");

    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg(
            "integration_tests::tests::llm_hard_kill::child_compiled_llm_suspended_before_response",
        )
        .arg("--nocapture")
        .env(CHILD_DIR, &directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start real compiled-workflow child");

    let stdout = child.stdout.take().expect("capture child readiness");
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if let Some(id) = line.split(READY).nth(1) {
                let _ = sender.send(id.trim().parse::<u64>());
                return;
            }
        }
    });

    let ready = receiver.recv_timeout(Duration::from_secs(45));
    // Reap the subprocess on both success and failure: no orphan test child.
    let killed = child.kill();
    let exit = child.wait().expect("reap child process");
    reader.join().expect("join readiness reader");
    let actor_id = ready
        .expect("child never acknowledged the suspended durable checkpoint")
        .expect("child returned an invalid actor id");
    killed.expect("child must still be alive when hard-killed");
    assert!(!exit.success(), "process must not exit gracefully");

    // This is a genuinely new process, connection and runtime. Recompile and
    // register the same actor executable before restoring the accepted step.
    let (module, _) = compile_source(SOURCE).expect("recompile recovery module");
    let meta = module
        .actor_metadata
        .first()
        .expect("workflow actor metadata");
    let mut offsets = vec![0; module.behaviors.len()];
    for &index in &meta.behavior_indices {
        if let Some(entry) = module.behaviors.get(index) {
            offsets[index] = entry.code_offset;
        }
    }

    let rt = Rc::new(RefCell::new(Runtime::new()));
    rt.borrow_mut().persistence = Box::new(LibsqlStore::new(&db_path).expect("reopen SQLite"));
    let provider = nulang_ai::MockLlmClient::text("recovered-response");
    rt.borrow_mut().set_llm_client(Box::new(provider.clone()));
    rt.borrow_mut().register_recovery_module(
        actor_id,
        module.clone(),
        offsets,
        vec![None; module.behaviors.len()],
    );
    assert_eq!(
        rt.borrow_mut().recover_actor(actor_id),
        Some(actor_id),
        "a genuinely interrupted compiled LLM step must remain recoverable"
    );
    rt.borrow_mut().run_scheduler();

    {
        let recovered = rt.borrow();
        let actor = recovered.actors.get(&actor_id).expect("recovered actor");
        assert_eq!(
            actor.get_state_field("step_index").and_then(|v| v.as_int()),
            Some(1)
        );
        assert!(actor.suspended_execution.is_none());
        assert_eq!(
            recovered.actor_state_string(actor_id, "answer").as_deref(),
            Some("recovered-response")
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
        assert_eq!(completions, 1, "terminal step must appear once in SQLite");
    }

    let calls = provider.recorded_calls();
    assert_eq!(
        calls.len(),
        1,
        "an uncompleted external call may redrive once after hard kill"
    );
    assert_eq!(calls[0].messages[0].content, "hello");

    drop(rt);
    fs::remove_dir_all(directory).expect("remove isolated SQLite data");
}
