//! Experimental, compiler-owned public API interface for typed HIR.
//!
//! This is an additive sidecar, not an ABI, import-resolution rule, or a
//! persistence/wire format. It uses the existing alpha-normalizing type
//! encoder so fresh inference IDs cannot change a signature fingerprint.
//!
//! The first slice covers public functions and types, plus explicit nested
//! module exports. Actor/effect/database exports require dedicated contracts
//! and deliberately fail closed rather than silently omit a promised export.

use crate::hir::{self, Decl};
use crate::semantic_identity::canonical_public_type_bytes;
use crate::types::Type;
use serde::Serialize;

const SCHEMA_VERSION: u32 = 1;
const DECL_DOMAIN: &[u8] = b"nulang.public-declaration.v1\0";
const MODULE_DOMAIN: &[u8] = b"nulang.public-module-interface.v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicKind {
    Function,
    TypeAlias,
    OpaqueType,
    RecordType,
    VariantType,
}

impl PublicKind {
    fn tag(self) -> u8 {
        match self {
            Self::Function => 0,
            Self::TypeAlias => 1,
            Self::OpaqueType => 2,
            Self::RecordType => 3,
            Self::VariantType => 4,
        }
    }
}

/// Presentation metadata is not used when computing either digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublicExport {
    pub path: String,
    pub kind: PublicKind,
    pub signature: String,
    pub signature_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublicModuleInterface {
    pub schema_version: u32,
    pub module: String,
    pub exports: Vec<PublicExport>,
    /// Source-level API digest; NOT a native ABI or artifact identity.
    pub api_digest: String,
}

