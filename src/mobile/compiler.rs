//! Compiler-side construction of the native client-action allowlist.
//!
//! Authorization is derived from semantic UI bindings, not from public
//! exports, tools, or function naming. A handler is eligible only when the
//! existing reactivity/effect analysis classifies its binding as `client`, its
//! effect row is actually known, its inferred type is the frozen
//! `String -> String` reducer ABI, and codegen produced a concrete top-level
//! function-table entry.

use std::collections::BTreeSet;
use std::fmt;

use crate::ast::AstModule;
use crate::bytecode::CodeModule;
use crate::effect_checker::EffectChecker;
use crate::format::mobile_nbc::{
    ClientActionEntry, MobileActionMetadata, CLIENT_ACTION_ABI,
};
use crate::mobile::action::stable_client_action_id;
use crate::typechecker::TypeChecker;
use crate::types::{PrimitiveType, Type};
use crate::web::reactivity::{analyze_module, ActionPlacement, GraphNode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientActionCompileError {
    MissingEffectRow { handler: String },
    MissingInferredType { handler: String },
    InvalidReducerType { handler: String, found: String },
    MissingCompiledFunction { handler: String },
    InvalidActionIdentity { handler: String, reason: String },
    FunctionIndexOverflow { handler: String, index: usize },
}

impl fmt::Display for ClientActionCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEffectRow { handler } => write!(
                f,
                "client action '{handler}' has no inferred effect row; refusing native authorization"
            ),
            Self::MissingInferredType { handler } => write!(
                f,
                "client action '{handler}' has no inferred function type"
            ),
            Self::InvalidReducerType { handler, found } => write!(
                f,
                "client action '{handler}' must have ABI String -> String, found {found}"
            ),
            Self::MissingCompiledFunction { handler } => write!(
                f,
                "client action '{handler}' is not a compiled top-level function"
            ),
            Self::InvalidActionIdentity { handler, reason } => write!(
                f,
                "client action '{handler}' could not derive an opaque action identity: {reason}"
            ),
            Self::FunctionIndexOverflow { handler, index } => write!(
                f,
                "client action '{handler}' function index {index} exceeds u32"
            ),
        }
    }
}

impl std::error::Error for ClientActionCompileError {}

/// Build the compiler-authorized native client-action table.
///
/// Repeated bindings to the same handler produce one allowlist entry. Server
/// actions are intentionally omitted: native hosts must route those through
/// the explicit server/network policy rather than executing them locally.
/// Missing effect-analysis results fail closed rather than inheriting the web
/// hydrator's permissive default-to-client behavior.
pub fn build_client_action_metadata(
    ast: &AstModule,
    type_checker: &TypeChecker,
    effect_checker: &EffectChecker,
    module: &CodeModule,
) -> Result<MobileActionMetadata, ClientActionCompileError> {
    let graph = analyze_module(ast, Some(effect_checker));
    let handlers: BTreeSet<String> = graph
        .nodes
        .iter()
        .filter_map(|node| match node {
            GraphNode::Action {
                handler,
                placement: ActionPlacement::Client,
                ..
            } => Some(handler.clone()),
            _ => None,
        })
        .collect();

    let mut client_actions = Vec::with_capacity(handlers.len());
    for handler in handlers {
        if effect_checker.function_row(&handler).is_none() {
            return Err(ClientActionCompileError::MissingEffectRow {
                handler: handler.clone(),
            });
        }

        let inferred = type_checker
            .inferred_decl_types
            .get(&handler)
            .ok_or_else(|| ClientActionCompileError::MissingInferredType {
                handler: handler.clone(),
            })?;

        if !is_string_reducer(inferred) {
            return Err(ClientActionCompileError::InvalidReducerType {
                handler,
                found: inferred.to_string(),
            });
        }

        let function_index = module.function_index_by_name(&handler).ok_or_else(|| {
            ClientActionCompileError::MissingCompiledFunction {
                handler: handler.clone(),
            }
        })?;
        let function_index = u32::try_from(function_index).map_err(|_| {
            ClientActionCompileError::FunctionIndexOverflow {
                handler: handler.clone(),
                index: function_index,
            }
        })?;

        let action_id = stable_client_action_id(&module.name, &handler).map_err(|error| {
            ClientActionCompileError::InvalidActionIdentity {
                handler: handler.clone(),
                reason: error.to_string(),
            }
        })?;

        client_actions.push(ClientActionEntry {
            action_id: action_id.as_str().to_owned(),
            handler,
            function_index,
        });
    }

    Ok(MobileActionMetadata {
        abi: CLIENT_ACTION_ABI.to_owned(),
        client_actions,
    })
}

