//! Canonical semantic identity for lowered Nulang programs.
//!
//! `SourceId` answers "did the source bytes change?" and `ArtifactId` answers
//! "did a backend/compiler configuration change?". This module closes the
//! missing middle layer: a `SemanticId` derived from backend-independent MIR.
//!
//! The encoding deliberately excludes debugger/source-location metadata and
//! alpha-normalizes compiler-generated type variables, regions, local IDs, and
//! block IDs. Equivalent programs therefore retain the same semantic identity
//! across formatting-only edits and fresh inference allocations, while changes
//! to executable behavior, types, effects, authority-bearing spawn grants, or
//! durable actor metadata change the identity.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::ast::{BinOp, CrdtType, StateModel, UnOp};
use crate::bytecode::{ActorMeta, Constant};
use crate::content_identity::SemanticId;
use crate::mir::{self, BlockId, FuncRef, LocalId, RValue, Stmt, Terminator};
use crate::types::{
    Capability, Effect, EffectRow, Placement, PrimitiveType, Region, Type, TypeVar,
};

const MIR_SEMANTIC_CANONICAL_VERSION: &[u8] = b"nulang.mir-semantic.v1\0";

/// Derive a compiler semantic identity from backend-independent MIR.
///
/// Dependency semantic IDs are folded in by [`SemanticId`] using set
/// semantics, so dependency traversal order and duplicate references do not
/// perturb the result.
pub fn semantic_id_for_mir<I>(
    module: &mir::Module,
    dependency_semantic_ids: I,
) -> Result<SemanticId, SemanticIdentityError>
where
    I: IntoIterator<Item = SemanticId>,
{
    let bytes = canonical_mir_bytes(module)?;
    Ok(SemanticId::from_canonical_bytes(
        &bytes,
        dependency_semantic_ids,
    ))
}

/// Produce the canonical MIR byte stream used by [`semantic_id_for_mir`].
pub fn canonical_mir_bytes(module: &mir::Module) -> Result<Vec<u8>, SemanticIdentityError> {
    let mut encoder = Encoder::default();
    encoder.bytes(MIR_SEMANTIC_CANONICAL_VERSION);
    encoder.string(&module.name);

    encoder.len(module.functions.len());
    for function in &module.functions {
        encoder.function(function)?;
    }

    encoder.len(module.behaviors.len());
    for behavior in &module.behaviors {
        encoder.function(behavior)?;
    }

    encoder.len(module.actor_metadata.len());
    for actor in &module.actor_metadata {
        encoder.actor_meta(actor);
    }

    // These are semantic lookup tables. Preserve emitted order: duplicate
    // entries must not become silently equivalent to a different MIR.
    encoder.len(module.compensation_of.len());
    for (step, compensation) in &module.compensation_of {
        encoder.usize(*step);
        encoder.usize(*compensation);
    }

    encoder.len(module.parallel_branches_of.len());
    for (behavior, branches) in &module.parallel_branches_of {
        encoder.usize(*behavior);
        encoder.len(branches.len());
        for branch in branches {
            encoder.string(branch);
        }
    }

    encoder.len(module.foreign_functions.len());
    for foreign in &module.foreign_functions {
        encoder.string(&foreign.library);
        encoder.string(&foreign.symbol);
        encoder.len(foreign.params.len());
        for param in &foreign.params {
            encoder.ty(param);
        }
        encoder.ty(&foreign.ret);
    }

    Ok(encoder.out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticIdentityError {
    UnterminatedBlock { function: String, block: u32 },
}

impl fmt::Display for SemanticIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedBlock { function, block } => write!(
                f,
                "cannot derive semantic identity: MIR function '{function}' contains unterminated block {block}"
            ),
        }
    }
}

impl Error for SemanticIdentityError {}

#[derive(Default)]
struct Encoder {
    out: Vec<u8>,
    type_vars: BTreeMap<u64, u32>,
    regions: BTreeMap<u64, u32>,
    skolems: BTreeMap<u64, u32>,
}

impl Encoder {
    fn byte(&mut self, value: u8) {
        self.out.push(value);
    }

    fn bool(&mut self, value: bool) {
        self.byte(u8::from(value));
    }

    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn usize(&mut self, value: usize) {
        self.u64(value as u64);
    }

    fn len(&mut self, value: usize) {
        self.u64(value as u64);
    }

