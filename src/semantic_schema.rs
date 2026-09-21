//! Canonical compiler-owned semantic schema inputs that are not fully carried by MIR.
//!
//! MIR intentionally excludes some source/type-system structure that does not belong in
//! runtime bytecode metadata. Actor state field types are one such example: runtime
//! `ActorMeta` keeps persistence/default information plus a derived compatibility hash,
//! while semantic identity should be defined from canonical typed inputs themselves.
//!
//! This module derives a stable actor-state schema sidecar from typed HIR and combines it
//! with canonical MIR bytes to produce the full typed-program [`SemanticId`].

use std::collections::BTreeMap;

use crate::ast::ActorBackendKind;
use crate::content_identity::SemanticId;
use crate::hir;
use crate::mir;
use crate::semantic_identity::{canonical_mir_bytes, SemanticIdentityError};
use crate::types::{Capability, Effect, EffectRow, PrimitiveType, Region, Type, TypeVar};

const TYPED_PROGRAM_SEMANTIC_VERSION: &[u8] = b"nulang.typed-program-semantic.v1\0";
const ACTOR_SCHEMA_CANONICAL_VERSION: &[u8] = b"nulang.actor-state-schema.v1\0";

/// Canonical compiler-owned state schema for one actor/entity/workflow/agent.
///
/// `actor_name` is the fully-qualified HIR nominal path (`module::...::Actor`),
/// so actors with the same short name in different namespaces remain distinct.
/// Field order is not semantic: actor state is addressed by field name, so the
/// canonical encoder sorts fields by name before hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorStateSchema {
    pub actor_name: String,
    pub fields: Vec<ActorStateField>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorStateField {
    pub name: String,
    pub ty: Type,
}

/// Extract actor state schemas from typed HIR, including actors nested inside
/// namespace modules and actors produced by workflow/agent desugaring.
///
/// Actor names are qualified from the root HIR module through every nested
/// module so nominal identity cannot collapse two distinct actors that happen
/// to share a short name.
pub fn actor_state_schemas_from_hir(module: &hir::Module) -> Vec<ActorStateSchema> {
    let mut schemas = Vec::new();
    let mut namespace = Vec::new();
    if !module.name.is_empty() {
        namespace.push(module.name.clone());
    }
    collect_decl_schemas(&module.decls, &mut namespace, &mut schemas);
    schemas.sort_by(|left, right| left.actor_name.cmp(&right.actor_name));
    schemas
}

fn collect_decl_schemas(
    decls: &[hir::Decl],
    namespace: &mut Vec<String>,
    out: &mut Vec<ActorStateSchema>,
) {
    for decl in decls {
        match decl {
            hir::Decl::Actor(actor) => {
                let actor_name = if namespace.is_empty() {
                    actor.name.clone()
                } else {
                    format!("{}::{}", namespace.join("::"), actor.name)
                };
                out.push(ActorStateSchema {
                    actor_name,
                    fields: actor
                        .state_fields
                        .iter()
                        .map(|(name, _model, ty, _default)| ActorStateField {
                            name: name.clone(),
                            ty: ty.clone(),
                        })
                        .collect(),
                });
            }
            hir::Decl::Module { name, decls, .. } => {
                namespace.push(name.clone());
                collect_decl_schemas(decls, namespace, out);
                namespace.pop();
            }
            _ => {}
        }
    }
}

/// Produce the canonical semantic byte stream for actor state schemas.
pub fn canonical_actor_state_schema_bytes(schemas: &[ActorStateSchema]) -> Vec<u8> {
    let mut encoder = TypeSchemaEncoder::default();
    encoder.bytes(ACTOR_SCHEMA_CANONICAL_VERSION);

    let mut schemas: Vec<_> = schemas.iter().collect();
    schemas.sort_by(|left, right| left.actor_name.cmp(&right.actor_name));

    encoder.len(schemas.len());
    for schema in schemas {
        encoder.string(&schema.actor_name);

        let mut fields: Vec<_> = schema.fields.iter().collect();
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        encoder.len(fields.len());
        for field in fields {
            encoder.string(&field.name);
            encoder.ty(&field.ty);
        }
    }

    encoder.out
}

