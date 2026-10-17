//! Source-compiled metadata through durable intent, provider dedup, and receipt.
//! This does not exercise the VM's live PerformAsync callback or process restarts.

use nulang::authority::{AuthorityGrant, AuthorityManifest};
use nulang::bytecode::CodeModule;
use nulang::durable_effect_runtime::{
    DurableAsyncDispatchError, DurableEffectCoordinator,
};
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::runtime::{Actor, MemoryStore, PersistenceStore};
use nulang::typechecker::TypeChecker;
use std::collections::BTreeMap;
use std::io;

fn inference_grant() -> AuthorityGrant {
    AuthorityGrant::Other {
        namespace: "Inference".into(),
        operation: "ask".into(),
        argument: None,
    }
}

fn authorized_actor() -> Actor {
    let mut actor = Actor::new(42, "Payments", 16);
    actor.persistent = true;
    actor.activation_epoch = 1;
    actor.install_authority_manifest(&AuthorityManifest::from_grants([inference_grant()]));
    actor
}

fn compiled_source() -> CodeModule {
    let source =
        "actor Payments { behavior charge(order: String) { perform Inference.ask(order) } }";
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut types = TypeChecker::new();
    types.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &types.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).unwrap();
    let module = mir_codegen::compile_mir(&mut mir, "compiled-durable-dispatch").unwrap();
    assert_eq!(module.effect_sites.len(), 1);
    module
}

#[test]
fn compiled_effect_dispatch_records_receipt_and_never_reinvokes_completed_provider() {
    let module = compiled_source();
    let pc = module.effect_sites[0].pc;
    let actor = authorized_actor();
    let grant = inference_grant();
    let request = b"order=42&cents=100";
    let mut store = MemoryStore::new();
    let mut calls = 0;

    let receipt = {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        coordinator
            .execute_compiled_async_with_deduplication(
                &actor, &grant,
                &module,
                pc,
                "order:42:payment",
                0,
                request,
                |key, supplied_request| {
                    calls += 1;
                    assert_eq!(supplied_request, request);
                    assert_eq!(key.len(), 64);
                    assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
                    Ok(b"payment-receipt".to_vec())
                },
            )
            .unwrap()
    };
    assert_eq!(receipt, b"payment-receipt");
    assert_eq!(store.latest_sequence(42), 2);
    assert_eq!(calls, 1);

    let mut recovered = DurableEffectCoordinator::new(&mut store, 42, 1);
    let replay = recovered
        .execute_compiled_async_with_deduplication(
                &actor, &grant,
            &module,
            pc,
            "order:42:payment",
            0,
            request,
            |_, _| panic!("completed operation must not redispatch"),
        )
        .unwrap();
    assert_eq!(replay, receipt);
    assert_eq!(store.latest_sequence(42), 2);
}

#[test]
fn lost_provider_response_keeps_prepared_intent_and_retries_same_key() {
    let module = compiled_source();
    let pc = module.effect_sites[0].pc;
    let actor = authorized_actor();
    let grant = inference_grant();
    let mut store = MemoryStore::new();
    let mut provider = BTreeMap::<String, Vec<u8>>::new();
    let mut calls = 0;

    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        let failed = coordinator.execute_compiled_async_with_deduplication(
                &actor, &grant,
            &module,
            pc,
            "order:42:payment",
            0,
            b"charge=100",
            |key, _| {
                calls += 1;
                provider.insert(key.to_string(), b"receipt".to_vec());
                Err(io::Error::new(io::ErrorKind::TimedOut, "lost provider response"))
            },
        );
        assert!(matches!(
            failed,
            Err(DurableAsyncDispatchError::Provider(ref error))
                if error.kind() == io::ErrorKind::TimedOut
        ));
    }
    assert_eq!(store.latest_sequence(42), 1);
    assert_eq!(provider.len(), 1);

    // Simulate a separate recovered coordinator with the same durable store.
    {
        let mut recovered = DurableEffectCoordinator::new(&mut store, 42, 1);
        let receipt = recovered
            .execute_compiled_async_with_deduplication(
                &actor, &grant,
                &module,
                pc,
                "order:42:payment",
                0,
                b"charge=100",
                |key, _| {
                    calls += 1;
                    Ok(provider
                        .entry(key.to_string())
                        .or_insert_with(|| b"unexpected-duplicate-mutation".to_vec())
                        .clone())
                },
            )
            .unwrap();
        assert_eq!(receipt, b"receipt");
    }
    assert_eq!(calls, 2);
    assert_eq!(provider.len(), 1);
    assert_eq!(store.latest_sequence(42), 2);
}

