//! Native concurrent, fuel-limited CoreVM children joined through TaskScope.
//!
//! This is an opt-in integration of two existing primitives: one fresh CoreVM
//! per child and the bounded native `run_scoped_threads` host. It does not
//! modify source-language `par` lowering or create a VM/WASM scheduler.

use crate::bytecode::OpCode;
use crate::core_vm::{CoreVM, Value};
use crate::runtime::CancellationToken;
use crate::scoped_task_core_vm::{poll_core_vm_once, ScopedCoreVmTask};
use crate::scoped_task_reference::TaskPoll;
use crate::scoped_tasks::TaskOutcome;
use crate::scoped_thread_host::{
    run_scoped_threads, ScopedThreadError, ScopedThreadExit, ScopedThreadTask,
};

/// A fail-closed bytecode admission error, distinct from a worker lifecycle error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParallelCoreVmError {
    Worker(ScopedThreadError),
    UnsafeOpcode {
        task_index: usize,
        instruction_index: usize,
        opcode: OpCode,
    },
}

impl From<ScopedThreadError> for ParallelCoreVmError {
    fn from(value: ScopedThreadError) -> Self {
        Self::Worker(value)
    }
}

/// Explicitly approved instructions for a fully isolated, fuel-bounded child.
///
/// Unknown and newly-added instructions are rejected by default. Calls,
/// closures, mutable heap operations, effects, actor communication, I/O,
/// introspection, FFI and Python interop require more complete effect proofs.
/// An instruction must be approved even if it currently appears unreachable.
fn is_parallel_safe_opcode(opcode: OpCode) -> bool {
    matches!(
        opcode,
        OpCode::Nop
            | OpCode::Halt
            | OpCode::Const0
            | OpCode::Const1
            | OpCode::Const2
            | OpCode::ConstM1
            | OpCode::ConstU
            | OpCode::ConstL
            | OpCode::Load
            | OpCode::Store
            | OpCode::Move
            | OpCode::Pop
            | OpCode::Dup
            | OpCode::Swap
            | OpCode::IAdd
            | OpCode::ISub
            | OpCode::IMul
            | OpCode::IDiv
            | OpCode::IMod
            | OpCode::INeg
            | OpCode::IInc
            | OpCode::IDec
            | OpCode::IPow
            | OpCode::Xor
            | OpCode::Shl
            | OpCode::Shr
            | OpCode::BitAnd
            | OpCode::BitOr
            | OpCode::FAdd
            | OpCode::FSub
            | OpCode::FMul
            | OpCode::FDiv
            | OpCode::FNeg
            | OpCode::FMod
            | OpCode::IToF
            | OpCode::FToI
            | OpCode::FPow
            | OpCode::ICmpEq
            | OpCode::ICmpLt
            | OpCode::ICmpGt
            | OpCode::ICmpLe
            | OpCode::ICmpGe
            | OpCode::FCmpEq
            | OpCode::FCmpLt
            | OpCode::FCmpGt
            | OpCode::SCmpEq
            | OpCode::Not
            | OpCode::And
            | OpCode::Or
            | OpCode::Jmp
            | OpCode::JmpT
            | OpCode::JmpF
    )
}

/// Run isolated bytecode children concurrently on bounded native threads.
///
/// Each callback creates and destroys its own `CoreVM`, borrows immutable
/// bytecode from the caller, and executes at most `instruction_budget`
/// instructions. Existing instruction-boundary cancellation sampling is used
/// unchanged. Completion or cancellation is acknowledged only after VM
/// execution terminates, and all launched OS threads join before return.
///
/// `max_in_flight` limits concurrent workers, and `max_pending` bounds the
/// entire initial task set. The first reported VM error cancels queued tasks
/// and signals running siblings; terminal results retain submission order.
/// Cooperative fuel/cancel checks cannot preempt a blocking host operation.
///
/// This opt-in native API is not the source-language `par` scheduler. It
/// does not use wasm32 threads, provide resumable bytecode or persistence.
pub fn run_parallel_core_vm_scope(
    tasks: &[ScopedCoreVmTask<'_>],
    max_in_flight: usize,
    max_pending: usize,
    instruction_budget: u64,
    parent: &CancellationToken,
) -> Result<Vec<TaskOutcome<Value, String>>, ParallelCoreVmError> {
    // Reject the entire bounded batch before launching any child or creating
    // a VM. Whole-module scanning prevents dead/unreachable instructions and
    // later control-flow branches from bypassing the effect boundary.
    for (task_index, child) in tasks.iter().enumerate() {
        for (instruction_index, instruction) in child.module.instructions.iter().enumerate() {
            if !is_parallel_safe_opcode(instruction.opcode) {
                return Err(ParallelCoreVmError::UnsafeOpcode {
                    task_index,
                    instruction_index,
                    opcode: instruction.opcode,
                });
            }
        }
    }

    let children: Vec<ScopedThreadTask<'_, Value, String>> = tasks
        .iter()
        .map(|child| {
            let module = child.module;
            let entry_pc = child.entry_pc;
            Box::new(move |cancellation| {
                let mut vm = CoreVM::new();
                let result = match vm.load_module_from_code(module) {
                    Ok(module_idx) => poll_core_vm_once(
                        &mut vm,
                        module_idx,
                        entry_pc,
                        instruction_budget,
                        &cancellation,
                    ),
                    Err(error) => TaskPoll::Ready(Err(error)),
                };
                match result {
                    TaskPoll::Ready(value) => ScopedThreadExit::Completed(value),
                    TaskPoll::Cancelled => ScopedThreadExit::Cancelled,
                    TaskPoll::Pending => {
                        unreachable!("one-shot CoreVM must reach a terminal state")
                    }
                }
            }) as ScopedThreadTask<'_, Value, String>
        })
        .collect();
    run_scoped_threads(children, max_in_flight, max_pending, parent).map_err(Into::into)
}