/// Derive the full compiler semantic identity from typed HIR plus
/// backend-independent MIR semantics.
///
/// Use this rather than MIR-only identity when typed HIR is available. The MIR
/// component covers executable/control-flow/effect/authority semantics; the
/// schema sidecar closes type-only actor-state changes that do not necessarily
/// alter generated behavior bodies or runtime defaults.
pub fn semantic_id_for_typed_program<I>(
    hir: &hir::Module,
    mir: &mir::Module,
    dependency_semantic_ids: I,
) -> Result<SemanticId, SemanticIdentityError>
where
    I: IntoIterator<Item = SemanticId>,
{
    let schemas = actor_state_schemas_from_hir(hir);
    semantic_id_for_mir_with_actor_schemas(mir, &schemas, dependency_semantic_ids)
}

/// Lower-level entry point for callers that already own canonical actor schemas.
///
/// `ActorMeta::backend` is intentionally normalized out here: selecting native
/// versus Wasm execution changes the compiled artifact, not the program's
/// semantics. Backend/compiler configuration belongs in `ArtifactId`.
pub fn semantic_id_for_mir_with_actor_schemas<I>(
    mir: &mir::Module,
    schemas: &[ActorStateSchema],
    dependency_semantic_ids: I,
) -> Result<SemanticId, SemanticIdentityError>
where
    I: IntoIterator<Item = SemanticId>,
{
    let mut semantic_mir = mir.clone();
    for actor in &mut semantic_mir.actor_metadata {
        actor.backend = ActorBackendKind::Native;
    }

    let mir_bytes = canonical_mir_bytes(&semantic_mir)?;
    let schema_bytes = canonical_actor_state_schema_bytes(schemas);

    let mut bytes = Vec::new();
    put_bytes(&mut bytes, TYPED_PROGRAM_SEMANTIC_VERSION);
    put_bytes(&mut bytes, &mir_bytes);
    put_bytes(&mut bytes, &schema_bytes);

    Ok(SemanticId::from_canonical_bytes(
        &bytes,
        dependency_semantic_ids,
    ))
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    put_u64(out, value.len() as u64);
    out.extend_from_slice(value);
}

#[derive(Default)]
struct TypeSchemaEncoder {
    out: Vec<u8>,
    type_vars: BTreeMap<u64, u32>,
    regions: BTreeMap<u64, u32>,
    skolems: BTreeMap<u64, u32>,
}

