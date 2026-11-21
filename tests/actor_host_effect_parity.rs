//! Source-to-bytecode regression for standard host effects in programs
//! that declare/spawn actors. Standalone VM and actor-backed VM must agree.

use std::cell::RefCell;
use std::rc::Rc;

use nulang::lexer::Lexer;
use nulang::mir_codegen::compile_mir;
use nulang::parser::Parser;
use nulang::runtime::{Runtime, RuntimeVmCallbacks};
use nulang::typechecker::TypeChecker;
use nulang::vm::{Value, VM};

fn compile_source(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).unwrap();
    let hir = nulang::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).unwrap();
    compile_mir(&mut mir, "actor-host-effect-parity").unwrap()
}

fn source_with_actor(expr: &str) -> String {
    format!(
        "actor Probe {{ behavior ping() {{ 1 }} }}\n\
         fn main() {{\n\
             let probe = spawn Probe {{}}\n\
             {expr}\n\
         }}\n"
    )
}

fn run_string(source: &str, actor_backed: bool) -> String {
    let mut vm = VM::new();
    vm.load_module(compile_source(source));
    if actor_backed {
        let runtime = Rc::new(RefCell::new(Runtime::new()));
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime)));
    }
    // Resolve while VM and its callback-owned heap are still alive.
    let value = vm.run().expect("source program must execute");
    nulang::vm::resolve_value_string(&[], value)
}

fn run_boolean(source: &str, actor_backed: bool) -> Value {
    let mut vm = VM::new();
    vm.load_module(compile_source(source));
    if actor_backed {
        let runtime = Rc::new(RefCell::new(Runtime::new()));
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime)));
    }
    vm.run().expect("source program must execute")
}

#[test]
fn actor_backed_env_get_matches_standalone_vm() {
    let source = source_with_actor("perform Env.get(\"PATH\")");
    assert_eq!(run_string(&source, true), run_string(&source, false));
}

#[test]
fn actor_backed_system_arg_matches_standalone_vm() {
    let source = source_with_actor("perform System.arg(0)");
    let actual = run_string(&source, true);
    assert_eq!(actual, run_string(&source, false));
    assert_eq!(actual, std::env::args().next().unwrap());
}

#[test]
fn actor_backed_fs_read_and_exists_match_standalone_vm() {
    let unique = format!(
        "nulang-actor-effect-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let file = std::env::temp_dir().join(unique);
    std::fs::write(&file, "effect-parity-payload").unwrap();
    // A JSON string literal has the same escaping needed for a Nulang path.
    let path = serde_json::to_string(&file.to_string_lossy().into_owned()).unwrap();
    let read_source = source_with_actor(&format!("perform FS.read({path})"));
    assert_eq!(run_string(&read_source, true), "effect-parity-payload");
    assert_eq!(run_string(&read_source, true), run_string(&read_source, false));

    let exists_source = source_with_actor(&format!("perform FS.exists({path})"));
    assert_eq!(run_boolean(&exists_source, true), Value::bool(true));
    assert_eq!(
        run_boolean(&exists_source, true),
        run_boolean(&exists_source, false)
    );
    std::fs::remove_file(file).unwrap();
}
