use nulang::hir::{Decl, Operand as HirOperand, RValue as HirRValue, Stmt as HirStmt};
use nulang::lexer::Lexer;
use nulang::mir::{RValue as MirRValue, Stmt as MirStmt};
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

fn lower(source: &str) -> (nulang::hir::Module, nulang::mir::Module) {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let mir = nulang::mir_lower::lower_module(&hir).expect("MIR lowering");
    (hir, mir)
}

fn hir_contains_nominal_dispatch(
    body: &nulang::hir::Body,
    actor_schema: &str,
    behavior_name: &str,
) -> bool {
    body.stmts.iter().any(|stmt| match stmt {
        HirStmt::Let {
            value: HirRValue::Block(inner),
            ..
        } => {
            let has_alias = inner.stmts.iter().any(|stmt| {
                matches!(
                    stmt,
                    HirStmt::Let {
                        name,
                        value: HirRValue::Use(_),
                        ..
                    } if name == actor_schema
                )
            });
            let has_dispatch = inner.stmts.iter().any(|stmt| match stmt {
                HirStmt::Let {
                    value:
                        HirRValue::Send {
                            actor: HirOperand::Var(actor, _),
                            behavior,
                            ..
                        }
                        | HirRValue::Ask {
                            actor: HirOperand::Var(actor, _),
                            behavior,
                            ..
                        },
                    ..
                } => actor == actor_schema && behavior == behavior_name,
                _ => false,
            });
            (has_alias && has_dispatch)
                || hir_contains_nominal_dispatch(inner, actor_schema, behavior_name)
        }
        _ => false,
    })
}

fn mir_send_indices(module: &nulang::mir::Module) -> Vec<usize> {
    module
        .functions
        .iter()
        .chain(module.behaviors.iter())
        .flat_map(|function| function.blocks.iter())
        .flat_map(|block| block.stmts.iter())
        .filter_map(|stmt| match stmt {
            MirStmt::Assign {
                op: MirRValue::Send { behavior_idx, .. },
                ..
            } => Some(*behavior_idx),
            _ => None,
        })
        .collect()
}

fn mir_ask_indices(module: &nulang::mir::Module) -> Vec<usize> {
    module
        .functions
        .iter()
        .chain(module.behaviors.iter())
        .flat_map(|function| function.blocks.iter())
        .flat_map(|block| block.stmts.iter())
        .filter_map(|stmt| match stmt {
            MirStmt::Assign {
                op: MirRValue::Ask { behavior_idx, .. },
                ..
            } => Some(*behavior_idx),
            _ => None,
        })
        .collect()
}

fn behavior_index(module: &nulang::mir::Module, name: &str) -> usize {
    module
        .behaviors
        .iter()
        .position(|behavior| behavior.name == name)
        .unwrap_or_else(|| panic!("missing MIR behavior {name}"))
}

#[test]
fn duplicate_short_behavior_name_lowers_send_to_receiver_schema() {
    let (hir, mir) = lower(
        r#"
        actor First {
            behavior hit(value: Int) { nil }
        }

        actor Second {
            behavior hit(value: String) { nil }
        }

        fn main() {
            let target = spawn Second {}
            send target hit("second")
        }
        "#,
    );

    assert!(hir.decls.iter().any(|decl| match decl {
        Decl::Function(function) => {
            hir_contains_nominal_dispatch(&function.body, "Second", "hit")
        }
        _ => false,
    }));

    let first_hit = behavior_index(&mir, "First.hit");
    let second_hit = behavior_index(&mir, "Second.hit");
    let send_indices = mir_send_indices(&mir);

    assert!(
        send_indices.contains(&second_hit),
        "send to Second.hit must use behavior slot {second_hit}; got {send_indices:?}"
    );
    assert!(
        !send_indices.contains(&first_hit),
        "send to Second.hit must never resolve First.hit slot {first_hit}"
    );
}

