//! Static runtime-feature analysis for compiled Nulang modules.
//!
//! The compiler and runtime support a deliberately broad set of facilities:
//! actors, durable execution, algebraic effects, distribution, Python, FFI,
//! and host I/O. Most programs need only a subset. This module derives a
//! conservative feature profile directly from a [`CodeModule`] so packaging,
//! Nulang Cloud, sandboxing, and future selective linking can avoid paying for
//! subsystems that a compiled artifact cannot reach.
//!
//! The analysis is intentionally conservative: false positives cost footprint;
//! false negatives could remove a required runtime/security boundary. When in
//! doubt, classify the feature as required.

use std::collections::BTreeSet;

use crate::bytecode::{CodeModule, OpCode};

/// Runtime subsystems that a compiled module may require.
///
/// Ordering is stable so profiles can be serialized deterministically by
/// callers without depending on hash-map iteration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuntimeFeature {
    /// Language object heap and reference-management machinery.
    Heap,
    /// Actor lifecycle, mailboxes, supervision-facing actor operations, state.
    Actors,
    /// Durable/persistent actor storage and replay machinery.
    Persistence,
    /// Algebraic effect dispatch and continuation machinery.
    Effects,
    /// Suspending asynchronous effect execution.
    AsyncEffects,
    /// Cluster membership and cross-node actor operations.
    Distribution,
    /// Python object/runtime interop.
    PythonInterop,
    /// Native foreign-function calls.
    Ffi,
    /// Host filesystem operations.
    FileIo,
    /// Host stdin/stdout operations.
    StdIo,
    /// Debugger/meta-introspection opcodes.
    Debug,
    /// Typed external-authority metadata is present and must be enforced.
    ExternalAuthority,
}

/// Conservative feature requirements for one compiled module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionProfile {
    features: BTreeSet<RuntimeFeature>,
}

impl ExecutionProfile {
    /// Derive the runtime requirements of `module` from bytecode and metadata.
    pub fn analyze(module: &CodeModule) -> Self {
        let mut profile = Self::default();

        if !module.actor_metadata.is_empty() {
            profile.require(RuntimeFeature::Actors);
        }
        if module.actor_metadata.iter().any(|meta| meta.persistent) {
            profile.require(RuntimeFeature::Persistence);
        }
        if !module.foreign_functions.is_empty() {
            // Conservatively retain FFI support even if an optimizer made the
            // declaration unreachable but left it in module metadata.
            profile.require(RuntimeFeature::Ffi);
        }
        if !module.spawn_capability_grants.is_empty() {
            profile.require(RuntimeFeature::ExternalAuthority);
            profile.require(RuntimeFeature::Actors);
        }

        for instruction in &module.instructions {
            profile.classify_opcode(instruction.opcode);
        }

        profile.close_dependencies();
        profile
    }

    /// Whether this profile requires a particular runtime subsystem.
    pub fn requires(&self, feature: RuntimeFeature) -> bool {
        self.features.contains(&feature)
    }

