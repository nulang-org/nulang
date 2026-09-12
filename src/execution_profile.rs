//! Static runtime-feature analysis for compiled Nulang modules.
//!
//! Nulang supports actors, durability, algebraic effects, distribution, Python,
//! FFI, and host I/O. Most programs need only a subset. This module derives a
//! conservative profile directly from a [`CodeModule`] so packaging, Nulang
//! Cloud, sandboxing, and future selective linking can make decisions from the
//! compiled artifact rather than re-parsing source.
//!
//! The analysis is intentionally conservative: false positives cost footprint;
//! false negatives could remove a required runtime/security boundary. When in
//! doubt, classify the feature as required.

use std::collections::BTreeSet;

use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};

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
    /// The module can cross an external authority/host boundary.
    ExternalAuthority,
}

impl RuntimeFeature {
    /// Stable wire/display name for deployment manifests and policy engines.
    pub const fn as_str(self) -> &'static str {
        match self {
            RuntimeFeature::Heap => "heap",
            RuntimeFeature::Actors => "actors",
            RuntimeFeature::Persistence => "persistence",
            RuntimeFeature::Effects => "effects",
            RuntimeFeature::AsyncEffects => "async_effects",
            RuntimeFeature::Distribution => "distribution",
            RuntimeFeature::PythonInterop => "python_interop",
            RuntimeFeature::Ffi => "ffi",
            RuntimeFeature::FileIo => "file_io",
            RuntimeFeature::StdIo => "stdio",
            RuntimeFeature::Debug => "debug",
            RuntimeFeature::ExternalAuthority => "external_authority",
        }
    }
}

/// Conservative feature and authority requirements for one compiled module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionProfile {
    features: BTreeSet<RuntimeFeature>,
    /// Fully-qualified unhandled/suspending effect names found in bytecode,
    /// e.g. `DB.query` or `LLM.ask`. `PerformDirect` is intentionally excluded:
    /// it is statically bound to a user handler inside the compiled module and
    /// therefore does not cross the host boundary.
    external_effects: BTreeSet<String>,
}