#[test]
fn duplicate_short_behavior_name_lowers_ask_to_receiver_schema() {
    let (hir, mir) = lower(
        r#"
        actor First {
            behavior read() -> Int { 1 }
        }

        actor Second {
            behavior read() -> String { "second" }
        }

        fn main() {
            let target = spawn Second {}
            ask target read()
        }
        "#,
    );

    assert!(hir.decls.iter().any(|decl| match decl {
        Decl::Function(function) => {
            hir_contains_nominal_dispatch(&function.body, "Second", "read")
        }
        _ => false,
    }));

    let first_read = behavior_index(&mir, "First.read");
    let second_read = behavior_index(&mir, "Second.read");
    let ask_indices = mir_ask_indices(&mir);

    assert!(ask_indices.contains(&second_read));
    assert!(!ask_indices.contains(&first_read));
}

#[test]
fn copied_actor_reference_preserves_nominal_schema() {
    let (hir, mir) = lower(
        r#"
        actor First {
            behavior hit(value: Int) { nil }
        }

        actor Second {
            behavior hit(value: String) { nil }
        }

        fn main() {
            let original = spawn Second {}
            let copy = original
            send copy hit("second")
        }
        "#,
    );

    assert!(hir.decls.iter().any(|decl| match decl {
        Decl::Function(function) => {
            hir_contains_nominal_dispatch(&function.body, "Second", "hit")
        }
        _ => false,
    }));

    let first_hit = behavior_index(&mir, "First.hit");
    let second_hit = behavior_index(&mir, "Second.hit");
    let send_indices = mir_send_indices(&mir);
    assert!(send_indices.contains(&second_hit));
    assert!(!send_indices.contains(&first_hit));
}

#[test]
fn conditional_actor_reference_preserves_nominal_schema_when_all_paths_agree() {
    let (hir, mir) = lower(
        r#"
        actor First {
            behavior hit(value: Int) { nil }
        }

        actor Second {
            behavior hit(value: String) { nil }
        }

        fn main() {
            let target = if true then {
                let candidate = spawn Second {}
                candidate
            } else {
                let candidate = spawn Second {}
                candidate
            }
            send target hit("second")
        }
        "#,
    );

    assert!(hir.decls.iter().any(|decl| match decl {
        Decl::Function(function) => {
            hir_contains_nominal_dispatch(&function.body, "Second", "hit")
        }
        _ => false,
    }));

    let first_hit = behavior_index(&mir, "First.hit");
    let second_hit = behavior_index(&mir, "Second.hit");
    let send_indices = mir_send_indices(&mir);
    assert!(send_indices.contains(&second_hit));
    assert!(!send_indices.contains(&first_hit));
}

#[test]
fn self_dispatch_preserves_enclosing_actor_schema() {
    let (hir, mir) = lower(
        r#"
        actor First {
            behavior hit() { nil }
        }

        actor Second {
            behavior hit() { nil }
            behavior relay() {
                send self hit()
            }
        }
        "#,
    );

    assert!(hir.decls.iter().any(|decl| match decl {
        Decl::Actor(actor) if actor.name == "Second" => actor.behaviors.iter().any(|behavior| {
            behavior.name == "relay"
                && hir_contains_nominal_dispatch(&behavior.body, "Second", "hit")
        }),
        _ => false,
    }));

    let first_hit = behavior_index(&mir, "First.hit");
    let second_hit = behavior_index(&mir, "Second.hit");
    let send_indices = mir_send_indices(&mir);
    assert!(send_indices.contains(&second_hit));
    assert!(!send_indices.contains(&first_hit));
}

#[test]
fn unique_dynamic_short_behavior_remains_lowerable() {
    let (_hir, mir) = lower(
        r#"
        actor Worker {
            behavior work(value: Int) { nil }
        }

        fn relay(target) {
            send target work(1)
        }
        "#,
    );

    let work = behavior_index(&mir, "Worker.work");
    assert!(mir_send_indices(&mir).contains(&work));
}