    fn bytes(&mut self, value: &[u8]) {
        self.len(value.len());
        self.out.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn option<T>(&mut self, value: &Option<T>, f: impl FnOnce(&mut Self, &T)) {
        match value {
            Some(value) => {
                self.byte(1);
                f(self, value);
            }
            None => self.byte(0),
        }
    }

    fn canonical_index(map: &mut BTreeMap<u64, u32>, raw: u64) -> u32 {
        if let Some(index) = map.get(&raw) {
            return *index;
        }
        let index = map.len() as u32;
        map.insert(raw, index);
        index
    }

    fn type_var(&mut self, var: TypeVar) {
        let index = Self::canonical_index(&mut self.type_vars, var.0);
        self.u32(index);
    }

    fn region(&mut self, region: Region) {
        let index = Self::canonical_index(&mut self.regions, region.0);
        self.u32(index);
    }

    fn skolem(&mut self, raw: u64) {
        let index = Self::canonical_index(&mut self.skolems, raw);
        self.u32(index);
    }

    fn ty(&mut self, ty: &Type) {
        match ty {
            Type::Var(var) => {
                self.byte(0x00);
                self.type_var(*var);
            }
            Type::Primitive(primitive) => {
                self.byte(0x01);
                self.primitive(primitive);
            }
            Type::Tuple(types) => {
                self.byte(0x02);
                self.len(types.len());
                for ty in types {
                    self.ty(ty);
                }
            }
            Type::Record(fields) => {
                self.byte(0x03);
                let mut fields: Vec<_> = fields.iter().collect();
                fields.sort_by(|left, right| left.0.cmp(&right.0));
                self.len(fields.len());
                for (name, ty) in fields {
                    self.string(name);
                    self.ty(ty);
                }
            }
            Type::Variant(cases) => {
                self.byte(0x04);
                self.len(cases.len());
                for (name, payload) in cases {
                    self.string(name);
                    self.option(payload, |encoder, ty| encoder.ty(ty));
                }
            }
            Type::Array(inner) => {
                self.byte(0x05);
                self.ty(inner);
            }
            Type::Function {
                param,
                ret,
                effect,
                cap,
            } => {
                self.byte(0x06);
                self.ty(param);
                self.ty(ret);
                self.effect_row(effect);
                self.capability(*cap);
            }
            Type::Actor { state, behavior } => {
                self.byte(0x07);
                self.ty(state);
                self.ty(behavior);
            }
            Type::App { constructor, args } => {
                self.byte(0x08);
                self.ty(constructor);
                self.len(args.len());
                for arg in args {
                    self.ty(arg);
                }
            }
            Type::Reference { cap, inner } => {
                self.byte(0x09);
                self.capability(*cap);
                self.ty(inner);
            }
            Type::Scheme { vars, body } => {
                self.byte(0x0A);
                self.len(vars.len());
                // Allocate canonical binder identities in declaration order
                // before encoding the body, preserving equality relationships
                // while removing process-global TypeVar counter values.
                for var in vars {
                    self.type_var(*var);
                }
                self.ty(body);
            }
            Type::Nominal { name, underlying } => {
                self.byte(0x0B);
                self.string(name);
                self.ty(underlying);
            }
            Type::Skolem(id) => {
                self.byte(0x0C);
                self.skolem(*id);
            }
        }
    }

    fn primitive(&mut self, primitive: &PrimitiveType) {
        self.byte(match primitive {
            PrimitiveType::Int => 0,
            PrimitiveType::Float => 1,
            PrimitiveType::Bool => 2,
            PrimitiveType::String => 3,
            PrimitiveType::Nil => 4,
            PrimitiveType::Unit => 5,
            PrimitiveType::Never => 6,
            PrimitiveType::Address => 7,
        });
    }

    fn capability(&mut self, cap: Capability) {
        self.byte(match cap {
            Capability::LinearIso => 0,
            Capability::Linear => 1,
            Capability::Iso => 2,
            Capability::Trn => 3,
            Capability::Ref => 4,
            Capability::Val => 5,
            Capability::Box => 6,
            Capability::Tag => 7,
        });
    }

    fn effect_row(&mut self, row: &EffectRow) {
        match row {
            EffectRow::Closed(effects) => {
                self.byte(0);
                self.effects(effects);
            }
            EffectRow::Open(effects, region) => {
                self.byte(1);
                self.effects(effects);
                self.region(*region);
            }
        }
    }

    fn effects(&mut self, effects: &[Effect]) {
        let mut encoded: Vec<Vec<u8>> = effects
            .iter()
            .map(|effect| {
                let mut nested = Encoder::default();
                nested.effect(effect);
                nested.out
            })
            .collect();
        encoded.sort();
        encoded.dedup();
        self.len(encoded.len());
        for effect in encoded {
            self.bytes(&effect);
        }
    }

    fn effect(&mut self, effect: &Effect) {
        match effect {
            Effect::IO => self.byte(0),
            Effect::Net => self.byte(1),
            Effect::String => self.byte(2),
            Effect::FS => self.byte(3),
            Effect::Rand => self.byte(4),
            Effect::Time => self.byte(5),
            Effect::Spawn => self.byte(6),
            Effect::Send => self.byte(7),
            Effect::Receive => self.byte(8),
            Effect::Migrate => self.byte(9),
            Effect::STM => self.byte(10),
            Effect::Async => self.byte(11),
            Effect::Inference => self.byte(12),
            Effect::Cost => self.byte(13),
            Effect::Event => self.byte(14),
            Effect::Array => self.byte(15),
            Effect::FFI => self.byte(16),
            Effect::Test => self.byte(17),
            Effect::DB => self.byte(18),
            Effect::Python => self.byte(19),
            Effect::Env => self.byte(20),
            Effect::Process => self.byte(21),
            Effect::System => self.byte(22),
            Effect::Render => self.byte(23),
            Effect::Request => self.byte(24),
            Effect::Respond => self.byte(25),
            Effect::Realtime => self.byte(26),
            Effect::Client => self.byte(27),
            Effect::Web => self.byte(28),
            Effect::UserDefined(name) => {
                self.byte(29);
                self.string(name);
            }
        }
    }

    fn placement(&mut self, placement: Placement) {
        self.byte(match placement {
            Placement::Static => 0,
            Placement::Server => 1,
            Placement::Edge => 2,
            Placement::Client => 3,
            Placement::Actor => 4,
            Placement::Workflow => 5,
        });
    }

    fn binop(&mut self, op: BinOp) {
        self.byte(match op {
            BinOp::Add => 0,
            BinOp::Sub => 1,
            BinOp::Mul => 2,
            BinOp::Div => 3,
            BinOp::Mod => 4,
            BinOp::Eq => 5,
            BinOp::Ne => 6,
            BinOp::Lt => 7,
            BinOp::Le => 8,
            BinOp::Gt => 9,
            BinOp::Ge => 10,
            BinOp::And => 11,
            BinOp::Or => 12,
            BinOp::BitAnd => 13,
            BinOp::BitOr => 14,
            BinOp::BitXor => 15,
            BinOp::Shl => 16,
            BinOp::Shr => 17,
            BinOp::Pow => 18,
            BinOp::Assign => 19,
            BinOp::Range => 20,
            BinOp::Pipe => 21,
        });
    }

    fn unop(&mut self, op: UnOp) {
        match op {
            UnOp::Neg => self.byte(0),
            UnOp::Not => self.byte(1),
            UnOp::Deref => self.byte(2),
            UnOp::Ref(cap) => {
                self.byte(3);
                self.capability(cap);
            }
        }
    }

    fn function(&mut self, function: &mir::Function) -> Result<(), SemanticIdentityError> {
        self.string(&function.name);

        let ids = FunctionIds::new(function);
        self.len(function.params.len());
        for param in &function.params {
            self.u32(ids.local(*param));
        }
        self.len(function.captures.len());
        for capture in &function.captures {
            self.u32(ids.local(*capture));
        }
        self.option(&function.ret, |encoder, ty| encoder.ty(ty));
        self.option(&function.placement, |encoder, placement| {
            encoder.placement(*placement)
        });

        // Local names are debugger/source presentation. Local identity, type,
        // and capability are semantic; normalize IDs to declaration order.
        self.len(function.locals.len());
        for local in &function.locals {
            self.u32(ids.local(local.id));
            self.ty(&local.ty);
            self.capability(local.cap);
        }

        self.len(function.handler_tables.len());
        for table in &function.handler_tables {
            self.len(table.bindings.len());
            for binding in &table.bindings {
                self.string(&binding.effect_name);
                self.len(binding.params.len());
                for param in &binding.params {
                    self.u32(ids.local(*param));
                }
                self.bool(binding.resume);
                self.bool(binding.single_shot);
                self.u32(ids.block(binding.body));
            }
        }

        // `line_table` and `type_metadata` are deliberately excluded: both are
        // compiler/debugger metadata derived from the same semantics.
        self.len(function.blocks.len());
        for block in &function.blocks {
            self.u32(ids.block(block.id));
            self.len(block.stmts.len());
            for stmt in &block.stmts {
                self.stmt(stmt, &ids);
            }
            self.terminator(&function.name, block.id, &block.terminator, &ids)?;
        }
        self.u32(ids.block(function.entry));
        Ok(())
    }

    fn stmt(&mut self, stmt: &Stmt, ids: &FunctionIds) {
        match stmt {
            Stmt::Assign { dst, op } => {
                self.byte(0);
                self.u32(ids.local(*dst));
                self.rvalue(op, ids);
            }
            Stmt::StoreFieldNamed { obj, field, src } => {
                self.byte(1);
                self.u32(ids.local(*obj));
                self.string(field);
                self.u32(ids.local(*src));
            }
            Stmt::ArrayStore { arr, idx, src } => {
                self.byte(2);
                self.u32(ids.local(*arr));
                self.u32(ids.local(*idx));
                self.u32(ids.local(*src));
            }
            Stmt::EnterHandle { table } => {
                self.byte(3);
                self.usize(*table);
            }
            Stmt::PopHandler => self.byte(4),
            Stmt::Emit { event, args } => {
                self.byte(5);
                self.string(event);
                self.locals(args, ids);
            }
            Stmt::StateSet { field, src } => {
                self.byte(6);
                self.string(field);
                self.u32(ids.local(*src));
            }
        }
    }

    fn rvalue(&mut self, value: &RValue, ids: &FunctionIds) {
        match value {
            RValue::Const(value) => {
                self.byte(0);
                self.constant(value);
            }
            RValue::Panic(message) => {
                self.byte(1);
                self.string(message);
            }
            RValue::Load(local) => {
                self.byte(2);
                self.u32(ids.local(*local));
            }
            RValue::LoadFieldNamed { obj, field } => {
                self.byte(3);
                self.u32(ids.local(*obj));
                self.string(field);
            }
            RValue::LoadFieldPos { obj, index } => {
                self.byte(4);
                self.u32(ids.local(*obj));
                self.byte(*index);
            }
            RValue::ArrayLoad { arr, idx } => {
                self.byte(5);
                self.u32(ids.local(*arr));
                self.u32(ids.local(*idx));
            }
            RValue::ArrayLen(arr) => {
                self.byte(6);
                self.u32(ids.local(*arr));
            }
            RValue::ArrayLit(values) => {
                self.byte(7);
                self.locals(values, ids);
            }
            RValue::Unary(op, value) => {
                self.byte(8);
                self.unop(*op);
                self.u32(ids.local(*value));
            }
            RValue::Binary(op, left, right) => {
                self.byte(9);
                self.binop(*op);
                self.u32(ids.local(*left));
                self.u32(ids.local(*right));
            }
            RValue::StringEq(left, right) => {
                self.byte(10);
                self.u32(ids.local(*left));
                self.u32(ids.local(*right));
            }
            RValue::StrConcat(left, right) => {
                self.byte(11);
                self.u32(ids.local(*left));
                self.u32(ids.local(*right));
            }
            RValue::Call { func, args } => {
                self.byte(12);
                match func {
                    FuncRef::Index(index) => {
                        self.byte(0);
                        self.usize(*index);
                    }
                    FuncRef::Local(local) => {
                        self.byte(1);
                        self.u32(ids.local(*local));
                    }
                }
                self.locals(args, ids);
            }
            RValue::Closure { func, captures } => {
                self.byte(13);
                self.usize(*func);
                self.locals(captures, ids);
            }
            RValue::Tuple(values) => {
                self.byte(14);
                self.locals(values, ids);
            }
            RValue::Record(fields) => {
                self.byte(15);
                self.len(fields.len());
                for (field, value) in fields {
                    self.string(field);
                    self.u32(ids.local(*value));
                }
            }
            RValue::RecordUpdate { base, overrides } => {
                self.byte(16);
                self.u32(ids.local(*base));
                self.len(overrides.len());
                for (field, value) in overrides {
                    self.string(field);
                    self.u32(ids.local(*value));
                }
            }
            RValue::Perform {
                effect,
                op,
                args,
                resolved_handler,
            } => {
                self.byte(17);
                self.string(effect);
                self.string(op);
                self.locals(args, ids);
                self.handler_ref(*resolved_handler);
            }
            RValue::PerformAsync {
                effect_op,
                args,
                resolved_handler,
            } => {
                self.byte(18);
                self.string(effect_op);
                self.locals(args, ids);
                self.handler_ref(*resolved_handler);
            }
            RValue::SignalWait { name } => {
                self.byte(19);
                self.string(name);
            }
            RValue::Receive => self.byte(20),
            RValue::ReceiveMatch {
                behavior_ids,
                max_params,
            } => {
                self.byte(21);
                self.len(behavior_ids.len());
                for behavior in behavior_ids {
                    self.u32(u32::from(*behavior));
                }
                self.usize(*max_params);
            }
            RValue::ReceiveWait {
                behavior_ids,
                max_params,
                timeout,
            } => {
                self.byte(22);
                self.len(behavior_ids.len());
                for behavior in behavior_ids {
                    self.u32(u32::from(*behavior));
                }
                self.usize(*max_params);
                self.u32(ids.local(*timeout));
            }
            RValue::ReceiveCommit => self.byte(23),
            RValue::FFICall { idx, args } => {
                self.byte(24);
                self.usize(*idx);
                self.locals(args, ids);
            }
            RValue::Migrate { actor, node } => {
                self.byte(25);
                self.u32(ids.local(*actor));
                self.u32(ids.local(*node));
            }
            RValue::SelfRef => self.byte(26),
            RValue::CapabilityCheck { val } => {
                self.byte(27);
                self.u32(ids.local(*val));
            }
            RValue::StateGet { field } => {
                self.byte(28);
                self.string(field);
            }
            RValue::Spawn {
                behavior_idx,
                init,
                target_node,
                capabilities,
            } => {
                self.byte(29);
                self.usize(*behavior_idx);
                self.len(init.len());
                for (field, value) in init {
                    self.string(field);
                    self.rvalue(value, ids);
                }
                self.option(target_node, |encoder, local| encoder.u32(ids.local(*local)));
                self.len(capabilities.len());
                for capability in capabilities {
                    self.string(capability);
                }
            }
            RValue::Send {
                actor,
                behavior_idx,
                args,
                remote,
            } => {
                self.byte(30);
                self.u32(ids.local(*actor));
                self.usize(*behavior_idx);
                self.locals(args, ids);
                self.bool(*remote);
            }
            RValue::Resume(value) => {
                self.byte(31);
                self.u32(ids.local(*value));
            }
            RValue::Ask {
                actor,
                behavior_idx,
                args,
                remote,
                timeout_ms,
            } => {
                self.byte(32);
                self.u32(ids.local(*actor));
                self.usize(*behavior_idx);
                self.locals(args, ids);
                self.bool(*remote);
                self.option(timeout_ms, |encoder, timeout| encoder.u64(*timeout));
            }
        }
    }

    fn handler_ref(&mut self, handler: Option<mir::HandlerRef>) {
        self.option(&handler, |encoder, handler| {
            encoder.u32(handler.table_index);
            encoder.u32(handler.binding_index);
        });
    }

    fn terminator(
        &mut self,
        function: &str,
        raw_block: BlockId,
        terminator: &Terminator,
        ids: &FunctionIds,
    ) -> Result<(), SemanticIdentityError> {
        match terminator {
            Terminator::Return(value) => {
                self.byte(0);
                self.option(value, |encoder, local| encoder.u32(ids.local(*local)));
            }
            Terminator::Jump(block) => {
                self.byte(1);
                self.u32(ids.block(*block));
            }
            Terminator::Branch { cond, then_, else_ } => {
                self.byte(2);
                self.u32(ids.local(*cond));
                self.u32(ids.block(*then_));
                self.u32(ids.block(*else_));
            }
            Terminator::Resume(local) => {
                self.byte(3);
                self.u32(ids.local(*local));
            }
            Terminator::Unterminated => {
                return Err(SemanticIdentityError::UnterminatedBlock {
                    function: function.to_string(),
                    block: raw_block.0,
                });
            }
        }
        Ok(())
    }

    fn locals(&mut self, locals: &[LocalId], ids: &FunctionIds) {
        self.len(locals.len());
        for local in locals {
            self.u32(ids.local(*local));
        }
    }

    fn constant(&mut self, constant: &Constant) {
        match constant {
            Constant::Int(value) => {
                self.byte(0);
                self.i64(*value);
            }
            Constant::Float(value) => {
                self.byte(1);
                self.u64(value.to_bits());
            }
            Constant::String(value) => {
                self.byte(2);
                self.string(value);
            }
            Constant::Bool(value) => {
                self.byte(3);
                self.bool(*value);
            }
            Constant::Nil => self.byte(4),
            Constant::Unit => self.byte(5),
            Constant::TypeDescriptor(value) => {
                self.byte(6);
                self.string(value);
            }
            Constant::FunctionRef(index) => {
                self.byte(7);
                self.usize(*index);
            }
            Constant::BehaviorRef(index) => {
                self.byte(8);
                self.usize(*index);
            }
        }
    }

    fn actor_meta(&mut self, actor: &ActorMeta) {
        self.string(&actor.name);
        self.bool(actor.persistent);

        self.len(actor.state_models.len());
        for (name, model) in &actor.state_models {
            self.string(name);
            self.state_model(*model);
        }
        self.len(actor.state_defaults.len());
        for (name, default) in &actor.state_defaults {
            self.string(name);
            self.constant(default);
        }
        self.len(actor.behavior_indices.len());
        for behavior in &actor.behavior_indices {
            self.usize(*behavior);
        }

        self.bool(actor.is_workflow);
        self.bool(actor.is_agent);
        self.bool(actor.is_organization);
        self.bool(actor.is_virtual);

        self.len(actor.tools.len());
        for tool in &actor.tools {
            self.string(&tool.name);
            self.string(&tool.description);
            self.json(&tool.parameters);
        }

        self.option(&actor.semantic_memory_dimensions, |encoder, dimensions| {
            encoder.usize(*dimensions)
        });
        self.option(&actor.procedural_memory_namespace, |encoder, namespace| {
            encoder.string(namespace)
        });
        // `backend` is an artifact/code-generation choice, not program
        // semantics. It is therefore excluded here and folded into ArtifactId
        // by the artifact-identity layer.
        self.json_string_or_raw(&actor.fallback_config);
        self.json_string_or_raw(&actor.retry_config);

        // `type_hash` is derived NTIR metadata, not semantics itself. Including
        // it would make SemanticId depend on the chosen type-hash algorithm.
        self.u32(actor.version);
        self.json_string_or_raw(&actor.migrations);
    }

    fn state_model(&mut self, model: StateModel) {
        match model {
            StateModel::Local => self.byte(0),
            StateModel::Durable => self.byte(1),
            StateModel::EventSourced => self.byte(2),
            StateModel::Crdt(crdt) => {
                self.byte(3);
                self.crdt(crdt);
            }
        }
    }

    fn crdt(&mut self, crdt: CrdtType) {
        self.byte(match crdt {
            CrdtType::GCounter => 0,
            CrdtType::PNCounter => 1,
            CrdtType::GSet => 2,
            CrdtType::ORSet => 3,
            CrdtType::AWORSet => 4,
            CrdtType::LWWRegister => 5,
            CrdtType::MVRegister => 6,
            CrdtType::RGA => 7,
        });
    }

    fn json_string_or_raw(&mut self, value: &str) {
        if value.is_empty() {
            self.byte(0);
            return;
        }
        match serde_json::from_str::<serde_json::Value>(value) {
            Ok(json) => {
                self.byte(1);
                self.json(&json);
            }
            Err(_) => {
                self.byte(2);
                self.string(value);
            }
        }
    }

    fn json(&mut self, value: &serde_json::Value) {
        match value {
            serde_json::Value::Null => self.byte(0),
            serde_json::Value::Bool(value) => {
                self.byte(1);
                self.bool(*value);
            }
            serde_json::Value::Number(value) => {
                self.byte(2);
                self.string(&value.to_string());
            }
            serde_json::Value::String(value) => {
                self.byte(3);
                self.string(value);
            }
            serde_json::Value::Array(values) => {
                self.byte(4);
                self.len(values.len());
                for value in values {
                    self.json(value);
                }
            }
            serde_json::Value::Object(values) => {
                self.byte(5);
                let mut values: Vec<_> = values.iter().collect();
                values.sort_by(|left, right| left.0.cmp(right.0));
                self.len(values.len());
                for (key, value) in values {
                    self.string(key);
                    self.json(value);
                }
            }
        }
    }
}

/// Dense per-function normalization of compiler-generated IDs.
struct FunctionIds {
    locals: BTreeMap<u32, u32>,
    blocks: BTreeMap<u32, u32>,
}

impl FunctionIds {
    fn new(function: &mir::Function) -> Self {
        let mut local_ids: Vec<u32> = function.locals.iter().map(|local| local.id.0).collect();
        local_ids.sort_unstable();
        local_ids.dedup();
        let locals = local_ids
            .into_iter()
            .enumerate()
            .map(|(index, raw)| (raw, index as u32))
            .collect();

        let mut block_ids: Vec<u32> = function.blocks.iter().map(|block| block.id.0).collect();
        block_ids.sort_unstable();
        block_ids.dedup();
        let blocks = block_ids
            .into_iter()
            .enumerate()
            .map(|(index, raw)| (raw, index as u32))
            .collect();

        Self { locals, blocks }
    }