#[test]
fn metadata_or_request_drift_never_reaches_provider() {
    let module = compiled_source();
    let pc = module.effect_sites[0].pc;
    let actor = authorized_actor();
    let grant = inference_grant();
    let mut store = MemoryStore::new();
    let mut mismatched = module.clone();
    mismatched.effect_sites[0].effect_operation = "Other.charge".into();
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        let failure = coordinator.execute_compiled_async_with_deduplication(
                &actor, &grant,
            &mismatched,
            pc,
            "order:42:payment",
            0,
            b"charge=100",
            |_, _| panic!("invalid compiler metadata must not dispatch"),
        );
        assert!(matches!(failure, Err(DurableAsyncDispatchError::InvalidSite(_))));
    }
    assert_eq!(store.latest_sequence(42), 0);

    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, 42, 1);
        coordinator
            .execute_compiled_async_with_deduplication(
                &actor, &grant,
                &module,
                pc,
                "order:42:payment",
                0,
                b"charge=100",
                |_, _| Ok(b"receipt".to_vec()),
            )
            .unwrap();
    }
    let mut replay = DurableEffectCoordinator::new(&mut store, 42, 1);
    assert!(matches!(
        replay.execute_compiled_async_with_deduplication(
                &actor, &grant,
            &module, pc, "order:42:payment", 0, b"charge=200",
            |_, _| panic!("request drift must not dispatch")
        ),
        Err(DurableAsyncDispatchError::Recovery(_))
    ));
    assert_eq!(store.latest_sequence(42), 2);
}

#[test]
fn stale_activation_does_not_call_provider_for_prepared_effect() {
    let module = compiled_source();
    let pc = module.effect_sites[0].pc;
    let actor = authorized_actor();
    let grant = inference_grant();
    let mut store = MemoryStore::new();
    {
        let mut first = DurableEffectCoordinator::new(&mut store, 42, 1);
        let failed = first.execute_compiled_async_with_deduplication(
                &actor, &grant,
            &module, pc, "order:42:payment", 0, b"charge=100",
            |_, _| Err(io::Error::new(io::ErrorKind::TimedOut, "no receipt")),
        );
        assert!(matches!(failed, Err(DurableAsyncDispatchError::Provider(_))));
    }
    {
        // Claim newer epoch by committing a distinct durable effect.
        use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
        use nulang::primitives::{DeliverySemantics, EffectBoundary};
        let other = DurableEffectSpec::new(
            DurableEffectId::derive(42, "epoch-2-claim", 0, "Test.claim"),
            "Test.claim",
            EffectBoundary::Local,
            DeliverySemantics::AtLeastOnce,
        );
        let mut claimed = DurableEffectCoordinator::new(&mut store, 42, 2);
        claimed.begin(other, b"claim").unwrap();
    }
    let mut stale = DurableEffectCoordinator::new(&mut store, 42, 1);
    let result = stale.execute_compiled_async_with_deduplication(
                &actor, &grant,
        &module, pc, "order:42:payment", 0, b"charge=100",
        |_, _| panic!("stale owner must not redispatch"),
    );
    assert!(matches!(result, Err(DurableAsyncDispatchError::Recovery(_))));
    assert_eq!(store.latest_sequence(42), 2);
}
