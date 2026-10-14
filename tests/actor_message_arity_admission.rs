//! An actor's compiler-defined behavior arity is an admission invariant.
//! Malformed dynamic/cross-version delivery must not reach a handler or journal.

use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::Runtime;
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn compiled_counter() -> (nulang::bytecode::CodeModule, u16) {
    let source = r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(by: Int) { self.total = self.total + by }
        }
    "#;
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lower");
    let module = nulang::mir_codegen::compile_mir(&mut mir, "arity_admission").expect("compile");
    let behavior_id = module.actor_metadata[0].behavior_indices[0] as u16;
    assert_eq!(module.behaviors[behavior_id as usize].param_count, 1);
    (module, behavior_id)
}

fn spawn_counter() -> (Runtime, u64, u16) {
    let (module, behavior_id) = compiled_counter();
    let mut rt = Runtime::new();
    let actor_id = rt
        .spawn_from_module(&module, behavior_id as usize, vec![])
        .as_actor_id()
        .expect("spawn persistent actor");
    (rt, actor_id, behavior_id)
}

#[test]
fn durable_actor_rejects_missing_argument_without_journaling() {
    let (mut rt, actor_id, behavior_id) = spawn_counter();
    let initial_journal = rt.persistence.read_journal(actor_id).len();

    rt.send_message_by_id(actor_id, behavior_id, &[]);
    rt.run_scheduler();

    assert_eq!(rt.persistence.read_journal(actor_id).len(), initial_journal);
    assert_eq!(
        rt.actors[&actor_id]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(0)
    );

    rt.send_message_by_id(actor_id, behavior_id, &[Value::int(4)]);
    rt.run_scheduler();

    assert_eq!(
        rt.persistence.read_journal(actor_id).len(),
        initial_journal + 1
    );
    assert_eq!(
        rt.actors[&actor_id]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(4)
    );
}

#[test]
fn durable_actor_rejects_extra_argument_without_journaling() {
    let (mut rt, actor_id, behavior_id) = spawn_counter();
    let initial_journal = rt.persistence.read_journal(actor_id).len();

    rt.send_message_by_id(actor_id, behavior_id, &[Value::int(4), Value::int(99)]);
    rt.run_scheduler();

    assert_eq!(rt.persistence.read_journal(actor_id).len(), initial_journal);
    assert_eq!(
        rt.actors[&actor_id]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(0)
    );
}

#[test]
fn synchronous_actor_ask_rejects_wrong_arity_before_execution() {
    let (mut rt, actor_id, behavior_id) = spawn_counter();
    let error = rt
        .ask_actor_sync(actor_id, behavior_id, &[])
        .expect_err("wrong argument count must be rejected");
    assert!(error.to_string().contains("expects 1 argument"), "{error}");
    assert_eq!(
        rt.actors[&actor_id]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(0)
    );
}

#[test]
fn malformed_queued_message_is_not_executed_during_synchronous_mailbox_flush() {
    let (mut rt, actor_id, behavior_id) = spawn_counter();
    rt.send_message_by_id(actor_id, behavior_id, &[]);
    let result = rt
        .ask_actor_sync(actor_id, behavior_id, &[Value::int(7)])
        .expect("valid ask executes");
    let _ = result;
    assert_eq!(
        rt.actors[&actor_id]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(7)
    );
}
