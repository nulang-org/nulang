//! Compile-time performance analyses over MIR.
//!
//! This module deliberately separates program-visible managed-heap object
//! creation from runtime operations that may allocate bookkeeping. Keeping the
//! distinction explicit lets future contracts choose the guarantee they need:
//! a managed-heap-free kernel is weaker than a fully allocator-free hot path.

use crate::bytecode::Constant;
use crate::mir::{self, FuncRef, RValue, Stmt};

/// Why executing a MIR function may allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AllocationRisk {
    /// Creates a Nulang managed-heap value such as a tuple, record, array,
    /// closure, or runtime string.
    ManagedHeap,
    /// Crosses a runtime boundary that may allocate scheduling, messaging,
    /// continuation, timer, actor, network, or foreign-call bookkeeping.
    Runtime,
    /// Calls through a value (or an invalid direct function index), so the
    /// callee's allocation behavior is not statically known.
    UnknownCall,
}

/// Allocation summary for one MIR function.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AllocationSummary {
    /// Risks introduced directly by this function's own MIR.
    pub direct: Vec<AllocationRisk>,
    /// Direct risks plus risks propagated through statically resolved callees.
    pub transitive: Vec<AllocationRisk>,
}

impl AllocationSummary {
    /// True when the function cannot currently be proven allocation-free.
    pub fn may_allocate(&self) -> bool {
        !self.transitive.is_empty()
    }

    /// True when the function may create a managed Nulang heap value.
    pub fn may_allocate_managed_heap(&self) -> bool {
        self.transitive.contains(&AllocationRisk::ManagedHeap)
    }

    /// True when the function crosses a runtime boundary that may allocate.
    pub fn may_allocate_runtime(&self) -> bool {
        self.transitive.contains(&AllocationRisk::Runtime)
    }
}

fn push_unique<T: PartialEq + Copy>(items: &mut Vec<T>, item: T) -> bool {
    if items.contains(&item) {
        false
    } else {
        items.push(item);
        true
    }
}

/// Classify allocation risk introduced directly by an rvalue.
///
/// Calls are handled by analyze_function_allocations because direct calls
/// need interprocedural propagation and indirect calls are unknown.
pub fn rvalue_allocation_risk(op: &RValue) -> Option<AllocationRisk> {
    match op {
        // Strings are represented as constants in MIR, but the bytecode VM
        // materializes them on the heap on demand. Treating them as heap
        // allocation keeps this analysis backend-independent and conservative.
        RValue::Const(Constant::String(_))
        | RValue::Tuple(_)
        | RValue::Record(_)
        | RValue::RecordUpdate { .. }
        | RValue::ArrayLit(_)
        | RValue::StrConcat(..)
        | RValue::Closure { .. } => Some(AllocationRisk::ManagedHeap),

        // These operations cross runtime boundaries whose implementation may
        // allocate bookkeeping even when they do not return a heap object.
        RValue::Perform { .. }
        | RValue::PerformAsync { .. }
        | RValue::SignalWait { .. }
        | RValue::ReceiveWait { .. }
        | RValue::FFICall { .. }
        | RValue::Migrate { .. }
        | RValue::Spawn { .. }
        | RValue::Send { .. }
        | RValue::Ask { .. } => Some(AllocationRisk::Runtime),

        // Interprocedural handling happens separately.
        RValue::Call { .. } => None,

        // Loads, arithmetic, comparisons, capability checks, mailbox reads,
        // and control-continuation resume do not themselves create a new
        // managed object or cross an allocation-prone runtime boundary.
        RValue::Const(_)
        | RValue::Panic(_)
        | RValue::Load(_)
        | RValue::LoadFieldNamed { .. }
        | RValue::LoadFieldPos { .. }
        | RValue::ArrayLoad { .. }
        | RValue::ArrayLen(_)
        | RValue::Unary(..)
        | RValue::Binary(..)
        | RValue::StringEq(..)
        | RValue::Receive
        | RValue::ReceiveMatch { .. }
        | RValue::ReceiveCommit
        | RValue::SelfRef
        | RValue::CapabilityCheck { .. }
        | RValue::StateGet { .. }
        | RValue::Resume(_) => None,
    }
}