impl PublicModuleInterface {
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

struct EncodedExport {
    export: PublicExport,
    canonical: Vec<u8>,
}

fn part(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn add(
    out: &mut Vec<EncodedExport>,
    path: String,
    kind: PublicKind,
    ty: Type,
    generic_arity: usize,
    argument_names: &[String],
    explicit_arg_count: usize,
) {
    let mut canonical = DECL_DOMAIN.to_vec();
    part(&mut canonical, path.as_bytes());
    canonical.push(kind.tag());
    canonical.extend_from_slice(&(generic_arity as u64).to_le_bytes());
    canonical.extend_from_slice(&(explicit_arg_count as u64).to_le_bytes());
    canonical.extend_from_slice(&(argument_names.len() as u64).to_le_bytes());
    for name in argument_names {
        part(&mut canonical, name.as_bytes());
    }
    part(&mut canonical, &canonical_public_type_bytes(&ty));

    let export = PublicExport {
        path,
        kind,
        signature: ty.to_string(),
        signature_digest: blake3::hash(&canonical).to_hex().to_string(),
    };
    out.push(EncodedExport { export, canonical });
}

fn allowed(name: &str, public: bool, selected: Option<&[String]>) -> bool {
    match selected {
        Some(names) => names.iter().any(|candidate| candidate == name),
        None => public,
    }
}

fn qualified(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        return name.to_string();
    }
    format!("{}::{name}", scope.join("::"))
}

fn collect(
    decls: &[Decl],
    scope: &mut Vec<String>,
    selected: Option<&[String]>,
    out: &mut Vec<EncodedExport>,
) -> Result<(), String> {
    if let Some(names) = selected {
        for name in names {
            if !decls.iter().any(|decl| matches_name(decl, name)) {
                return Err(format!(
                    "public interface: module '{}' exports unknown declaration '{}'",
                    scope.join("::"),
                    name
                ));
            }
        }
    }

    for decl in decls {
        match decl {
            Decl::Function(func) if allowed(&func.name, func.public, selected) => {
                let types = func
                    .params
                    .iter()
                    .chain(func.dict_params.iter())
                    .map(|(_, ty)| ty.clone())
                    .collect();
                let names: Vec<String> = func
                    .params
                    .iter()
                    .chain(func.dict_params.iter())
                    .map(|(name, _)| name.clone())
                    .collect();
                let signature = Type::Function {
                    param: Box::new(Type::Tuple(types)),
                    ret: Box::new(func.ret.clone()),
                    effect: func.effect.clone(),
                    cap: func.cap,
                };
                add(
                    out,
                    qualified(scope, &func.name),
                    PublicKind::Function,
                    signature,
                    func.type_params.len(),
                    &names,
                    func.params.len(),
                );
            }
            Decl::TypeAlias {
                name,
                type_params,
                body,
                opaque,
                public,
                ..
            } if allowed(name, *public, selected) => {
                add(
                    out,
                    qualified(scope, name),
                    if *opaque {
                        PublicKind::OpaqueType
                    } else {
                        PublicKind::TypeAlias
                    },
                    if *opaque {
                        Type::Nominal {
                            name: qualified(scope, name),
                            underlying: Box::new(Type::unit()),
                        }
                    } else {
                        body.clone()
                    },
                    type_params.len(),
                    &[],
                    0,
                );
            }
            Decl::RecordType {
                name,
                type_params,
                fields,
                public,
                ..
            } if allowed(name, *public, selected) => {
                add(
                    out,
                    qualified(scope, name),
                    PublicKind::RecordType,
                    Type::Record(fields.clone()),
                    type_params.len(),
                    &[],
                    0,
                );
            }
            Decl::VariantType {
                name,
                type_params,
                variants,
                public,
                ..
            } if allowed(name, *public, selected) => {
                add(
                    out,
                    qualified(scope, name),
                    PublicKind::VariantType,
                    Type::Variant(variants.clone()),
                    type_params.len(),
                    &[],
                    0,
                );
            }
            Decl::Module {
                name,
                exports,
                decls,
                ..
            } if selected.is_none() || allowed(name, false, selected) => {
                scope.push(name.clone());
                let result = collect(decls, scope, Some(exports), out);
                scope.pop();
                result?;
            }
            // In a nested module the export list is authoritative. Do not
            // produce a partial digest for an unsupported exported kind.
            other if selected.is_some_and(|names| {
                names.iter().any(|name| matches_name(other, name))
            }) => {
                return Err(format!(
                    "public interface: unsupported exported declaration in module '{}'",
                    scope.join("::")
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn matches_name(decl: &Decl, expected: &str) -> bool {
    match decl {
        Decl::Function(f) => f.name == expected,
        Decl::Actor(a) => a.name == expected,
        Decl::TypeAlias { name, .. }
        | Decl::RecordType { name, .. }
        | Decl::VariantType { name, .. }
        | Decl::Module { name, .. }
        | Decl::EffectDecl { name, .. }
        | Decl::Database { name, .. }
        | Decl::Workflow { name, .. }
        | Decl::Agent { name, .. }
        | Decl::Constant { name, .. }
        | Decl::CrdtDecl { name, .. } => name == expected,
        Decl::Import { .. } | Decl::ExternBlock { .. } => false,
    }
}

/// Generate a deterministic public-API sidecar from already-checked typed HIR.
///
/// Callers must supply the *declaring module's* HIR. Imported declarations
/// flattened into the supplied HIR cannot yet be distinguished from local
/// declarations; import-provenance integration is separate stabilization work.
pub fn interface_for_hir(module: &hir::Module) -> Result<PublicModuleInterface, String> {
    let mut entries = Vec::new();
    let mut scope = if module.name.is_empty() {
        Vec::new()
    } else {
        vec![module.name.clone()]
    };
    collect(&module.decls, &mut scope, None, &mut entries)?;
    entries.sort_by(|a, b| {
        a.export
            .path
            .cmp(&b.export.path)
            .then(a.export.kind.cmp(&b.export.kind))
    });
    for window in entries.windows(2) {
        if window[0].export.path == window[1].export.path {
            return Err(format!(
                "public interface: duplicate exported declaration '{}'",
                window[0].export.path
            ));
        }
    }

    let mut canonical = MODULE_DOMAIN.to_vec();
    canonical.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    part(&mut canonical, module.name.as_bytes());
    canonical.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for entry in &entries {
        part(&mut canonical, &entry.canonical);
    }
    Ok(PublicModuleInterface {
        schema_version: SCHEMA_VERSION,
        module: module.name.clone(),
        exports: entries.into_iter().map(|entry| entry.export).collect(),
        api_digest: blake3::hash(&canonical).to_hex().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Capability, Effect, EffectRow, PrimitiveType, Span, TypeVar};

    fn primitive(p: PrimitiveType) -> Type {
        Type::Primitive(p)
    }

    fn function(
        name: &str,
        public: bool,
        parameter: Type,
        result: Type,
        effects: Vec<Effect>,
    ) -> Decl {
        Decl::Function(hir::FunctionDef {
            name: name.to_string(),
            type_params: Vec::new(),
            params: vec![("value".to_string(), parameter)],
            dict_params: Vec::new(),
            ret: result,
            effect: EffectRow::Closed(effects),
            cap: Capability::Ref,
            body: hir::Body::default(),
            public,
            placement: None,
            span: Span::default(),
        })
    }

    fn module(decls: Vec<Decl>) -> hir::Module {
        hir::Module { name: "test".to_string(), decls }
    }

    #[test]
    fn private_declarations_and_declaration_order_do_not_change_api_digest() {
        let int = primitive(PrimitiveType::Int);
        let exported = function("exported", true, int.clone(), int.clone(), vec![]);
        let private = function(
            "private",
            false,
            int.clone(),
            primitive(PrimitiveType::String),
            vec![],
        );
        let first = interface_for_hir(&module(vec![exported.clone()])).unwrap();
        let second = interface_for_hir(&module(vec![private, exported])).unwrap();
        assert_eq!(first.api_digest, second.api_digest);
        assert_eq!(first.exports, second.exports);
    }

    #[test]
    fn order_of_multiple_public_declarations_is_not_part_of_the_interface() {
        let int = primitive(PrimitiveType::Int);
        let a = function("a", true, int.clone(), int.clone(), vec![]);
        let z = function("z", true, int.clone(), int, vec![]);
        let first = interface_for_hir(&module(vec![a.clone(), z.clone()])).unwrap();
        let second = interface_for_hir(&module(vec![z, a])).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.exports[0].path, "test::a");
        assert_eq!(first.exports[1].path, "test::z");
    }

    #[test]
    fn public_parameter_name_changes_signature_digest() {
        let int = primitive(PrimitiveType::Int);
        let before = function("lookup", true, int.clone(), int, vec![]);
        let mut after = before.clone();
        if let Decl::Function(ref mut f) = after {
            f.params[0].0 = "id".to_string();
        }
        assert_ne!(
            interface_for_hir(&module(vec![before])).unwrap().api_digest,
            interface_for_hir(&module(vec![after])).unwrap().api_digest
        );
    }

    #[test]
    fn changed_public_return_type_or_effect_changes_digest() {
        let int = primitive(PrimitiveType::Int);
        let plain = interface_for_hir(&module(vec![function(
            "get", true, int.clone(), int.clone(), vec![],
        )])).unwrap();
        let changed_type = interface_for_hir(&module(vec![function(
            "get", true, int.clone(), primitive(PrimitiveType::String), vec![],
        )])).unwrap();
        let changed_effect = interface_for_hir(&module(vec![function(
            "get", true, int.clone(), int, vec![Effect::IO],
        )])).unwrap();
        assert_ne!(plain.api_digest, changed_type.api_digest);
        assert_ne!(plain.api_digest, changed_effect.api_digest);
    }

    #[test]
    fn fresh_type_variable_ids_are_alpha_normalized() {
        let generic = |raw| {
            let tv = Type::Var(TypeVar(raw));
            module(vec![Decl::Function(hir::FunctionDef {
                name: "id".to_string(),
                type_params: vec!["T".to_string()],
                params: vec![("value".to_string(), tv.clone())],
                dict_params: Vec::new(),
                ret: tv,
                effect: EffectRow::Closed(vec![]),
                cap: Capability::Ref,
                body: hir::Body::default(),
                public: true,
                placement: None,
                span: Span::default(),
            })])
        };
        let first = interface_for_hir(&generic(17)).unwrap();
        let second = interface_for_hir(&generic(930)).unwrap();
        assert_eq!(first.api_digest, second.api_digest);
        assert_eq!(first.exports, second.exports);
    }

    #[test]
    fn opaque_and_transparent_aliases_have_different_api_digests() {
        let alias = |opaque| module(vec![Decl::TypeAlias {
            name: "Token".to_string(),
            type_params: vec![],
            body: primitive(PrimitiveType::Int),
            opaque,
            public: true,
            span: Span::default(),
        }]);
        assert_ne!(
            interface_for_hir(&alias(true)).unwrap().api_digest,
            interface_for_hir(&alias(false)).unwrap().api_digest
        );
    }

    #[test]
    fn opaque_private_representation_does_not_affect_source_api_digest() {
        let alias = |body| module(vec![Decl::TypeAlias {
            name: "Token".to_string(),
            type_params: vec![],
            body,
            opaque: true,
            public: true,
            span: Span::default(),
        }]);
        let first = interface_for_hir(&alias(primitive(PrimitiveType::Int))).unwrap();
        let second = interface_for_hir(&alias(primitive(PrimitiveType::String))).unwrap();
        assert_eq!(first.api_digest, second.api_digest);
        assert_eq!(first.exports, second.exports);
    }

    #[test]
    fn opaque_parameters_hash_by_identity_not_private_representation() {
        let nominal = |underlying| Type::Nominal {
            name: "test::Token".to_string(),
            underlying: Box::new(underlying),
        };
        let make = |underlying| module(vec![function(
            "accept", true, nominal(underlying), Type::unit(), vec![],
        )]);
        let first = interface_for_hir(&make(primitive(PrimitiveType::Int))).unwrap();
        let second = interface_for_hir(&make(primitive(PrimitiveType::String))).unwrap();
        assert_eq!(first.api_digest, second.api_digest);
    }

    #[test]
    fn nested_exports_are_selective_and_namespaced() {
        let int = primitive(PrimitiveType::Int);
        let make = |private_ret| module(vec![Decl::Module {
            name: "Math".to_string(),
            exports: vec!["visible".to_string()],
            decls: vec![
                function("visible", false, int.clone(), int.clone(), vec![]),
                function("hidden", false, int.clone(), private_ret, vec![]),
            ],
            span: Span::default(),
        }]);
        let first = interface_for_hir(&make(int.clone())).unwrap();
        let second = interface_for_hir(&make(primitive(PrimitiveType::String))).unwrap();
        assert_eq!(first.api_digest, second.api_digest);
        assert_eq!(first.exports[0].path, "test::Math::visible");
        assert_eq!(first.exports.len(), 1);
    }

    #[test]
    fn unsupported_exported_actor_fails_closed() {
        let module = module(vec![Decl::Module {
            name: "Bad".to_string(),
            exports: vec!["Counter".to_string()],
            decls: vec![Decl::Actor(hir::ActorDef {
                name: "Counter".to_string(),
                type_params: Vec::new(),
                persistent: false,
                state_fields: Vec::new(),
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
            })],
            span: Span::default(),
        }]);
        let err = interface_for_hir(&module).unwrap_err();
        assert!(err.contains("unsupported exported declaration"));
    }

    fn ast_fn(name: &str, annotated_effect: Option<EffectRow>) -> crate::ast::Decl {
        use crate::ast::{Decl as AstDecl, Expr, Literal, Param};
        AstDecl::Function {
            name: name.to_string(),
            type_params: Vec::new(),
            type_param_constraints: Vec::new(),
            params: vec![Param::new("value", None)],
            default_values: vec![None],
            using_params: Vec::new(),
            ret_type: None,
            error_type: None,
            effect: annotated_effect,
            cap: None,
            requires: Vec::new(),
            ensures: Vec::new(),
            body: Expr::Literal(Literal::Int(1), Span::default()),
            annotations: Vec::new(),
            public: true,
            span: Span::default(),
        }
    }

    fn inferred_function(ret: Type) -> Type {
        Type::Function {
            param: Box::new(Type::int()),
            ret: Box::new(ret),
            effect: EffectRow::empty(),
            cap: Capability::Ref,
        }
    }

    #[test]
    fn checked_interface_uses_inferred_return_type_not_hir_unit_fallback() {
        let source = crate::ast::AstModule {
            name: "api".to_string(),
            decls: vec![ast_fn("answer", Some(EffectRow::empty()))],
        };
        let mut left = rustc_hash::FxHashMap::default();
        left.insert("answer".to_string(), inferred_function(Type::int()));
        let mut right = rustc_hash::FxHashMap::default();
        right.insert("answer".to_string(), inferred_function(Type::string()));

        let before = interface_for_checked_ast(&source, &left).unwrap();
        let after = interface_for_checked_ast(&source, &right).unwrap();
        assert_ne!(before.api_digest, after.api_digest);
    }

    #[test]
    fn checked_interface_rejects_unverified_effect_rows() {
        let source = crate::ast::AstModule {
            name: "api".to_string(),
            decls: vec![ast_fn("answer", None)],
        };
        let mut inferred = rustc_hash::FxHashMap::default();
        inferred.insert("answer".to_string(), inferred_function(Type::int()));
        let err = interface_for_checked_ast(&source, &inferred).unwrap_err();
        assert!(err.contains("effect"));
    }

    #[test]
    fn checked_interface_rejects_missing_inference_record() {
        let source = crate::ast::AstModule {
            name: "api".to_string(),
            decls: vec![ast_fn("answer", Some(EffectRow::empty()))],
        };
        let err = interface_for_checked_ast(&source, &rustc_hash::FxHashMap::default()).unwrap_err();
        assert!(err.contains("inferred"));
    }

    #[test]
    fn json_sidecar_is_deterministic() {
        let int = primitive(PrimitiveType::Int);
        let api = interface_for_hir(&module(vec![function(
            "get", true, int.clone(), int, vec![],
        )])).unwrap();
        let first = api.to_pretty_json().unwrap();
        assert_eq!(first, api.to_pretty_json().unwrap());
        assert!(first.contains("signature_digest"));
        assert!(first.contains("api_digest"));
    }
}
