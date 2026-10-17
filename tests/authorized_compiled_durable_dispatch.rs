//! Authorization boundary for compiler-identified durable provider calls.
//! The authority supplied here represents the live actor's grants, not a
//! capability asserted by the provider callback or an untrusted request.

use nulang::authority::{AuthorityGrant, AuthorityManifest};
use nulang::bytecode::CodeModule;
use nulang::durable_effect_runtime::{DurableAsyncDispatchError, DurableEffectCoordinator};
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::runtime::{Actor, MemoryStore, PersistenceStore};
use nulang::typechecker::TypeChecker;
use std::io;

fn compiled_source() -> CodeModule {
    let source = "actor Payments { behavior charge(order: String) { perform Inference.ask(order) } }";
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut types = TypeChecker::new();
    types.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &types.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).unwrap();
    let module = mir_codegen::compile_mir(&mut mir, "authorized-compiled-dispatch").unwrap();
    assert_eq!(module.effect_sites.len(), 1);
    module
}

fn inference_grant() -> AuthorityGrant {
    AuthorityGrant::Other {
        namespace: "Inference".into(),
        operation: "ask".into(),
        argument: None,
    }
}

fn actor_with_grants(id: u64, epoch: u64, grants: Vec<AuthorityGrant>) -> Actor {
    let mut actor = Actor::new(id, "Payments", 16);
    actor.persistent = true;
    actor.activation_epoch = epoch;
    actor.install_authority_manifest(&AuthorityManifest::from_grants(grants));
    actor
}

#[test]
fn missing_authority_blocks_preparation_and_provider_dispatch() {
    let module = compiled_source();
    let actor = actor_with_grants(42, 1, vec![]);
    let mut store = MemoryStore::new();
    let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
    let result = coordinator.execute_compiled_async_with_deduplication(
        &actor, &inference_grant(), &module, module.effect_sites[0].pc,
        "order:42:payment", 0, b"request",
        |_, _| panic!("missing grant must not call provider"),
    );
    assert!(matches!(result, Err(DurableAsyncDispatchError::Authorization(_))));
    assert_eq!(store.latest_sequence(42), 0);
}

#[test]
fn actor_identity_and_activation_must_match_coordinator() {
    let module = compiled_source();
    let grant = inference_grant();
    let wrong_actor = actor_with_grants(7, 1, vec![grant.clone()]);
    let stale_actor = actor_with_grants(42, 1, vec![grant.clone()]);
    let mut store = MemoryStore::new();
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert!(matches!(
            coordinator.execute_compiled_async_with_deduplication(
                &wrong_actor, &grant, &module, module.effect_sites[0].pc,
                "order:42:payment", 0, b"request",
                |_, _| panic!("wrong actor must not dispatch")),
            Err(DurableAsyncDispatchError::ActorMismatch { .. })
        ));
    }
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 2);
        assert!(matches!(
            coordinator.execute_compiled_async_with_deduplication(
                &stale_actor, &grant, &module, module.effect_sites[0].pc,
                "order:42:payment", 0, b"request",
                |_, _| panic!("stale activation must not dispatch")),
            Err(DurableAsyncDispatchError::ActorEpochMismatch { .. })
        ));
    }
    assert_eq!(store.latest_sequence(42), 0);
}

#[test]
fn unrelated_or_resource_parameterized_grants_cannot_authorize_inference_site() {
    let module = compiled_source();
    let wrong = AuthorityGrant::Other {
        namespace: "Inference".into(),
        operation: "delete".into(),
        argument: None,
    };
    let with_argument = AuthorityGrant::Other {
        namespace: "Inference".into(),
        operation: "ask".into(),
        argument: Some("different-resource".into()),
    };
    let actor = actor_with_grants(42, 1, vec![wrong.clone(), with_argument.clone()]);
    let mut store = MemoryStore::new();
    for grant in [&wrong, &with_argument] {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert!(matches!(
            coordinator.execute_compiled_async_with_deduplication(
                &actor, grant, &module, module.effect_sites[0].pc,
                "order:42:payment", 0, b"request",
                |_, _| panic!("wrong authority must not dispatch")),
            Err(DurableAsyncDispatchError::GrantOperationMismatch { .. })
        ));
    }
    assert_eq!(store.latest_sequence(42), 0);
}

