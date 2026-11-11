//! Cross-host cancellation safepoint contract. The two interpreters share
//! the portable cancellation token; neither introduces new bytecode.

use crate::bytecode::{CodeModule, Instruction, OpCode};
use crate::core_vm::CoreVM;
use crate::scoped_tasks::CancellationToken;
use crate::vm::{DebugAction, DebugContext, DebugHook, VM};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn module_with_halt() -> CodeModule {
    let mut module = CodeModule::new("cancellation_conformance");
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);
    module
}

#[test]
fn pre_cancelled_scope_rejects_halt_before_execution_on_both_hosts() {
    let token = CancellationToken::default();
    token.cancel();
    let module = module_with_halt();

    let mut native = VM::new();
    native.load_module(module.clone());
    native.set_cancellation_token(Some(token.clone()));
    let err = native
        .run()
        .expect_err("cancelled native VM must not complete");
    assert!(format!("{err}").contains("Task cancelled"), "{err}");

    let mut portable = CoreVM::new();
    let idx = portable.load_module_from_code(&module).unwrap();
    portable.set_cancellation_token(Some(token));
    let err = portable
        .run(idx, 0)
        .expect_err("cancelled CoreVM must not complete");
    assert!(err.contains("Task cancelled"), "{err}");
    assert!(
        portable.frames.is_empty(),
        "a pre-cancelled CoreVM must not allocate an activation frame"
    );
}

#[test]
fn native_precancelled_entrypoints_do_not_allocate_execution_frames() {
    let token = CancellationToken::default();
    token.cancel();
    let mut native = VM::new_without_jit();
    native.load_module(module_with_halt());
    native.set_cancellation_token(Some(token));

    let err = native.run().expect_err("cancelled run must fail");
    assert!(format!("{err}").contains("Task cancelled"));
    assert!(native.frames().is_empty(), "cancelled run created a frame");

    let err = native
        .run_from(0, 0)
        .expect_err("cancelled run_from must fail");
    assert!(format!("{err}").contains("Task cancelled"));
    assert!(
        native.frames().is_empty(),
        "cancelled run_from created a frame"
    );

    let err = native
        .call_function(0, 0, &[])
        .expect_err("cancelled call_function must fail");
    assert!(format!("{err}").contains("Task cancelled"));
    assert!(
        native.frames().is_empty(),
        "cancelled call_function created a frame"
    );
}

#[test]
fn native_precancelled_entrypoints_preserve_existing_activation() {
    let token = CancellationToken::default();
    let mut native = VM::new_without_jit();
    native.load_module(module_with_halt());
    native
        .run()
        .expect("baseline run should create a completed frame");
    let before_pc = native.frames()[0].pc;
    let before_register = native.frames()[0].regs[0];

    token.cancel();
    native.set_cancellation_token(Some(token));
    for name in ["run", "run_from", "call_function", "resume"] {
        let result = match name {
            "run" => native.run(),
            "run_from" => native.run_from(0, 0),
            "call_function" => native.call_function(0, 0, &[]),
            _ => native.resume(),
        };
        let err = result.expect_err("cancelled scope must reject execution");
        assert!(format!("{err}").contains("Task cancelled"), "{name}: {err}");
        assert_eq!(native.frames().len(), 1, "{name} changed frame count");
        assert_eq!(native.frames()[0].pc, before_pc, "{name} changed pc");
        assert_eq!(
            native.frames()[0].regs[0],
            before_register,
            "{name} changed register"
        );
    }
}

#[test]
fn expired_deadline_rejects_program_on_both_hosts() {
    let token = CancellationToken::with_deadline(Instant::now() - Duration::from_secs(1));
    let module = module_with_halt();

    let mut native = VM::new_without_jit();
    native.load_module(module.clone());
    native.set_cancellation_token(Some(token.clone()));
    let err = native.run().unwrap_err();
    assert!(format!("{err}").contains("Task deadline exceeded"), "{err}");
    assert!(
        native.frames().is_empty(),
        "an expired deadline must not create a native activation frame"
    );

    let mut portable = CoreVM::new();
    let idx = portable.load_module_from_code(&module).unwrap();
    portable.set_cancellation_token(Some(token));
    let err = portable.run(idx, 0).unwrap_err();
    assert!(err.contains("Task deadline exceeded"), "{err}");
}

#[test]
fn uncancelled_program_remains_compatible_with_both_hosts() {
    let token = CancellationToken::default();
    let module = module_with_halt();

    let mut native = VM::new();
    native.load_module(module.clone());
    native.set_cancellation_token(Some(token.clone()));
    assert!(native.run().is_ok());

    let mut portable = CoreVM::new();
    let idx = portable.load_module_from_code(&module).unwrap();
    portable.set_cancellation_token(Some(token));
    assert!(portable.run(idx, 0).is_ok());
}

#[derive(Debug)]
struct CancelOnFirstInstruction {
    token: CancellationToken,
    visits: Arc<AtomicUsize>,
}

impl DebugHook for CancelOnFirstInstruction {
    fn before_instruction(&mut self, _ctx: &DebugContext) -> DebugAction {
        self.visits.fetch_add(1, Ordering::SeqCst);
        self.token.cancel();
        DebugAction::Continue
    }
}

#[test]
fn native_interpreter_stops_at_the_next_instruction_after_cancellation() {
    let token = CancellationToken::default();
    let visits = Arc::new(AtomicUsize::new(0));
    let mut module = CodeModule::new("cancel_between_instructions");
    module.emit(Instruction::new1(OpCode::Const1, 0));
    module.emit(Instruction::new1(OpCode::Const2, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);

    let mut native = VM::new_without_jit();
    native.load_module(module);
    native.set_cancellation_token(Some(token.clone()));
    native.set_debug_hook(Some(Box::new(CancelOnFirstInstruction {
        token,
        visits: Arc::clone(&visits),
    })));
    let err = native.run().unwrap_err();
    assert!(format!("{err}").contains("Task cancelled"), "{err}");
    assert_eq!(visits.load(Ordering::SeqCst), 1);
}

#[test]
fn native_run_from_and_resume_honor_token_without_changing_bytecode() {
    let token = CancellationToken::default();
    token.cancel();
    let mut native = VM::new();
    native.load_module(module_with_halt());
    native.set_cancellation_token(Some(token));

    let start_error = native.run_from(0, 0).unwrap_err();
    assert!(format!("{start_error}").contains("Task cancelled"));
    let resume_error = native.resume().unwrap_err();
    assert!(format!("{resume_error}").contains("Task cancelled"));

    // Detaching the scope returns the VM to its ordinary execution behavior.
    native.set_cancellation_token(None);
    assert!(native.resume().is_ok());
}
