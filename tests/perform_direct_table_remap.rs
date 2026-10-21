//! Regression for module-global handler-table indices in PerformDirect.
//!
//! MIR HandlerRef.table_index is function-local, but bytecode tables are
//! module-global. A direct perform in a later function must target the same
//! table as that function's Handle instruction.

use nulang::bytecode::{CodeModule, OpCode};
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;

fn compile(source: &str) -> CodeModule {
    let tokens = Lexer::new(source).lex().expect("source should lex");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("source should parse");
    let mut checker = TypeChecker::new();
    checker
        .check_module(&ast)
        .expect("source should type-check");
    let hir = hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).expect("MIR lowering should succeed");
    mir_codegen::compile_mir(&mut mir, "perform-direct-table-regression")
        .expect("MIR code generation should succeed")
}

#[test]
fn direct_effects_in_different_functions_use_their_own_global_table_indices() {
    let module = compile(
        r#"
        fn first() -> Int {
            handle perform First.get() { | First.get() resume => 11 }
        }
        fn main() -> Int {
            handle perform Second.get() { | Second.get() resume => 22 }
        }
        "#,
    );

    assert_eq!(module.handler_tables.len(), 2, "each function owns a handler table");
    for effect in ["First.get", "Second.get"] {
        let global_table_index = module
            .handler_tables
            .iter()
            .position(|table| table.bindings.iter().any(|binding| binding.effect_name == effect))
            .expect("the handler table must be installed");
        let site = module
            .effect_sites
            .iter()
            .find(|site| site.effect_operation == effect)
            .expect("each effect must have an artifact site");
        let perform = module.instructions[site.pc];
        assert_eq!(
            perform.opcode,
            OpCode::PerformDirect,
            "{effect} should use statically resolved dispatch",
        );
        assert_eq!(
            perform.op1 as usize,
            global_table_index,
            "{effect} must encode the module-global handler table index",
        );
    }

    let mut vm = VM::new();
    vm.load_module(module);
    let result = vm.run().expect("main's handler should execute correctly");
    assert_eq!(result.as_int(), Some(22));
}
