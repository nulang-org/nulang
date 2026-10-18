#![cfg(feature = "sqlite")]

/*!
Compiler-owned async effect metadata through a real file-backed libSQL restart.

This is a trusted Rust host-adapter test: it does not claim that an ordinary
Nulang VM PerformAsync callback automatically uses the durable coordinator, or
that dropping a store is equivalent to killing a process. Subprocess crash
windows are covered separately by the pending durable-effect crash fixture.
*/

use nulang::authority::{AuthorityGrant, AuthorityManifest};
use nulang::bytecode::CodeModule;
use nulang::durable_effect_runtime::{DurableAsyncDispatchError, DurableEffectCoordinator};
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::runtime::{Actor, LibsqlStore, PersistenceStore};
use nulang::typechecker::TypeChecker;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const ACTOR_ID: u64 = 42;
const EXECUTION_KEY: &str = "order:42:payment";
const REQUEST: &[u8] = b"order=42&cents=100";
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    dir: PathBuf,
    path: PathBuf,
}

impl TestDb {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nulang-compiled-durable-reopen-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&dir).expect("isolated test directory");
        let path = dir.join("effects.db");
        Self { dir, path }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn compiled_nbc() -> CodeModule {
    let source = "actor Payments { behavior charge(order: String) { perform Inference.ask(order) } }";
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut types = TypeChecker::new();
    types.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &types.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).unwrap();
    let compiled = mir_codegen::compile_mir(&mut mir, "sqlite-effect-reopen").unwrap();
    let bytes = compiled.to_nbc(None).expect("serialize compiled artifact");
    let restored = CodeModule::from_nbc(&bytes)
        .expect("load compiled artifact")
        .module;
    assert_eq!(restored.effect_sites.len(), 1);
    assert_eq!(restored.effect_sites[0].effect_operation, "Inference.ask");
    restored
}

fn grant() -> AuthorityGrant {
    AuthorityGrant::Other {
        namespace: "Inference".into(),
        operation: "ask".into(),
        argument: None,
    }
}

fn actor(authorized: bool) -> Actor {
    let mut actor = Actor::new(ACTOR_ID, "Payments", 16);
    actor.persistent = true;
    actor.activation_epoch = 1;
    let grants: Vec<AuthorityGrant> = if authorized { vec![grant()] } else { vec![] };
    actor.install_authority_manifest(&AuthorityManifest::from_grants(grants));
    actor
}

#[test]
fn compiled_receipt_replays_after_sqlite_reopen_without_provider_or_journal_mutation() {
    let db = TestDb::new("completed");
    let module = compiled_nbc();
    let pc = module.effect_sites[0].pc;
    let authorized = actor(true);
    let denied = actor(false);
    let result = b"provider-receipt-42".to_vec();

    {
        let mut store = LibsqlStore::new(&db.path).unwrap();
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        let first = coordinator
            .execute_compiled_async_with_deduplication(
                &authorized,
                &grant(),
                &module,
                pc,
                EXECUTION_KEY,
                0,
                REQUEST,
                |key, request| {
                    assert_eq!(key.len(), 64);
                    assert_eq!(request, REQUEST);
                    Ok(result.clone())
                },
            )
            .unwrap();
        assert_eq!(first, result);
        assert_eq!(store.latest_sequence(ACTOR_ID), 2);
    }

    // A fresh store object must load Prepared/Completed from actual disk, not
    // reuse an in-memory coordinator or provider response.
    {
        let mut reopened = LibsqlStore::new(&db.path).unwrap();
        let mut coordinator = DurableEffectCoordinator::new(&mut reopened, ACTOR_ID, 1);
        let replayed = coordinator
            .execute_compiled_async_with_deduplication(
                &authorized,
                &grant(),
                &module,
                pc,
                EXECUTION_KEY,
                0,
                REQUEST,
                |_, _| panic!("completed receipt must never redispatch"),
            )
            .unwrap();
        assert_eq!(replayed, result);
        assert_eq!(reopened.latest_sequence(ACTOR_ID), 2);
    }

    {
        let mut reopened = LibsqlStore::new(&db.path).unwrap();
        let mut coordinator = DurableEffectCoordinator::new(&mut reopened, ACTOR_ID, 1);
        assert!(matches!(
            coordinator.execute_compiled_async_with_deduplication(
                &authorized,
                &grant(),
                &module,
                pc,
                EXECUTION_KEY,
                0,
                b"order=42&cents=200",
                |_, _| panic!("recovery request drift must not reach provider"),
            ),
            Err(DurableAsyncDispatchError::Recovery(_))
        ));
        assert_eq!(reopened.latest_sequence(ACTOR_ID), 2);
    }

    {
        let mut reopened = LibsqlStore::new(&db.path).unwrap();
        let mut coordinator = DurableEffectCoordinator::new(&mut reopened, ACTOR_ID, 1);
        assert!(matches!(
            coordinator.execute_compiled_async_with_deduplication(
                &denied,
                &grant(),
                &module,
                pc,
                EXECUTION_KEY,
                0,
                REQUEST,
                |_, _| panic!("revoked authority must not reveal receipt or dispatch"),
            ),
            Err(DurableAsyncDispatchError::Authorization(_))
        ));
        assert_eq!(reopened.latest_sequence(ACTOR_ID), 2);
    }
}