fn is_string_reducer(ty: &Type) -> bool {
    match ty {
        Type::Scheme { body, .. } => is_string_reducer(body),
        Type::Function { param, ret, .. } => {
            is_primitive_string(param.as_ref()) && is_primitive_string(ret.as_ref())
        }
        _ => false,
    }
}

fn is_primitive_string(ty: &Type) -> bool {
    matches!(ty, Type::Primitive(PrimitiveType::String))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn compile(source: &str) -> (AstModule, TypeChecker, EffectChecker, CodeModule) {
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");

        let mut type_checker = TypeChecker::new();
        type_checker.check_module(&ast).expect("typecheck");

        let mut effect_checker = EffectChecker::new();
        effect_checker
            .check_module(&ast.decls)
            .expect("effect check");

        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("MIR lower");
        let module = crate::mir_codegen::compile_mir(&mut mir, "mobile-actions")
            .expect("bytecode compile");

        (ast, type_checker, effect_checker, module)
    }

    #[test]
    fn bound_client_string_reducer_is_authorized_once() {
        let source = r#"
import stdlib::web::html
import stdlib::web::types

fn increment(request: String) -> String { request }

fn view() -> Html {
    <div>
        <button action={increment}>One</button>
        <button action={increment}>Two</button>
    </div>
}
"#;
        let (ast, types, effects, module) = compile(source);
        let metadata = build_client_action_metadata(&ast, &types, &effects, &module)
            .expect("client action metadata");

        let expected_id =
            stable_client_action_id(&module.name, "increment").expect("opaque action id");
        assert_eq!(metadata.abi, CLIENT_ACTION_ABI);
        assert_eq!(metadata.client_actions.len(), 1);
        assert_eq!(metadata.client_actions[0].action_id, expected_id.as_str());
        assert_ne!(metadata.client_actions[0].action_id, "increment");
        assert_eq!(metadata.client_actions[0].handler, "increment");
        assert_eq!(
            metadata.client_actions[0].function_index as usize,
            module
                .function_index_by_name("increment")
                .expect("compiled increment function")
        );
    }

    #[test]
    fn unbound_function_is_not_authorized() {
        let source = r#"
import stdlib::web::html
import stdlib::web::types

fn secret(request: String) -> String { request }
fn view() -> Html { <button>Safe</button> }
"#;
        let (ast, types, effects, module) = compile(source);
        let metadata = build_client_action_metadata(&ast, &types, &effects, &module)
            .expect("client action metadata");
        assert!(metadata.client_actions.is_empty());
    }

    #[test]
    fn bound_wrong_signature_fails_closed() {
        let source = r#"
import stdlib::web::html
import stdlib::web::types

fn bad(request: Int) -> Int { request }
fn view() -> Html { <button action={bad}>Bad</button> }
"#;
        let (ast, types, effects, module) = compile(source);
        let error = build_client_action_metadata(&ast, &types, &effects, &module)
            .expect_err("wrong reducer type must be rejected");
        assert!(matches!(
            error,
            ClientActionCompileError::InvalidReducerType { ref handler, .. }
                if handler == "bad"
        ));
    }

    #[test]
    fn missing_effect_analysis_cannot_authorize_action() {
        let source = r#"
import stdlib::web::html
import stdlib::web::types

fn save(request: String) -> String { request }
fn view() -> Html { <button action={save}>Save</button> }
"#;
        let (ast, types, _effects, module) = compile(source);
        let empty_effects = EffectChecker::new();
        let error = build_client_action_metadata(&ast, &types, &empty_effects, &module)
            .expect_err("missing effects must fail closed");
        assert_eq!(
            error,
            ClientActionCompileError::MissingEffectRow {
                handler: "save".to_string()
            }
        );
    }
}