impl TypeSchemaEncoder {
    fn byte(&mut self, value: u8) {
        self.out.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
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

    fn capability(&mut self, capability: Capability) {
        self.byte(match capability {
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
                let mut nested = TypeSchemaEncoder::default();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Literal, StateModel};
    use crate::bytecode::ActorMeta;
    use crate::types::{PrimitiveType, Span};

    fn primitive(primitive: PrimitiveType) -> Type {
        Type::Primitive(primitive)
    }

    fn schema(field_ty: Type) -> ActorStateSchema {
        ActorStateSchema {
            actor_name: "Counter".to_string(),
            fields: vec![ActorStateField {
                name: "value".to_string(),
                ty: field_ty,
            }],
        }
    }

    fn actor_def(name: &str, field_name: &str, field_ty: Type) -> hir::ActorDef {
        hir::ActorDef {
            name: name.to_string(),
            type_params: Vec::new(),
            persistent: true,
            state_fields: vec![(
                field_name.to_string(),
                StateModel::Durable,
                field_ty.clone(),
                hir::Operand::Literal(Literal::Int(0), primitive(PrimitiveType::Int)),
            )],
            behaviors: Vec::new(),
            init: Vec::new(),
            events: Vec::new(),
            apply_handlers: Vec::new(),
            version: 1,
            migrations: Vec::new(),
            is_organization: false,
            is_workflow: false,
            is_agent: false,
            virtual_: false,
            tools: Vec::new(),
            semantic_memory_dimensions: None,
            procedural_memory_namespace: None,
            fallback_config: String::new(),
            retry_config: String::new(),
            span: Span::default(),
        }
    }

    #[test]
    fn actor_schema_field_order_is_canonical() {
        let left = ActorStateSchema {
            actor_name: "Pair".to_string(),
            fields: vec![
                ActorStateField {
                    name: "b".to_string(),
                    ty: primitive(PrimitiveType::String),
                },
                ActorStateField {
                    name: "a".to_string(),
                    ty: primitive(PrimitiveType::Int),
                },
            ],
        };
        let right = ActorStateSchema {
            actor_name: "Pair".to_string(),
            fields: vec![left.fields[1].clone(), left.fields[0].clone()],
        };

        assert_eq!(
            canonical_actor_state_schema_bytes(&[left]),
            canonical_actor_state_schema_bytes(&[right])
        );
    }

    #[test]
    fn actor_state_type_change_changes_typed_program_identity() {
        let mir = mir::Module::new("schema-test");
        let int_schema = schema(primitive(PrimitiveType::Int));
        let string_schema = schema(primitive(PrimitiveType::String));

        let int_id = semantic_id_for_mir_with_actor_schemas(&mir, &[int_schema], []).unwrap();
        let string_id =
            semantic_id_for_mir_with_actor_schemas(&mir, &[string_schema], []).unwrap();

        assert_ne!(int_id, string_id);
    }

    #[test]
    fn actor_backend_is_artifact_identity_not_semantic_identity() {
        let mut native = mir::Module::new("backend-test");
        native.actor_metadata.push(ActorMeta::new("Worker"));
        let mut wasm = native.clone();
        wasm.actor_metadata[0].backend = ActorBackendKind::WasmComponent;

        let native_id = semantic_id_for_mir_with_actor_schemas(&native, &[], []).unwrap();
        let wasm_id = semantic_id_for_mir_with_actor_schemas(&wasm, &[], []).unwrap();

        assert_eq!(native_id, wasm_id);
    }

    #[test]
    fn actor_schema_type_variables_are_alpha_normalized() {
        let first = schema(Type::Var(TypeVar::fresh()));
        let second = schema(Type::Var(TypeVar::fresh()));

        assert_eq!(
            canonical_actor_state_schema_bytes(&[first]),
            canonical_actor_state_schema_bytes(&[second])
        );
    }

    #[test]
    fn nested_actor_short_names_preserve_nominal_module_identity() {
        let field_ty = primitive(PrimitiveType::Int);
        let module = hir::Module {
            name: "typed".to_string(),
            decls: vec![
                hir::Decl::Module {
                    name: "Billing".to_string(),
                    exports: Vec::new(),
                    decls: vec![hir::Decl::Actor(actor_def(
                        "Counter",
                        "value",
                        field_ty.clone(),
                    ))],
                    span: Span::default(),
                },
                hir::Decl::Module {
                    name: "Inventory".to_string(),
                    exports: Vec::new(),
                    decls: vec![hir::Decl::Actor(actor_def(
                        "Counter",
                        "value",
                        field_ty,
                    ))],
                    span: Span::default(),
                },
            ],
        };

        let schemas = actor_state_schemas_from_hir(&module);
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0].actor_name, "typed::Billing::Counter");
        assert_eq!(schemas[1].actor_name, "typed::Inventory::Counter");
        assert_ne!(
            canonical_actor_state_schema_bytes(&[schemas[0].clone()]),
            canonical_actor_state_schema_bytes(&[schemas[1].clone()])
        );
    }

    #[test]
    fn typed_hir_extraction_keeps_state_types_not_defaults_or_spans() {
        let field_ty = Type::Nominal {
            name: "UserId".to_string(),
            underlying: Box::new(primitive(PrimitiveType::Int)),
        };
        let actor = actor_def("Account", "owner", field_ty.clone());
        let module = hir::Module {
            name: "typed".to_string(),
            decls: vec![hir::Decl::Actor(actor)],
        };

        let schemas = actor_state_schemas_from_hir(&module);
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].actor_name, "typed::Account");
        assert_eq!(schemas[0].fields.len(), 1);
        assert_eq!(schemas[0].fields[0].name, "owner");
        assert_eq!(schemas[0].fields[0].ty, field_ty);
    }
}