    /// Stable iterator over required features.
    pub fn features(&self) -> impl Iterator<Item = RuntimeFeature> + '_ {
        self.features.iter().copied()
    }

    /// Number of runtime feature groups required by this module.
    pub fn len(&self) -> usize {
        self.features.len()
    }

    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    /// True when the module has no persistent actor requirement.
    pub fn is_ephemeral(&self) -> bool {
        !self.requires(RuntimeFeature::Persistence)
    }

    /// True when the module requires no cluster/cross-node runtime support.
    pub fn is_local_only(&self) -> bool {
        !self.requires(RuntimeFeature::Distribution)
    }

    /// True when the module reaches a host boundary that deployment policy
    /// should explicitly review or sandbox.
    pub fn reaches_host_boundary(&self) -> bool {
        [
            RuntimeFeature::Distribution,
            RuntimeFeature::PythonInterop,
            RuntimeFeature::Ffi,
            RuntimeFeature::FileIo,
            RuntimeFeature::StdIo,
            RuntimeFeature::ExternalAuthority,
        ]
        .into_iter()
        .any(|feature| self.requires(feature))
    }

    fn require(&mut self, feature: RuntimeFeature) {
        self.features.insert(feature);
    }

    fn close_dependencies(&mut self) {
        if self.requires(RuntimeFeature::Persistence) || self.requires(RuntimeFeature::Distribution)
        {
            self.require(RuntimeFeature::Actors);
        }
        if self.requires(RuntimeFeature::Actors)
            || self.requires(RuntimeFeature::PythonInterop)
            || self.requires(RuntimeFeature::FileIo)
            || self.requires(RuntimeFeature::StdIo)
        {
            // Actors own an ActorHeap; Python conversion and read-oriented
            // host I/O may materialize heap values such as strings/arrays.
            // Keep this deliberately conservative for future selective linking.
            self.require(RuntimeFeature::Heap);
        }
        if self.requires(RuntimeFeature::AsyncEffects) {
            self.require(RuntimeFeature::Effects);
        }
    }

    fn classify_opcode(&mut self, opcode: OpCode) {
        use OpCode::*;

        match opcode {
            Alloc | FieldL | FieldS | ArrAlloc | ArrLoad | ArrStore | ArrLen | TupleMk | TupleL
            | RecMk | RecL | RecS | RecCopy | Copy | Drop | Closure | CapLoad | CapStore | FToS
            | SConcat => {
                self.require(RuntimeFeature::Heap);
            }

            Spawn | Send | Ask | SelfOp | Receive | Monitor | Demon | Link | Unlink | Exit
            | Yield | StateGet | StateSet | Emit | SignalWait | ReceiveMatch | ReceiveWait
            | ReceiveCommit => {
                self.require(RuntimeFeature::Actors);
            }

            Perform | Handle | Resume | Unwind | PerformDirect => {
                self.require(RuntimeFeature::Effects);
            }
            PerformAsync => {
                self.require(RuntimeFeature::AsyncEffects);
                self.require(RuntimeFeature::Effects);
            }

            PyImport | PyGetAttr | PyCall | PyCallKw | PySetAttr | PyToNu | PyFromNu
            | PyRelease => {
                self.require(RuntimeFeature::PythonInterop);
            }

            FFICall => {
                self.require(RuntimeFeature::Ffi);
            }

            NodeId | Migrate | RSend | RAsk | RSpawn | Gossip => {
                self.require(RuntimeFeature::Distribution);
                self.require(RuntimeFeature::Actors);
            }

            FOpen | FRead | FWrite | FClose => {
                self.require(RuntimeFeature::FileIo);
            }

            SPrint | SRead | Print => {
                self.require(RuntimeFeature::StdIo);
            }

            DbgBreak | DbgPrint | DbgStack | MetaType | MetaCap => {
                self.require(RuntimeFeature::Debug);
            }

            // Pure compute/control instructions have no optional runtime
            // subsystem requirement beyond the base VM itself.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Instruction;

    fn module_with(opcodes: &[OpCode]) -> CodeModule {
        let mut module = CodeModule::new("profile-test");
        module
            .instructions
            .extend(opcodes.iter().copied().map(Instruction::new0));
        module
    }

    #[test]
    fn pure_compute_does_not_pull_optional_runtime_subsystems() {
        let module = module_with(&[OpCode::Const1, OpCode::IAdd, OpCode::RetVal]);
        let profile = ExecutionProfile::analyze(&module);
        assert!(profile.is_empty());
        assert!(profile.is_ephemeral());
        assert!(profile.is_local_only());
        assert!(!profile.reaches_host_boundary());
    }

    #[test]
    fn actors_and_distribution_pull_required_dependencies() {
        let module = module_with(&[OpCode::RSend]);
        let profile = ExecutionProfile::analyze(&module);
        assert!(profile.requires(RuntimeFeature::Distribution));
        assert!(profile.requires(RuntimeFeature::Actors));
        assert!(profile.requires(RuntimeFeature::Heap));
        assert!(profile.is_ephemeral());
        assert!(!profile.is_local_only());
        assert!(profile.reaches_host_boundary());
    }

    #[test]
    fn async_effects_imply_effect_runtime() {
        let module = module_with(&[OpCode::PerformAsync]);
        let profile = ExecutionProfile::analyze(&module);
        assert!(profile.requires(RuntimeFeature::AsyncEffects));
        assert!(profile.requires(RuntimeFeature::Effects));
    }

    #[test]
    fn allocating_string_ops_require_heap() {
        let module = module_with(&[OpCode::FToS, OpCode::SConcat]);
        let profile = ExecutionProfile::analyze(&module);
        assert!(profile.requires(RuntimeFeature::Heap));
    }

    #[test]
    fn host_integrations_are_visible_to_deployment_policy() {
        let module = module_with(&[
            OpCode::PyCall,
            OpCode::FFICall,
            OpCode::FRead,
            OpCode::SPrint,
            OpCode::DbgBreak,
        ]);
        let profile = ExecutionProfile::analyze(&module);

        assert!(profile.requires(RuntimeFeature::PythonInterop));
        assert!(profile.requires(RuntimeFeature::Ffi));
        assert!(profile.requires(RuntimeFeature::FileIo));
        assert!(profile.requires(RuntimeFeature::StdIo));
        assert!(profile.requires(RuntimeFeature::Debug));
        assert!(profile.requires(RuntimeFeature::Heap));
        assert!(profile.reaches_host_boundary());
    }

    #[test]
    fn authority_metadata_is_never_treated_as_pure_compute() {
        let mut module = module_with(&[OpCode::Spawn]);
        module
            .spawn_capability_grants
            .push((0, vec!["Net::TcpOut(api.example.com:443)".to_string()]));

        let profile = ExecutionProfile::analyze(&module);
        assert!(profile.requires(RuntimeFeature::ExternalAuthority));
        assert!(profile.requires(RuntimeFeature::Actors));
        assert!(profile.requires(RuntimeFeature::Heap));
        assert!(profile.reaches_host_boundary());
    }
}