#[test]
fn malformed_live_manifest_fails_closed_even_with_matching_grant() {
    let module = compiled_source();
    let grant = inference_grant();
    let mut actor = actor_with_grants(42, 1, vec![grant.clone()]);
    actor.capabilities.insert("invalid-authority-metadata".into());
    let mut store = MemoryStore::new();
    let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
    assert!(matches!(
        coordinator.execute_compiled_async_with_deduplication(
            &actor, &grant, &module, module.effect_sites[0].pc,
            "order:42:payment", 0, b"request",
            |_, _| panic!("corrupt manifest must not dispatch")),
        Err(DurableAsyncDispatchError::Authorization(_))
    ));
    assert_eq!(store.latest_sequence(42), 0);
}

#[test]
fn revocation_blocks_prepared_retry_and_completed_receipt_read() {
    let module = compiled_source();
    let grant = inference_grant();
    let authorized = actor_with_grants(42, 1, vec![grant.clone()]);
    let denied = actor_with_grants(42, 1, vec![]);
    let mut store = MemoryStore::new();
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert!(matches!(
            coordinator.execute_compiled_async_with_deduplication(
                &authorized, &grant, &module, module.effect_sites[0].pc,
                "order:42:payment", 0, b"request",
                |_, _| Err(io::Error::new(io::ErrorKind::TimedOut, "ambiguous result"))),
            Err(DurableAsyncDispatchError::Provider(_))
        ));
    }
    assert_eq!(store.latest_sequence(42), 1);
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        assert!(matches!(
            coordinator.execute_compiled_async_with_deduplication(
                &denied, &grant, &module, module.effect_sites[0].pc,
                "order:42:payment", 0, b"request",
                |_, _| panic!("revoked provider must not retry")),
            Err(DurableAsyncDispatchError::Authorization(_))
        ));
    }
    assert_eq!(store.latest_sequence(42), 1);
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        coordinator.execute_compiled_async_with_deduplication(
            &authorized, &grant, &module, module.effect_sites[0].pc,
            "order:42:payment", 0, b"request",
            |_, _| Ok(b"receipt".to_vec()),
        ).unwrap();
    }
    assert_eq!(store.latest_sequence(42), 2);
    let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
    assert!(matches!(
        coordinator.execute_compiled_async_with_deduplication(
            &denied, &grant, &module, module.effect_sites[0].pc,
            "order:42:payment", 0, b"request",
            |_, _| panic!("revoked receipt must not be read")),
        Err(DurableAsyncDispatchError::Authorization(_))
    ));
    assert_eq!(store.latest_sequence(42), 2);
}

#[test]
fn exact_live_grant_keeps_stable_deduplication_and_replay() {
    let module = compiled_source();
    let grant = inference_grant();
    let actor = actor_with_grants(42, 1, vec![grant.clone()]);
    let mut store = MemoryStore::new();
    let first = {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        coordinator.execute_compiled_async_with_deduplication(
            &actor, &grant, &module, module.effect_sites[0].pc,
            "order:42:payment", 0, b"request",
            |key, _| {
                assert_eq!(key.len(), 64);
                Ok(b"receipt".to_vec())
            },
        ).unwrap()
    };
    let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
    let replay = coordinator.execute_compiled_async_with_deduplication(
        &actor, &grant, &module, module.effect_sites[0].pc,
        "order:42:payment", 0, b"request",
        |_, _| panic!("completed effect must replay only"),
    ).unwrap();
    assert_eq!(first, replay);
    assert_eq!(store.latest_sequence(42), 2);
}
