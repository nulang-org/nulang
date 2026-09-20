//! Runtime lookup of canonical actor protocol identity.
//!
//! This module is intentionally narrow: it translates a live actor's runtime
//! identity into the compiler-emitted `ActorMeta.protocol_id`. It does not
//! infer protocol identity from behavior bytecode or content hashes.

use crate::bytecode::{ActorMeta, CodeModule};
use crate::protocol::ProtocolId;
use crate::runtime::Runtime;

fn actor_meta_for_runtime_name<'a>(
    module: &'a CodeModule,
    runtime_name: &str,
) -> Option<&'a ActorMeta> {
    if let Some(meta) = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == runtime_name)
    {
        return Some(meta);
    }

    // Virtual actor instances use the runtime name "Type@key". The type
    // prefix, not the key, is the actor schema identity.
    let (actor_type, _) = runtime_name.split_once('@')?;
    module
        .actor_metadata
        .iter()
        .find(|meta| meta.is_virtual && meta.name == actor_type)
}

pub(crate) fn protocol_id_for_actor(runtime: &Runtime, actor_id: u64) -> Option<ProtocolId> {
    let actor = runtime.actors.get(&actor_id)?;
    let module = actor.bytecode_module.as_ref()?;
    let meta = actor_meta_for_runtime_name(module, &actor.name)?;
    meta.protocol_id.map(ProtocolId::from_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;

    fn compile(source: &str) -> CodeModule {
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        let mut typechecker = TypeChecker::new();
        typechecker.check_module(&ast).expect("typecheck");
        let hir = crate::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("MIR lowering");
        crate::mir_codegen::compile_mir(&mut mir, "protocol_lookup")
            .expect("bytecode codegen")
    }

    #[test]
    fn ordinary_runtime_name_resolves_protocol_metadata() {
        let module = compile(
            r#"
            actor Account {
                behavior balance() -> Int { 0 }
            }
            "#,
        );
        let meta = actor_meta_for_runtime_name(&module, "Account").expect("Account metadata");
        assert!(meta.protocol_id.is_some());
    }

    #[test]
    fn virtual_instance_resolves_type_protocol_metadata() {
        let module = compile(
            r#"
            virtual entity User(key: String) {
                behavior name() -> String { "x" }
            }
            "#,
        );
        let meta =
            actor_meta_for_runtime_name(&module, "User@abc").expect("virtual actor metadata");
        assert_eq!(meta.name, "User");
        assert!(meta.protocol_id.is_some());
    }

    #[test]
    fn live_bytecode_actor_exposes_compiler_protocol_id() {
        let module = compile(
            r#"
            actor Account {
                behavior balance() -> Int { 0 }
            }
            "#,
        );
        let expected = module
            .actor_metadata
            .iter()
            .find(|meta| meta.name == "Account")
            .and_then(|meta| meta.protocol_id)
            .expect("compiled protocol id");
        let behavior_idx = module
            .actor_metadata
            .iter()
            .find(|meta| meta.name == "Account")
            .and_then(|meta| meta.behavior_indices.first())
            .copied()
            .expect("behavior index");

        let mut runtime = Runtime::new();
        let actor_id = runtime
            .spawn_from_module(&module, behavior_idx, vec![])
            .as_actor_id()
            .expect("actor ref");

        assert_eq!(
            runtime.actor_protocol_id(actor_id),
            Some(ProtocolId::from_bytes(expected))
        );
        assert_eq!(runtime.actors.get(&actor_id).unwrap().name, "Account");
    }

    #[test]
    fn synthetic_runtime_name_fails_closed() {
        let module = compile(
            r#"
            actor Account {
                behavior balance() -> Int { 0 }
            }
            "#,
        );
        assert!(actor_meta_for_runtime_name(&module, "actor_42").is_none());
    }
}