impl ExecutionProfile {
    /// Derive runtime and host-boundary requirements from bytecode + metadata.
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
            profile.require(RuntimeFeature::ExternalAuthority);
        }
        if !module.spawn_capability_grants.is_empty() {
            profile.require(RuntimeFeature::ExternalAuthority);
            profile.require(RuntimeFeature::Actors);
        }

        for instruction in &module.instructions {
            profile.classify_instruction(module, instruction);
        }

        if !profile.external_effects.is_empty() {
            profile.require(RuntimeFeature::ExternalAuthority);
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

    /// Stable deployment-manifest names for all required features.
    pub fn feature_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.features.iter().copied().map(RuntimeFeature::as_str)
    }

    /// External effect operations encoded in this compiled module.
    pub fn external_effects(&self) -> impl Iterator<Item = &str> + '_ {
        self.external_effects.iter().map(String::as_str)
    }

    /// Root effect namespaces for external operations (`DB.query` -> `DB`).
    pub fn external_effect_roots(&self) -> Vec<String> {
        let mut roots = BTreeSet::new();
        for effect in &self.external_effects {
            let root = effect.split_once('.').map_or(effect.as_str(), |(root, _)| root);
            roots.insert(root.to_string());
        }
        roots.into_iter().collect()
    }

    /// Number of runtime feature groups required by this module.
    pub fn len(&self) -> usize {
        self.features.len()
    }

    pub fn is_empty(&self) -> bool {
        self.features.is_empty() && self.external_effects.is_empty()
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
        self.requires(RuntimeFeature::ExternalAuthority)
            || [
                RuntimeFeature::Distribution,
                RuntimeFeature::PythonInterop,
                RuntimeFeature::Ffi,
                RuntimeFeature::FileIo,
                RuntimeFeature::StdIo,
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

    fn classify_instruction(&mut self, module: &CodeModule, instruction: &Instruction) {
        self.classify_opcode(instruction.opcode);

        if matches!(instruction.opcode, OpCode::Perform | OpCode::PerformAsync) {
            if let Some(name) = effect_name(module, instruction) {
                self.external_effects.insert(name.to_string());
            }
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
                self.require(RuntimeFeature::ExternalAuthority);
            }

            FFICall => {
                self.require(RuntimeFeature::Ffi);
                self.require(RuntimeFeature::ExternalAuthority);
            }

            NodeId | Migrate | RSend | RAsk | RSpawn | Gossip => {
                self.require(RuntimeFeature::Distribution);
                self.require(RuntimeFeature::Actors);
                self.require(RuntimeFeature::ExternalAuthority);
            }

            FOpen | FRead | FWrite | FClose => {
                self.require(RuntimeFeature::FileIo);
                self.require(RuntimeFeature::ExternalAuthority);
            }

            SPrint | SRead | Print => {
                self.require(RuntimeFeature::StdIo);
                self.require(RuntimeFeature::ExternalAuthority);
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

/// Resolve the effect-operation constant referenced by `Perform` / `PerformAsync`.
fn effect_name<'a>(module: &'a CodeModule, instruction: &Instruction) -> Option<&'a str> {
    let idx = instruction.imm16() as usize;
    match module.constants.get(idx) {
        Some(Constant::String(name)) => Some(name.as_str()),
        _ => None,
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

    fn emit_effect(module: &mut CodeModule, opcode: OpCode, name: &str) {
        let idx = module.add_constant(Constant::String(name.to_string()));
        module.emit(Instruction::new3(
            opcode,
            ((idx >> 8) & 0xff) as u8,
            (idx & 0xff) as u8,
            0,
        ));
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
        assert!(profile.requires(RuntimeFeature::ExternalAuthority));
        assert!(profile.is_ephemeral());
        assert!(!profile.is_local_only());
        assert!(profile.reaches_host_boundary());
    }

    #[test]
    fn async_effects_imply_effect_runtime() {
        let mut module = CodeModule::new("profile-test");
        emit_effect(&mut module, OpCode::PerformAsync, "LLM.ask");
        let profile = ExecutionProfile::analyze(&module);
        assert!(profile.requires(RuntimeFeature::AsyncEffects));
        assert!(profile.requires(RuntimeFeature::Effects));
        assert!(profile.requires(RuntimeFeature::ExternalAuthority));
        assert_eq!(profile.external_effects().collect::<Vec<_>>(), vec!["LLM.ask"]);
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
        assert!(profile.requires(RuntimeFeature::ExternalAuthority));
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

    #[test]
    fn external_effects_are_derived_from_compiled_bytecode() {
        let mut module = CodeModule::new("profile-test");
        emit_effect(&mut module, OpCode::Perform, "DB.query");
        emit_effect(&mut module, OpCode::PerformAsync, "Net.fetch");

        let profile = ExecutionProfile::analyze(&module);
        assert_eq!(
            profile.external_effects().collect::<Vec<_>>(),
            vec!["DB.query", "Net.fetch"]
        );
        assert_eq!(profile.external_effect_roots(), vec!["DB", "Net"]);
        assert!(profile.reaches_host_boundary());
    }

    #[test]
    fn perform_direct_is_internal_not_external_authority() {
        let module = module_with(&[OpCode::PerformDirect]);
        let profile = ExecutionProfile::analyze(&module);

        assert!(profile.requires(RuntimeFeature::Effects));
        assert!(profile.external_effects().next().is_none());
        assert!(!profile.requires(RuntimeFeature::ExternalAuthority));
        assert!(!profile.reaches_host_boundary());
    }

    #[test]
    fn feature_names_are_stable_for_manifests() {
        let module = module_with(&[OpCode::RSend]);
        let profile = ExecutionProfile::analyze(&module);
        let names = profile.feature_names().collect::<Vec<_>>();
        assert!(names.contains(&"actors"));
        assert!(names.contains(&"distribution"));
        assert!(names.contains(&"external_authority"));
        assert!(names.contains(&"heap"));
    }
}