    fn local(&self, id: LocalId) -> u32 {
        self.locals.get(&id.0).copied().unwrap_or(id.0)
    }

    fn block(&self, id: BlockId) -> u32 {
        self.blocks.get(&id.0).copied().unwrap_or(id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::ActorBackendKind;
    use crate::bytecode::ActorMeta;
    use crate::hir_lower;
    use crate::lexer::Lexer;
    use crate::mir_lower;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;

    fn lower(source: &str) -> mir::Module {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().unwrap();
        let mut typechecker = TypeChecker::new();
        typechecker.check_module(&ast).unwrap();
        let hir = hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
        mir_lower::lower_module(&hir).unwrap()
    }

    #[test]
    fn formatting_comments_and_source_lines_do_not_change_semantic_id() {
        let compact = lower("fn add(x: Int, y: Int) -> Int { x + y }\nadd(20, 22)");
        let formatted = lower(
            "// presentation-only comment\n\nfn add(x: Int, y: Int) -> Int {\n    x + y\n}\n\nadd(20, 22)\n",
        );
        assert_eq!(
            semantic_id_for_mir(&compact, []).unwrap(),
            semantic_id_for_mir(&formatted, []).unwrap()
        );
    }

    #[test]
    fn executable_change_changes_semantic_id() {
        let forty_two = lower("fn answer() -> Int { 42 }\nanswer()");
        let forty_three = lower("fn answer() -> Int { 43 }\nanswer()");
        assert_ne!(
            semantic_id_for_mir(&forty_two, []).unwrap(),
            semantic_id_for_mir(&forty_three, []).unwrap()
        );
    }

    #[test]
    fn fresh_type_variable_numbers_are_alpha_normalized() {
        // TypeVar::fresh is process-global. Lowering the same generic program
        // twice therefore allocates different raw IDs; SemanticId must not.
        let first = lower("fn id[T](x: T) -> T { x }\nid(42)");
        let second = lower("fn id[T](x: T) -> T { x }\nid(42)");
        assert_eq!(
            semantic_id_for_mir(&first, []).unwrap(),
            semantic_id_for_mir(&second, []).unwrap()
        );
    }

    #[test]
    fn dependency_order_and_duplicates_do_not_change_semantic_id() {
        let module = lower("42");
        let a = SemanticId::from_canonical_bytes(b"a", []);
        let b = SemanticId::from_canonical_bytes(b"b", []);
        assert_eq!(
            semantic_id_for_mir(&module, [a, b]).unwrap(),
            semantic_id_for_mir(&module, [b, a, b]).unwrap()
        );
    }

    #[test]
    fn actor_backend_does_not_change_mir_semantic_id() {
        let mut native = mir::Module::new("backend-test");
        native.actor_metadata.push(ActorMeta::new("Worker"));
        let mut wasm = native.clone();
        wasm.actor_metadata[0].backend = ActorBackendKind::WasmComponent;

        assert_eq!(
            semantic_id_for_mir(&native, []).unwrap(),
            semantic_id_for_mir(&wasm, []).unwrap()
        );
    }
}
