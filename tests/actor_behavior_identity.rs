use nulang::hir::{Decl, RValue as HirRValue, Stmt as HirStmt};
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

fn hir_contains_behavior(body: &nulang::hir::Body, expected: &str) -> bool {
    body.stmts.iter().any(|stmt| match stmt {
        HirStmt::Let { value, .. } => match value {
            HirRValue::Send { behavior, .. } | HirRValue::Ask { behavior, .. } => {
                behavior == expected
            }
            HirRValue::Block(inner) => hir_contains_behavior(inner, expected),
            _ => false,
        },
        _ => false,
    })
}

fn mir_send_indices(module: &nulang::mir::Module) -> Vec<usize> {
    module
        .functions
        .iter()
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

#[test]
fn duplicate_short_behavior_name_lowers_to_receiver_schema() {
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
        Decl::Function(function) => hir_contains_behavior(&function.body, "Second.hit"),
        _ => false,
    }));

    let first_hit = mir
        .behaviors
        .iter()
        .position(|behavior| behavior.name == "First.hit")
        .expect("First.hit MIR behavior");
    let second_hit = mir
        .behaviors
        .iter()
        .position(|behavior| behavior.name == "Second.hit")
        .expect("Second.hit MIR behavior");
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

    let work = mir
        .behaviors
        .iter()
        .position(|behavior| behavior.name == "Worker.work")
        .expect("Worker.work MIR behavior");
    assert!(mir_send_indices(&mir).contains(&work));
}
