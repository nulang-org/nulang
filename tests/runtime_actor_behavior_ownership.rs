use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::Runtime;
use nulang::typechecker::TypeChecker;

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lowering");
    nulang::mir_codegen::compile_mir(&mut mir, "runtime_behavior_ownership")
        .expect("bytecode codegen")
}

fn duplicate_hit_module() -> (nulang::bytecode::CodeModule, usize, usize) {
    let module = compile(
        r#"
        actor First {
            behavior hit() -> Int { 1 }
        }

        actor Second {
            behavior hit() -> Int { 2 }
        }
        "#,
    );

    let first = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "First")
        .expect("First actor metadata");
    let second = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "Second")
        .expect("Second actor metadata");
    let first_hit = *first.behavior_indices.first().expect("First.hit index");
    let second_hit = *second.behavior_indices.first().expect("Second.hit index");
    assert_ne!(first_hit, second_hit, "fixture requires distinct behavior ids");
    (module, first_hit, second_hit)
}

fn spawn_second(module: &nulang::bytecode::CodeModule, second_hit: usize) -> (Runtime, u64) {
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(module, second_hit, vec![])
        .as_actor_id()
        .expect("module spawn returns actor ref");
    assert_eq!(runtime.actors[&actor_id].name, "Second");
    (runtime, actor_id)
}

#[test]
fn name_lookup_is_scoped_to_target_actor_schema() {
    let (module, first_hit, second_hit) = duplicate_hit_module();
    let (runtime, actor_id) = spawn_second(&module, second_hit);

    assert_eq!(
        runtime.behavior_id_for(actor_id, "hit"),
        Some(second_hit as u16),
        "Second.hit must not resolve the earlier First.hit slot {first_hit}"
    );
    assert_eq!(
        runtime.behavior_id_for(actor_id, "Second.hit"),
        Some(second_hit as u16),
        "fully qualified lookup must preserve the owning schema"
    );
    assert_eq!(
        runtime.behavior_id_for(actor_id, "First.hit"),
        None,
        "a target actor must not expose another schema's qualified behavior"
    );
}

#[test]
fn numeric_ask_rejects_behavior_owned_by_another_actor_schema() {
    let (module, first_hit, second_hit) = duplicate_hit_module();
    let (mut runtime, actor_id) = spawn_second(&module, second_hit);

    let error = runtime
        .ask_actor_sync(actor_id, first_hit as u16, &[])
        .expect_err("Second must reject First.hit's numeric behavior id");
    assert!(
        error.to_string().contains("does not declare behavior id"),
        "unexpected error: {error}"
    );

    let own = runtime
        .ask_actor_sync(actor_id, second_hit as u16, &[])
        .expect("Second.hit remains executable");
    assert_eq!(own.as_int(), Some(2));
}

#[test]
fn numeric_send_rejects_behavior_owned_by_another_actor_schema() {
    let (module, first_hit, second_hit) = duplicate_hit_module();
    let (mut runtime, actor_id) = spawn_second(&module, second_hit);

    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 0);
    runtime.send_message_by_id(actor_id, first_hit as u16, &[]);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.len(),
        0,
        "foreign-schema numeric id must be rejected before mailbox publication"
    );

    runtime.send_message_by_id(actor_id, second_hit as u16, &[]);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.len(),
        1,
        "owned behavior id remains deliverable"
    );
}

#[test]
fn cross_shard_numeric_delivery_rejects_foreign_schema_before_execution() {
    let module = compile(
        r#"
        actor First {
            state marker: Int = 0
            behavior hit() { self.marker = 1 }
        }

        actor Second {
            state marker: Int = 0
            behavior hit() { self.marker = 2 }
        }
        "#,
    );
    let first_hit = *module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "First")
        .and_then(|meta| meta.behavior_indices.first())
        .expect("First.hit index");
    let second_hit = *module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "Second")
        .and_then(|meta| meta.behavior_indices.first())
        .expect("Second.hit index");
    assert_ne!(first_hit, second_hit, "fixture requires distinct behavior ids");

    let mut shards = Runtime::new_sharded(2);
    let actor_id = shards[0]
        .spawn_from_module(&module, second_hit, vec![])
        .as_actor_id()
        .expect("module spawn returns actor ref");
    let owner = (actor_id % 2) as usize;
    if owner != 0 {
        let actor = shards[0]
            .actors
            .remove(&actor_id)
            .expect("spawned actor on construction shard");
        shards[owner].actors.insert(actor_id, actor);
    }
    assert_eq!(shards[owner].actors[&actor_id].name, "Second");
    let sender = 1 - owner;

    shards[sender].send_message_by_id(actor_id, first_hit as u16, &[]);
    let _ = shards[owner].run_scheduler_deterministic(7, 8);
    assert_eq!(
        shards[owner].actors[&actor_id]
            .get_state_field("marker")
            .and_then(|value| value.as_int()),
        Some(0),
        "cross-shard ingress must reject First.hit before it can execute on Second"
    );

    shards[sender].send_message_by_id(actor_id, second_hit as u16, &[]);
    let _ = shards[owner].run_scheduler_deterministic(7, 8);
    assert_eq!(
        shards[owner].actors[&actor_id]
            .get_state_field("marker")
            .and_then(|value| value.as_int()),
        Some(2),
        "owned Second.hit must still execute across shards"
    );
}
