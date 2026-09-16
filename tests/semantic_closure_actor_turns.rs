//! Semantic-closure regression tests for actor turn isolation.
//!
//! A suspended actor turn must remain the actor's only live turn. Messages
//! may queue while the turn is suspended, but they must not start another
//! behavior until the suspended VM activation resumes or terminates.

use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::runtime::Runtime;
use nulang::typechecker::TypeChecker;

const SUSPENDED_TURN_SOURCE: &str = r#"
actor Gate {
    state phase: Int = 0
    state marked: Int = 0

    behavior start() {
        self.phase = 1
        let resumed = receive {
            | wake() => 2
        } after 5000 => 3
        self.phase = resumed
    }

    behavior mark() {
        self.marked = self.marked + 1
    }

    behavior wake() { 0 }
}
"#;

fn compile_module(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("source should lex");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("source should parse");
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("source should type-check");
    let hir = hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).expect("source should lower to MIR");
    mir_codegen::compile_mir(&mut mir, "semantic-closure-actor-turns").expect("MIR should compile")
}

fn state_int(runtime: &Runtime, actor_id: u64, field: &str) -> i64 {
    runtime
        .actors
        .get(&actor_id)
        .and_then(|actor| actor.get_state_field(field))
        .and_then(|value| value.as_int())
        .unwrap_or_else(|| panic!("missing integer state field {field}"))
}

#[test]
fn suspended_actor_turn_is_non_reentrant_until_resume() {
    let module = compile_module(SUSPENDED_TURN_SOURCE);
    let behavior_idx = module.actor_metadata[0].behavior_indices[0];
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, behavior_idx, vec![])
        .as_actor_id()
        .expect("spawn should return an actor reference");

    runtime.send_message(actor_id, "start", &[]);
    runtime.step_actor(actor_id);

    {
        let actor = runtime.actors.get(&actor_id).expect("actor should exist");
        assert!(
            actor.suspended_execution.is_some(),
            "the start turn must be suspended inside receive-after"
        );
    }
    assert_eq!(state_int(&runtime, actor_id, "phase"), 1);
    assert_eq!(state_int(&runtime, actor_id, "marked"), 0);

    // This message does not match the live receive arm. The runtime may wake
    // and re-scan the suspended ReceiveWait, but it must re-suspend the SAME
    // activation instead of dispatching `mark` as a second actor turn.
    runtime.send_message(actor_id, "mark", &[]);
    assert_eq!(state_int(&runtime, actor_id, "marked"), 0);
    assert!(
        runtime
            .actors
            .get(&actor_id)
            .expect("actor should exist")
            .suspended_execution
            .is_some(),
        "a non-matching queued message must not end the suspended turn"
    );

    // Even an explicit scheduler step must not enter another behavior while
    // the actor owns a suspended VM activation.
    runtime.step_actor(actor_id);
    assert_eq!(state_int(&runtime, actor_id, "marked"), 0);
    assert!(
        runtime
            .actors
            .get(&actor_id)
            .expect("actor should exist")
            .suspended_execution
            .is_some(),
        "step_actor must refuse re-entry while suspension is live"
    );

    // A matching receive message resumes the original activation. `wake` is
    // consumed by the receive expression, so phase becomes 2; the previously
    // queued `mark` remains pending for the next actor turn.
    runtime.send_message(actor_id, "wake", &[]);
    assert_eq!(state_int(&runtime, actor_id, "phase"), 2);
    assert_eq!(state_int(&runtime, actor_id, "marked"), 0);
    assert!(
        runtime
            .actors
            .get(&actor_id)
            .expect("actor should exist")
            .suspended_execution
            .is_none(),
        "matching receive must complete the suspended activation"
    );

    runtime.step_actor(actor_id);
    assert_eq!(
        state_int(&runtime, actor_id, "marked"),
        1,
        "queued work may run only after the suspended turn has completed"
    );
}