#[test]
fn sqlite_reopen_retries_prepared_effect_with_same_key_but_requires_live_authority() {
    let db = TestDb::new("prepared");
    let module = compiled_nbc();
    let pc = module.effect_sites[0].pc;
    let authorized = actor(true);
    let denied = actor(false);
    let mut prepared_key = String::new();

    {
        let mut store = LibsqlStore::new(&db.path).unwrap();
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        let failure = coordinator.execute_compiled_async_with_deduplication(
            &authorized,
            &grant(),
            &module,
            pc,
            EXECUTION_KEY,
            0,
            REQUEST,
            |key, _| {
                prepared_key = key.to_owned();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "external provider may have committed",
                ))
            },
        );
        assert!(matches!(failure, Err(DurableAsyncDispatchError::Provider(_))));
        assert_eq!(store.latest_sequence(ACTOR_ID), 1);
    }
    assert_eq!(prepared_key.len(), 64);

    {
        let mut reopened = LibsqlStore::new(&db.path).unwrap();
        let mut coordinator = DurableEffectCoordinator::new(&mut reopened, ACTOR_ID, 1);
        assert!(matches!(
            coordinator.execute_compiled_async_with_deduplication(
                &denied,
                &grant(),
                &module,
                pc,
                EXECUTION_KEY,
                0,
                REQUEST,
                |_, _| panic!("revoked authority must not retry Prepared"),
            ),
            Err(DurableAsyncDispatchError::Authorization(_))
        ));
        assert_eq!(reopened.latest_sequence(ACTOR_ID), 1);
    }

    {
        let mut reopened = LibsqlStore::new(&db.path).unwrap();
        let mut coordinator = DurableEffectCoordinator::new(&mut reopened, ACTOR_ID, 1);
        let receipt = coordinator
            .execute_compiled_async_with_deduplication(
                &authorized,
                &grant(),
                &module,
                pc,
                EXECUTION_KEY,
                0,
                REQUEST,
                |key, request| {
                    assert_eq!(key, prepared_key);
                    assert_eq!(request, REQUEST);
                    Ok(b"deduplicated-provider-receipt".to_vec())
                },
            )
            .unwrap();
        assert_eq!(receipt.as_slice(), b"deduplicated-provider-receipt");
        assert_eq!(reopened.latest_sequence(ACTOR_ID), 2);
    }

    {
        let mut reopened = LibsqlStore::new(&db.path).unwrap();
        let mut coordinator = DurableEffectCoordinator::new(&mut reopened, ACTOR_ID, 1);
        let replayed = coordinator
                .execute_compiled_async_with_deduplication(
                    &authorized,
                    &grant(),
                    &module,
                    pc,
                    EXECUTION_KEY,
                    0,
                    REQUEST,
                    |_, _| panic!("terminal receipt must not redispatch"),
                )
                .unwrap();
        assert_eq!(replayed.as_slice(), b"deduplicated-provider-receipt");
        assert_eq!(reopened.latest_sequence(ACTOR_ID), 2);
    }
}