/// Compute conservative interprocedural allocation summaries for ordinary MIR
/// functions. Direct calls through FuncRef::Index propagate callee risks to a
/// fixpoint, including recursive call graphs. Calls through locals are
/// classified as AllocationRisk::UnknownCall.
///
/// Behavior dispatch is represented by dedicated MIR operations (send/ask/
/// spawn), which are already classified as runtime allocation risks above.
pub fn analyze_function_allocations(module: &mir::Module) -> Vec<AllocationSummary> {
    let count = module.functions.len();
    let mut summaries = vec![AllocationSummary::default(); count];
    let mut callees: Vec<Vec<usize>> = vec![Vec::new(); count];

    for (index, function) in module.functions.iter().enumerate() {
        for block in &function.blocks {
            for stmt in &block.stmts {
                let Stmt::Assign { op, .. } = stmt else {
                    continue;
                };

                if let RValue::Call { func, .. } = op {
                    match func {
                        FuncRef::Index(callee) if *callee < count => {
                            if !callees[index].contains(callee) {
                                callees[index].push(*callee);
                            }
                        }
                        FuncRef::Index(_) | FuncRef::Local(_) => {
                            push_unique(
                                &mut summaries[index].direct,
                                AllocationRisk::UnknownCall,
                            );
                        }
                    }
                    continue;
                }

                if let Some(risk) = rvalue_allocation_risk(op) {
                    push_unique(&mut summaries[index].direct, risk);
                }
            }
        }
        summaries[index].transitive = summaries[index].direct.clone();
    }

    // Risks only grow and there are three finite risk categories, so this
    // reaches a fixpoint quickly even for mutually recursive call graphs.
    loop {
        let snapshot: Vec<Vec<AllocationRisk>> = summaries
            .iter()
            .map(|summary| summary.transitive.clone())
            .collect();
        let mut changed = false;

        for index in 0..count {
            for callee in &callees[index] {
                for risk in &snapshot[*callee] {
                    changed |= push_unique(&mut summaries[index].transitive, *risk);
                }
            }
        }

        if !changed {
            break;
        }
    }

    summaries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::{FunctionBuilder, Module, Terminator};
    use crate::types::Type;

    fn finish(mut builder: FunctionBuilder) -> mir::Function {
        builder.terminate(Terminator::Return(None));
        builder.build()
    }

    #[test]
    fn test_allocation_summary_pure_function() {
        let mut module = Module::new("test");
        let mut builder = FunctionBuilder::new("pure", None);
        let dst = builder.add_temp(Type::int());
        builder.assign(dst, RValue::Const(Constant::Int(42)));
        module.functions.push(finish(builder));

        let summaries = analyze_function_allocations(&module);
        assert!(!summaries[0].may_allocate());
    }

    #[test]
    fn test_allocation_summary_managed_heap() {
        let mut module = Module::new("test");
        let mut builder = FunctionBuilder::new("make_array", None);
        let item = builder.add_temp(Type::int());
        builder.assign(item, RValue::Const(Constant::Int(1)));
        let array = builder.add_temp(Type::unit());
        builder.assign(array, RValue::ArrayLit(vec![item]));
        module.functions.push(finish(builder));

        let summaries = analyze_function_allocations(&module);
        assert!(summaries[0].may_allocate_managed_heap());
        assert!(!summaries[0].may_allocate_runtime());
    }

    #[test]
    fn test_allocation_summary_propagates_direct_call() {
        let mut module = Module::new("test");

        let mut callee = FunctionBuilder::new("callee", None);
        let string = callee.add_temp(Type::string());
        callee.assign(
            string,
            RValue::Const(Constant::String("allocates".to_string())),
        );
        module.functions.push(finish(callee));

        let mut caller = FunctionBuilder::new("caller", None);
        let result = caller.add_temp(Type::unit());
        caller.assign(
            result,
            RValue::Call {
                func: FuncRef::Index(0),
                args: vec![],
            },
        );
        module.functions.push(finish(caller));

        let summaries = analyze_function_allocations(&module);
        assert!(summaries[0].may_allocate_managed_heap());
        assert!(summaries[1].may_allocate_managed_heap());
        assert!(summaries[1].direct.is_empty());
    }

    #[test]
    fn test_allocation_summary_indirect_call_is_unknown() {
        let mut module = Module::new("test");
        let mut builder = FunctionBuilder::new("caller", None);
        let callable = builder.add_temp(Type::unit());
        builder.assign(callable, RValue::Const(Constant::Int(0)));
        let result = builder.add_temp(Type::unit());
        builder.assign(
            result,
            RValue::Call {
                func: FuncRef::Local(callable),
                args: vec![],
            },
        );
        module.functions.push(finish(builder));

        let summaries = analyze_function_allocations(&module);
        assert!(summaries[0]
            .transitive
            .contains(&AllocationRisk::UnknownCall));
    }

    #[test]
    fn test_allocation_summary_runtime_boundary() {
        let mut module = Module::new("test");
        let mut builder = FunctionBuilder::new("send", None);
        let actor = builder.add_temp(Type::unit());
        builder.assign(actor, RValue::Const(Constant::Int(1)));
        let dst = builder.add_temp(Type::unit());
        builder.assign(
            dst,
            RValue::Send {
                actor,
                behavior_idx: 0,
                args: vec![],
                remote: false,
            },
        );
        module.functions.push(finish(builder));

        let summaries = analyze_function_allocations(&module);
        assert!(summaries[0].may_allocate_runtime());
    }
}
