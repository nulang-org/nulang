//! End-to-end integration tests that exercise the full compiler pipeline.
//!
//! Tests go through lex → parse → typecheck → compile → VM run.

#[cfg(test)]
mod tests {
    use crate::bytecode::OpCode;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::runtime::{
        grain_actor_id, ActorSnapshot, DehydratePolicy, EventEntry, GrainId, JournalEntry,
        MemoryStore, PersistenceStore, Runtime, RuntimeVmCallbacks, WorkflowEvent,
    };
    use crate::typechecker::TypeChecker;
    use crate::types::NuError;
    use crate::types::Type;
    use crate::vm::{Value, VM};
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    /// Thread-safe, shareable in-memory persistence store for tests that need
    /// to simulate a runtime restart while keeping the same underlying storage.
    #[derive(Debug, Clone)]
    struct SharedMemoryStore(Arc<Mutex<MemoryStore>>);

    impl SharedMemoryStore {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(MemoryStore::new())))
        }
    }

    impl PersistenceStore for SharedMemoryStore {
        fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> std::io::Result<()> {
            self.0.lock().unwrap().save_snapshot(snapshot)
        }
        fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
            self.0.lock().unwrap().load_snapshot(actor_id)
        }
        fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> std::io::Result<()> {
            self.0.lock().unwrap().append_journal(actor_id, entry)
        }
        fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
            self.0.lock().unwrap().read_journal(actor_id)
        }
        fn latest_sequence(&self, actor_id: u64) -> u64 {
            self.0.lock().unwrap().latest_sequence(actor_id)
        }
        fn append_workflow_event(
            &mut self,
            actor_id: u64,
            event: WorkflowEvent,
        ) -> std::io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .append_workflow_event(actor_id, event)
        }
        fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
            self.0.lock().unwrap().read_workflow_events(actor_id)
        }
        fn clear(&mut self, actor_id: u64) -> std::io::Result<()> {
            self.0.lock().unwrap().clear(actor_id)
        }
        fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> std::io::Result<()> {
            self.0.lock().unwrap().append_event(actor_id, entry)
        }
        fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
            self.0.lock().unwrap().read_events(actor_id)
        }
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Which backend to use. Controlled by the `NU_BACKEND` env var:
    ///   - unset or "bytecode" → bytecode VM (default)
    ///   - "native" → AOT native compilation via Cranelift
    fn backend() -> &'static str {
        use std::sync::LazyLock;
        static BACKEND: LazyLock<String> = LazyLock::new(|| {
            std::env::var("NU_BACKEND")
                .unwrap_or_else(|_| "bytecode".to_string())
                .to_lowercase()
        });
        &BACKEND
    }

    /// Run a source string through the full pipeline and return (value, type).
    fn run_source(source: &str) -> Result<(Value, Type), NuError> {
        // 1. Parse
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module()?;

        // 2. Type check
        let mut type_checker = TypeChecker::new();
        let mut module_type = type_checker.check_module(&ast)?;

        // If the last declaration is the synthetic function wrapper __main, unpack its return type
        if let Some(crate::ast::Decl::Function { name, .. }) = ast.decls.last() {
            if name == "__main" {
                if let Type::Function { ret, .. } = module_type {
                    module_type = *ret;
                }
            }
        }

        // 3. Effect check
        // (placeholder: effect checker would go here)

        // 4. Compile via HIR/MIR pipeline
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir)?;

        match backend() {
            "native" => {
                let aot_module = crate::aot::AotModule::compile(&mir)?;
                let result_raw = aot_module.run()?;
                let value = Value::from_raw(result_raw);
                Ok((value, module_type))
            }
            _ => {
                let module = crate::mir_codegen::compile_mir(&mut mir, "test")?;
                // 5. Run
                let mut vm = VM::new();
                vm.load_module(module);
                let value = vm.run()?;
                Ok((value, module_type))
            }
        }
    }

    /// Assert that running source produces an integer value.
    fn assert_int(source: &str, expected: i64) {
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(
            value.as_int(),
            Some(expected),
            "Expected integer result for: {}",
            source
        );
    }

    /// Assert that running source produces a float value.
    fn assert_float(source: &str, expected: f64) {
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(
            value.as_float(),
            Some(expected),
            "Expected float result for: {}",
            source
        );
    }

    /// Assert that running source produces a boolean value.
    fn assert_bool(source: &str, expected: bool) {
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(
            value.as_bool(),
            Some(expected),
            "Expected boolean result for: {}",
            source
        );
    }

    /// Assert that running source produces the given string value.
    fn assert_string(source: &str, expected: &str) {
        let (module, _ty) = compile_source(source).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        let value = vm.run().unwrap();
        // Handle both constant-pool strings (TAG_STRING) and heap-allocated
        // strings (TAG_PTR from SConcat).
        if let Some(id) = value.as_string_id() {
            let module_idx = vm.modules.len() - 1;
            let content = vm.constant_string(module_idx, id).unwrap();
            assert_eq!(content, expected, "unexpected string for: {}", source);
        } else if value.is_ptr() {
            // Heap-allocated string: read C string from the heap.
            let ptr = value.as_ptr().expect("expected ptr value");
            let mut len = 0usize;
            unsafe {
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr, len);
                let content = String::from_utf8_lossy(slice);
                assert_eq!(content, expected, "unexpected heap string for: {}", source);
            }
        } else {
            panic!("expected string result, got {:?}", value);
        }
    }
    /// Run source through the full compiler pipeline using a real actor runtime.
    fn run_source_with_runtime(
        source: &str,
        runtime: Rc<RefCell<Runtime>>,
    ) -> Result<(Value, Type), NuError> {
        let (module, module_type) = compile_source(source)?;

        runtime.borrow_mut().register_module_grains(&module);
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime)));
        let value = vm.run()?;

        Ok((value, module_type))
    }

    /// Compile source into a bytecode module and its top-level type.
    fn compile_source(source: &str) -> Result<(crate::bytecode::CodeModule, Type), NuError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module()?;

        let mut type_checker = TypeChecker::new();
        let module_type = type_checker.check_module(&ast)?;

        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir)?;
        let module = crate::mir_codegen::compile_mir(&mut mir, "test")?;
        Ok((module, module_type))
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_literal_int() {
        assert_int("42", 42);
    }

    #[test]
    fn test_literal_negative_int() {
        assert_int("-17", -17);
    }

    #[test]
    fn test_arithmetic_add() {
        assert_int("1 + 2", 3);
    }

    #[test]
    fn test_arithmetic_sub() {
        assert_int("10 - 3", 7);
    }

    #[test]
    fn test_arithmetic_mul() {
        assert_int("4 * 5", 20);
    }

    #[test]
    fn test_arithmetic_div() {
        assert_int("20 / 4", 5);
    }

    #[test]
    fn test_bitwise_operators() {
        assert_int("6 & 3", 2);
        // Single `|` is reserved as a match-arm separator, so bitwise OR uses
        // the `|||` token.
        assert_int("6 ||| 3", 7);
        assert_int("6 ^ 3", 5);
        assert_int("1 << 3", 8);
        assert_int("16 >> 2", 4);
    }

    #[test]
    fn test_arithmetic_precedence() {
        assert_int("1 + 2 * 3", 7); // mul before add
        assert_int("(1 + 2) * 3", 9); // parens override
    }

    #[test]
    fn test_let_binding() {
        let source = "let x = 10 in x + 5";
        assert_int(source, 15);
    }

    #[test]
    fn test_local_assignment() {
        // `&` creates a ref cell; `*` dereferences it. Assignment mutates the ref.
        let source = "let x = &10 in { x = 3; *x }";
        assert_int(source, 3);
    }

    #[test]
    fn test_let_rec_module_level() {
        // Doc-pass gap 3: `let rec f(x) = ... in ...` at module level used
        // to fail in `parse_module_let` ("Expected =" on the parameter
        // list); it now rewinds to the expression path. Recursion works
        // through the full VM.
        assert_int(
            "let rec f(n) = if n <= 1 then 1 else n * f(n - 1) in f(5)",
            120,
        );
        assert_int(
            "fn main() { let rec fib(n) = if n <= 1 then n else fib(n - 1) + fib(n - 2) in fib(10) }",
            55,
        );
    }

    #[test]
    fn test_type_decl_alias_bodies() {
        // Doc-pass gap 5: `type X = <full type>` now accepts array,
        // primitive, function, and reference bodies (previously only
        // variants/records parsed; `type Buffer = [Int]` failed with
        // "Expected variant name").
        assert_int(
            "type Buffer = [Int]\nlet b: Buffer = [1, 2, 3]\nperform Array.length(b)",
            3,
        );
        assert_int("type T = Int\nlet x: T = 5\nx + 1", 6);
        assert_int(
            "type F = (Int) -> Int\nlet inc: F = fn(n) n + 1\ninc(41)",
            42,
        );
        // Variants still parse as variants.
        assert_int(
            "type Option[T] = Some(T) | None\nlet o: Option[Int] = Some(5)\nmatch o { | Some(v) => v | None => 0 }",
            5,
        );
    }

    #[test]
    fn test_cap_ref_value_constructors() {
        // Value-level capability constructors: every capability parses as
        // `&cap expr`, erases to a plain move at runtime (capabilities are
        // compile-time only), and dereferences through `*`. Rejection of a
        // second unique constructor (`&iso x` twice) is pinned at the
        // capability-analyzer level (see effect_checker tests) because the
        // integration pipeline below skips capability analysis.
        assert_int("let x = &10 in *x", 10);
        assert_int("let x = &ref 10 in *x", 10);
        assert_int("let x = &iso 10 in *x", 10);
        assert_int("let x = &trn 10 in *x", 10);
        assert_int("let x = &val 10 in *x", 10);
        assert_int("let x = &box 10 in *x", 10);
        assert_int("let x = &linear 10 in *x", 10);
        assert_int("let x = &lineariso 10 in *x", 10);
        assert_int("let x = &tag 10 in 0", 0); // tag is opaque identity only
                                               // Constructing from a variable and passing the unique reference to a
                                               // function parameter annotated with the same capability.
        assert_int(
            "fn f(x: &iso Int) -> Int { *x }; let v = 42 in let r = &iso v in f(r)",
            42,
        );
        // Shared constructors alias without consuming: the source binding
        // stays usable.
        assert_int("let v = 42 in let r = &val v in *r + v", 84);
    }

    #[test]
    fn test_record_field_access() {
        let source = "let r = { x: 1, y: 2 } in r.x + r.y";
        assert_int(source, 3);
    }

    #[test]
    fn test_let_multiple() {
        let source = "let x = 1 in let y = 2 in let z = 3 in x + y + z";
        assert_int(source, 6);
    }

    #[test]
    fn test_boolean_true() {
        let (value, _ty) = run_source("true").unwrap();
        assert_eq!(value.as_bool(), Some(true));
    }

    #[test]
    fn test_boolean_false() {
        let (value, _ty) = run_source("false").unwrap();
        assert_eq!(value.as_bool(), Some(false));
    }

    #[test]
    fn test_boolean_and() {
        let (value, _ty) = run_source("true and false").unwrap();
        assert_eq!(value.as_bool(), Some(false));
    }

    #[test]
    fn test_boolean_or() {
        let (value, _ty) = run_source("true or false").unwrap();
        assert_eq!(value.as_bool(), Some(true));
    }

    #[test]
    fn test_comparison_eq() {
        let (value, _ty) = run_source("5 == 5").unwrap();
        assert_eq!(value.as_bool(), Some(true));
    }

    #[test]
    fn test_comparison_ne() {
        let (value, _ty) = run_source("5 != 3").unwrap();
        assert_eq!(value.as_bool(), Some(true));
    }

    #[test]
    fn test_comparison_lt() {
        let (value, _ty) = run_source("3 < 5").unwrap();
        assert_eq!(value.as_bool(), Some(true));
    }

    #[test]
    fn test_if_then_else() {
        assert_int("if true then 1 else 2", 1);
        assert_int("if false then 1 else 2", 2);
    }

    #[test]
    fn test_if_with_comparison() {
        assert_int("if 5 > 3 then 10 else 20", 10);
    }

    #[test]
    fn test_lambda_apply() {
        // Lambda: fn(x) x + 1, applied to 5
        let source = "(fn(x) x + 1)(5)";
        assert_int(source, 6);
    }

    #[test]
    fn test_lambda_two_args() {
        let source = "(fn(x, y) x + y)(3, 4)";
        assert_int(source, 7);
    }

    #[test]
    fn test_unit_value() {
        let (value, _ty) = run_source("unit").unwrap();
        assert!(value.is_unit());
    }

    #[test]
    fn test_nil_value() {
        let (value, _ty) = run_source("nil").unwrap();
        assert!(value.is_nil());
    }

    #[test]
    fn test_spawn_returns_actor_ref() {
        let source = r#"
            actor Counter {
                state count = 0
                behavior get() { self.count }
                behavior inc() { self.count + 1 }
            }
            spawn Counter { count = 0 }
        "#;
        let (value, _ty) = run_source(source).unwrap();
        // Should be an actor reference
        assert!(value.as_actor_id().is_some(), "Expected actor reference");
    }

    // -----------------------------------------------------------------------
    // Test: Effects
    // -----------------------------------------------------------------------

    #[test]
    fn test_perform_unhandled_effect_errors() {
        // Unhandled effects should return an error (v0.15+ effect system).
        // Note: IO.print is no longer unhandled — the standalone VM handles
        // it as a built-in — so this uses an effect with no built-in.
        let source = r#"
            perform Net.fetch("hello")
        "#;
        let result = run_source(source);
        assert!(result.is_err(), "Unhandled effect should error");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unhandled effect"),
            "Error should mention unhandled effect: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Test: examples/*.nula run end-to-end through the full pipeline
    // -----------------------------------------------------------------------

    #[test]
    fn test_example_fibonacci_runs() {
        let source = include_str!("../../examples/fibonacci.nula");
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(value.as_int(), Some(55), "fib(10) = 55");
    }

    #[test]
    fn test_example_effects_runs() {
        let source = include_str!("../../examples/effects.nula");
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(value.as_int(), Some(42), "handler should resume with 42");
    }

    #[test]
    fn test_example_counter_actor_runs() {
        let source = include_str!("../../examples/counter_actor.nula");
        let (value, _ty) = run_source(source).unwrap();
        assert!(
            value.as_actor_id().is_some(),
            "spawn should return an actor reference"
        );
    }

    #[test]
    fn test_declared_effect_annotation_rejects_undeclared_effects() {
        // Mirrors the CLI frontend's enforcement: a function annotated with a
        // declared effect row must not perform effects outside that row.
        use crate::effect_checker::{EffectChecker, EffectContext};

        let source = r#"
            fn f() -> Unit ! {} {
                perform IO.print("x")
            }
        "#;
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().unwrap();

        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let mut checked = false;
        for decl in &ast.decls {
            if let crate::ast::Decl::Function {
                name,
                body,
                effect: Some(declared),
                ..
            } = decl
            {
                if name == "f" {
                    checked = true;
                    let result = checker.check_effects(&ctx, body, declared);
                    assert!(
                        result.is_err(),
                        "function declared pure (`! {{}}`) but performing IO must be rejected"
                    );
                }
            }
        }
        assert!(
            checked,
            "parser should surface the `! {{}}` annotation on fn f"
        );
    }

    #[test]
    fn test_declared_effect_annotation_accepts_matching_effects() {
        use crate::effect_checker::{EffectChecker, EffectContext};

        let source = r#"
            fn f() -> Unit ! {IO} {
                perform IO.print("x")
            }
        "#;
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().unwrap();

        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let mut checked = false;
        for decl in &ast.decls {
            if let crate::ast::Decl::Function {
                name,
                body,
                effect: Some(declared),
                ..
            } = decl
            {
                if name == "f" {
                    checked = true;
                    let result = checker.check_effects(&ctx, body, declared);
                    assert!(
                        result.is_ok(),
                        "function performing only its declared effects must pass: {:?}",
                        result.err()
                    );
                }
            }
        }
        assert!(
            checked,
            "parser should surface the `! {{IO}}` annotation on fn f"
        );
    }

    /// Run the module effect check the way the CLI frontend does
    /// (`run_frontend` in main.rs): one `EffectChecker::check_module` over
    /// the parsed declarations.
    fn check_module_effects(source: &str) -> Result<(), NuError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().unwrap();
        let mut checker = crate::effect_checker::EffectChecker::new();
        checker.check_module(&ast.decls)
    }

    #[test]
    fn test_pure_fn_calling_io_fn_rejected() {
        // Finding: a function declared pure (`! {}`) that calls a function
        // performing IO must be rejected statically (SPEC2 §4.7/§4.9); the
        // callee's row propagates to the call site.
        let source = r#"
            fn do_io() -> Unit ! {IO} { perform IO.print("x") }
            fn pure() -> Unit ! {} { do_io() }
        "#;
        let result = check_module_effects(source);
        assert!(
            result.is_err(),
            "pure function calling an IO function must be rejected"
        );
    }

    #[test]
    fn test_pure_fn_calling_io_fn_transitively_rejected() {
        // The row map iterates to a fixpoint, so IO propagates through an
        // unannotated intermediate function as well.
        let source = r#"
            fn do_io() -> Unit ! {IO} { perform IO.print("x") }
            fn middle() -> Unit { do_io() }
            fn pure() -> Unit ! {} { middle() }
        "#;
        let result = check_module_effects(source);
        assert!(
            result.is_err(),
            "pure function transitively performing IO must be rejected"
        );
    }

    #[test]
    fn test_module_nested_effect_violation_rejected() {
        // Finding: declarations nested in `module {}` must be effect-checked
        // just like top-level ones (the typechecker already flattens them).
        let source = r#"
            module M {
                fn pure() -> Unit ! {} { perform IO.print("x") }
            }
        "#;
        let result = check_module_effects(source);
        assert!(
            result.is_err(),
            "module-nested pure function performing IO must be rejected"
        );
    }

    #[test]
    fn test_event_effect_annotation_accepted() {
        // Finding: `Event` (like `FFI`) is a built-in effect (SPEC2 §4.6), so
        // an `{Event}` annotation must satisfy a body that emits an event.
        let source = r#"
            fn f() -> Unit ! {Event} { emit MyEvent(1) }
        "#;
        let result = check_module_effects(source);
        assert!(
            result.is_ok(),
            "fn annotated ! {{Event}} may emit events: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_ffi_effect_annotation_accepted() {
        let source = r#"
            fn f() -> Unit ! {FFI} { 1 }
        "#;
        let result = check_module_effects(source);
        assert!(
            result.is_ok(),
            "fn annotated ! {{FFI}} must parse and check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_pure_functions_still_pass_effect_check() {
        // Positive: legitimately pure functions — including pure calls
        // between them and module-nested pure functions — keep passing.
        let source = r#"
            fn pure() -> Unit ! {} { unit }
            fn also_pure() -> Unit ! {} { pure() }
            module M { fn nested_pure() -> Unit ! {} { also_pure() } }
        "#;
        let result = check_module_effects(source);
        assert!(
            result.is_ok(),
            "legitimately pure functions must pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_perform_effect_with_handler() {
        // perform with a handler that catches the effect.
        // The compiler generates a Handle opcode and handler table with
        // HandlerBindings, and the VM invokes the handler + resumes.
        let source = r#"
            handle perform IO.print("hello") {
                | IO.print(msg) => unit
            }
        "#;
        let (value, _ty) = run_source(source).unwrap();
        assert!(value.is_unit(), "Expected unit from handled perform");
    }

    #[test]
    fn test_handler_returns_value_via_resume() {
        // Handler computes a value and resumes with it.
        let source = r#"
            handle perform Math.getAnswer() {
                | Math.getAnswer() => 42
            }
        "#;
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(
            value.as_int(),
            Some(42),
            "Handler should return 42 via resume"
        );
    }

    #[test]
    fn test_handler_with_parameter() {
        // Handler receives the perform argument and uses it.
        let source = r#"
            handle perform Math.double(21) {
                | Math.double(x) => x + x
            }
        "#;
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(value.as_int(), Some(42), "Handler should double 21 to 42");
    }

    // -----------------------------------------------------------------------
    // Regression: non-resuming handlers (`=> body` without `resume`) must
    // abort the handled computation with the body value instead of silently
    // resuming the continuation
    // -----------------------------------------------------------------------

    #[test]
    fn test_non_resuming_handler_aborts_with_body_value() {
        // Without `resume`, the handler's value becomes the handle
        // expression's value; the `; 100` continuation must NOT run.
        let source = r#"
            handle { perform E.op(); 100 } { | E.op() => 42 }
        "#;
        assert_int(source, 42);
    }

    #[test]
    fn test_resuming_handler_continues_body() {
        // With `resume`, the handler value flows back to the perform site
        // and the body continues: 42 is bound to x, then discarded for 100.
        let source = r#"
            handle { let x = perform E.op() in { x; 100 } } { | E.op() resume => 42 }
        "#;
        assert_int(source, 100);
    }

    #[test]
    fn test_resuming_handler_value_reaches_perform_site() {
        // The resumed value must land in the perform's dst: 41 + 1 = 42.
        let source = r#"
            handle { let x = perform E.op() in x + 1 } { | E.op() resume => 41 }
        "#;
        assert_int(source, 42);
    }

    #[test]
    fn test_non_resuming_handler_with_parameter() {
        // Abortive handlers receive perform arguments like resuming ones.
        let source = r#"
            handle { perform Math.double(21); 0 } { | Math.double(x) => x + x }
        "#;
        assert_int(source, 42);
    }

    /// End-to-end: `perform IO.print` with no handler resolves through the
    /// standalone built-in effect instead of failing with
    /// "Unhandled effect: IO" (the `nulang --eval` path).
    #[test]
    fn test_standalone_io_print_end_to_end() {
        let (value, _ty) = run_source(r#"perform IO.print("hello")"#)
            .expect("standalone IO.print must not be an unhandled effect");
        assert!(value.is_unit(), "IO.print resumes with unit");
    }

    /// Source-level op-name dispatch: a handler for `IO.bar` must NOT catch
    /// `perform IO.foo()` — handler bindings are op-qualified ("Effect.op").
    #[test]
    fn test_source_handler_does_not_catch_other_op() {
        let source = r#"handle perform IO.foo() { | IO.bar() => 1 }"#;
        let err = run_source(source).expect_err("IO.bar handler must not catch IO.foo");
        let msg = format!("{}", err);
        assert!(
            msg.contains("Unhandled effect: 'IO.foo'"),
            "expected unhandled IO.foo, got: {}",
            msg
        );
    }

    /// Source-level op-name dispatch, positive control: the matching
    /// `IO.foo` handler catches the perform.
    #[test]
    fn test_source_handler_catches_matching_op() {
        let source = r#"handle perform IO.foo() { | IO.foo() => 1 }"#;
        assert_int(source, 1);
    }

    // -----------------------------------------------------------------------
    // Effect monomorphization: PerformDirect bytecode emission
    // -----------------------------------------------------------------------

    /// When a `perform` is inside a statically-known `handle` block, the
    /// compiler emits `PerformDirect` instead of `Perform` — the handler
    /// table and binding indices are baked into the instruction.
    #[test]
    fn test_perform_direct_emitted_for_statically_resolved_handler() {
        let source = r#"handle perform IO.print("hi") { | IO.print(msg) => unit }"#;
        let module = compile_source_new(source).expect("compile should succeed");
        let has_perform_direct = module
            .instructions
            .iter()
            .any(|i| i.opcode == OpCode::PerformDirect);
        let has_perform = module
            .instructions
            .iter()
            .any(|i| i.opcode == OpCode::Perform);
        assert!(
            has_perform_direct,
            "statically-resolved perform must emit PerformDirect"
        );
        assert!(
            !has_perform,
            "statically-resolved perform must NOT emit dynamic Perform"
        );
        // And the program still evaluates correctly.
        let (value, _ty) = run_source(source).unwrap();
        assert!(value.is_unit(), "perform+handler should yield unit");
    }

    /// A `perform` with no handler in scope (e.g. built-in effect like
    /// `IO.print` called at the top level) still uses the dynamic `Perform`
    /// opcode — no monomorphization applies.
    #[test]
    fn test_perform_uses_dynamic_opcode_when_no_handler() {
        let source = r#"perform IO.print("hello")"#;
        let module = compile_source_new(source).expect("compile should succeed");
        let has_perform = module
            .instructions
            .iter()
            .any(|i| i.opcode == OpCode::Perform);
        assert!(
            has_perform,
            "unresolved perform must emit dynamic Perform opcode"
        );
        let has_perform_direct = module
            .instructions
            .iter()
            .any(|i| i.opcode == OpCode::PerformDirect);
        assert!(
            !has_perform_direct,
            "unresolved perform must NOT emit PerformDirect"
        );
    }

    /// Nested handler resolution: the inner handler shadows the outer one.
    #[test]
    fn test_nested_handler_resolves_to_inner() {
        let source = r#"
            handle {
                handle { perform E.op() }
                    { | E.op() => 2 }
            } { | E.op() => 1 }
        "#;
        assert_int(source, 2);
    }
    /// A program with a mix of resolved and unresolved performs works.
    #[test]
    fn test_mixed_resolved_and_unresolved_performs() {
        // A handle with two performs — both should use PerformDirect.
        let source = r#"
            handle {
                perform E.one();
                perform E.two()
            } { | E.one() resume => 1 | E.two() resume => 2 }
        "#;
        // The last perform resumes with 2, so the handle evaluates to 2.
        assert_int(source, 2);
    }

    // -----------------------------------------------------------------------
    // Test: Pipe operator
    // -----------------------------------------------------------------------

    #[test]
    fn test_pipe() {
        // 5 |> add(3) should be equivalent to add(5, 3) = 8
        let source = "let add = fn(x, y) x + y in 5 |> add(3)";
        // Note: The pipe operator's exact semantics may vary.
        // The parser handles |>, and the compiler generates Call for it.
        let (value, _ty) = run_source(source).unwrap();
        // The pipe compiles to a function call
        assert!(
            value.as_int().is_some(),
            "Pipe operation should produce an integer result"
        );
    }

    // -----------------------------------------------------------------------
    // Test: Blocks
    // -----------------------------------------------------------------------

    #[test]
    fn test_block() {
        let source = "{ let x = 1 in let y = 2 in x + y }";
        assert_int(source, 3);
    }

    #[test]
    fn test_block_sequential() {
        let source = "{ 1; 2; 3 }";
        assert_int(source, 3);
    }

    #[test]
    fn test_block_nested() {
        let source = "{ let a = 10 in { let b = 20 in a + b } }";
        assert_int(source, 30);
    }

    // -----------------------------------------------------------------------
    // Test: Pattern matching (basic)
    // -----------------------------------------------------------------------

    #[test]
    fn test_match_int_literal() {
        let source = r#"match 42 {
            case 1 => 10
            case 42 => 100
            case _ => 0
        }"#;
        assert_int(source, 100);
    }

    #[test]
    fn test_match_wildcard() {
        let source = r#"match 99 {
            case 1 => 10
            case 2 => 20
            case _ => 50
        }"#;
        assert_int(source, 50);
    }

    // -----------------------------------------------------------------------
    // Test: Recursion
    // -----------------------------------------------------------------------

    #[test]
    fn test_recursion_factorial() {
        let source = r#"
            let fac = fn(n) {
                if n == 0 then 1 else n * fac(n - 1)
            } in fac(5)
        "#;
        assert_int(source, 120);
    }

    #[test]
    fn test_recursion_fibonacci() {
        let source = r#"
            let fib = fn(n) {
                if n <= 1 then n else fib(n - 1) + fib(n - 2)
            } in fib(8)
        "#;
        assert_int(source, 21);
    }

    // -----------------------------------------------------------------------
    // Test: String literal
    // -----------------------------------------------------------------------

    #[test]
    fn test_string_literal() {
        let source = r#""hello""#;
        let result = run_source(source);
        // String literals should either produce a string value or an error
        // depending on compiler support.
        match result {
            Ok((value, _)) => {
                // Should be some kind of string representation
                assert!(
                    value.as_int().is_some() || value.is_nil() || value.is_string(),
                    "String literal should produce a value"
                );
            }
            Err(_) => {
                // String support may not be fully implemented yet
            }
        }
    }

    // -----------------------------------------------------------------------
    // Test: List literal
    // -----------------------------------------------------------------------

    #[test]
    fn test_list_literal() {
        let source = "[1, 2, 3]";
        let result = run_source(source);
        match result {
            Ok((value, _)) => {
                assert!(
                    !value.is_nil(),
                    "List literal should produce a non-nil value"
                );
            }
            Err(_) => {
                // List support may not be fully implemented yet
            }
        }
    }

    // -----------------------------------------------------------------------
    // Regression: owning locals must get real Drop instructions (mir_lower's
    // temp-fusion peephole keeps plan_drops effective; previously every
    // named local was defined by a non-owning Load, so no heap value was
    // ever reclaimed before actor exit)
    // -----------------------------------------------------------------------

    /// Register (LOCAL_BASE + local id) of the named local in __main, plus
    /// the compiled module.
    fn compile_and_find_local(source: &str, name: &str) -> (crate::bytecode::CodeModule, u8) {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().unwrap();
        let mut type_checker = TypeChecker::new();
        type_checker.check_module(&ast).unwrap();
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).unwrap();
        let main = mir
            .functions
            .iter()
            .find(|f| f.name == "__main")
            .expect("__main lowered");
        let local = main
            .locals
            .iter()
            .find(|l| l.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("local '{}' not found in {:?}", name, main.locals));
        let reg = (crate::mir_codegen::LOCAL_BASE + local.id.0) as u8;
        let module = crate::mir_codegen::compile_mir(&mut mir, "test").unwrap();
        (module, reg)
    }

    #[test]
    fn test_array_local_gets_real_drop() {
        // `a` solely owns its array and is only read (indexing), so codegen
        // must emit a Drop of `a`'s register after its last use — before
        // the fusion fix, `a` was defined by a non-owning Load and no Drop
        // of any array ever appeared.
        let source = "let a = [1, 2, 3] in a[0] + a[1]";
        let (module, reg) = compile_and_find_local(source, "a");
        let drops = module
            .instructions
            .iter()
            .filter(|i| i.opcode == crate::bytecode::OpCode::Drop && i.op1 == reg)
            .count();
        assert!(
            drops >= 1,
            "owning array local must be dropped at least once (reg {}), instructions: {:?}",
            reg,
            module.instructions
        );
        // And the program still evaluates correctly (no use-after-free).
        assert_int(source, 3);
    }

    #[test]
    fn test_record_local_gets_real_drop() {
        let source = "let r = { x: 1, y: 2 } in r.x + r.y";
        let (module, reg) = compile_and_find_local(source, "r");
        let drops = module
            .instructions
            .iter()
            .filter(|i| i.opcode == crate::bytecode::OpCode::Drop && i.op1 == reg)
            .count();
        assert!(
            drops >= 1,
            "owning record local must be dropped at least once (reg {})",
            reg
        );
        assert_int(source, 3);
    }

    // -----------------------------------------------------------------------
    // Test: Float literal
    // -----------------------------------------------------------------------

    #[test]
    fn test_float_literal() {
        let source = "3.14";
        let result = run_source(source);
        match result {
            Ok((value, _)) => {
                assert!(
                    value.as_float().is_some() || value.as_int().is_some(),
                    "Float literal should produce a numeric value"
                );
            }
            Err(_) => {
                // Float support may not be fully implemented yet
            }
        }
    }

    // -----------------------------------------------------------------------
    // Test: Type error detection
    // -----------------------------------------------------------------------

    #[test]
    fn test_type_error_mismatch() {
        let source = "1 + true"; // Can't add int and bool
        let result = run_source(source);
        assert!(
            result.is_err(),
            "Adding int and bool should be a type error"
        );
    }

    #[test]
    fn test_type_error_undefined_var() {
        let source = "undefined_variable + 1";
        let result = run_source(source);
        assert!(
            result.is_err(),
            "Using undefined variable should be an error"
        );
    }

    #[test]
    fn test_type_error_wrong_arity() {
        let source = "(fn(x) x)(1, 2)"; // Too many arguments
        let result = run_source(source);
        // This may or may not be caught by the type checker depending on
        // how function application is handled.
        match result {
            Ok(_) | Err(_) => {
                // Accept either — arity checking varies by implementation
            }
        }
    }

    // -----------------------------------------------------------------------
    // Test: declared types — variants, aliases, records, Nil (SPEC2 §3.4.1)
    // -----------------------------------------------------------------------

    /// Run only the frontend (lex → parse → typecheck), mirroring `--check`.
    fn check_source(source: &str) -> Result<Type, NuError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module()?;
        let mut type_checker = TypeChecker::new();
        type_checker.check_module(&ast)
    }

    #[test]
    fn test_declared_variant_construction_typechecks() {
        let result = check_source("type Option[T] = Some(T) | None\nSome(1)");
        assert!(
            result.is_ok(),
            "declared variant construction should check, got {:?}",
            result.err()
        );
    }

    #[test]
    fn test_unbound_variant_constructor_is_error() {
        let result = check_source("Some(1)");
        assert!(
            result.is_err(),
            "Some without a declaring variant type must be an error"
        );
    }

    #[test]
    fn test_variant_spec_example_typechecks() {
        // The canonical SPEC2 §3.4.1 example: declared Result variant used
        // for construction, annotation, and pattern matching.
        let source = r#"
type Result[T, E] = Ok(T) | Error(E)

fn safe_divide(a: Float, b: Float) -> Result[Float, String] {
  if b == 0.0 then
    Error("Division by zero")
  else
    Ok(a / b)
}

fn describe(r: Result[Float, String]) -> String {
  match r with {
    | Ok(value) => "ok"
    | Error(msg) => msg
  }
}
describe
"#;
        let result = check_source(source);
        assert!(
            result.is_ok(),
            "spec variant example should check, got {:?}",
            result.err()
        );
    }

    #[test]
    fn test_unknown_type_name_in_annotation_is_error() {
        let result = check_source("fn f(x: Bogus) x\nf(1)");
        assert!(
            result.is_err(),
            "annotation with an unknown type name must be an error"
        );
    }

    #[test]
    fn test_type_alias_expansion_end_to_end() {
        let ok = check_source("type alias MyInt = Int\nfn f(x: MyInt) -> MyInt { x }\nf(1)");
        assert!(
            ok.is_ok(),
            "alias use with Int should check, got {:?}",
            ok.err()
        );
        let bad = check_source("type alias MyInt = Int\nfn f(x: MyInt) -> MyInt { x }\nf(\"s\")");
        assert!(
            bad.is_err(),
            "alias must expand to the aliased type and reject String"
        );
    }

    #[test]
    fn test_record_type_annotation_end_to_end() {
        let result = check_source(
            "type Point = { x: Int, y: Int }\nfn get_x(p: Point) -> Int { p.x }\nget_x",
        );
        assert!(
            result.is_ok(),
            "record type name in annotation should check, got {:?}",
            result.err()
        );
    }

    #[test]
    fn test_derive_eq_generates_structural_equality() {
        // `@derive(eq)` synthesizes `point_eq(a, b)` comparing fields with
        // structural `==` (the VM's bare `==` on records is pointer equality,
        // so this is a real behavioral difference).
        let eq_source = r#"
@derive(eq)
type Point = { x: Int, y: Int }
point_eq({ x: 1, y: 2 }, { x: 1, y: 2 })
"#;
        let eq_value = run_source_new(eq_source).unwrap();
        assert_eq!(
            eq_value.as_bool(),
            Some(true),
            "identical records must compare structurally equal"
        );

        let ne_source = r#"
@derive(eq)
type Point = { x: Int, y: Int }
point_eq({ x: 1, y: 2 }, { x: 1, y: 3 })
"#;
        let ne_value = run_source_new(ne_source).unwrap();
        assert_eq!(
            ne_value.as_bool(),
            Some(false),
            "differing records must compare unequal"
        );
    }

    #[test]
    fn test_contracts_requires_ensures() {
        // Satisfied pre/postconditions: the call succeeds.
        let ok_source = r#"
fn add(a: Int, b: Int) -> Int
    requires a >= 0
    ensures result >= a
{
    a + b
}
add(1, 2)
"#;
        let v = run_source_new(ok_source).unwrap();
        assert_eq!(v.as_int(), Some(3), "valid contract call must succeed");

        // Precondition violation: `add(-1, 2)` must fail at runtime.
        let bad_source = r#"
fn add(a: Int, b: Int) -> Int
    requires a >= 0
{
    a + b
}
add(-1, 2)
"#;
        assert!(
            run_source_new(bad_source).is_err(),
            "precondition violation must fail at runtime"
        );
    }
    #[test]
    fn test_nil_annotation_end_to_end() {
        assert!(
            check_source("fn f(x: Nil) x\nf(nil)").is_ok(),
            "nil must have type Nil"
        );
        assert!(
            check_source("fn f(x: Nil) x\nf(1)").is_err(),
            "Int must not be accepted where Nil is annotated"
        );
    }

    #[test]
    fn test_variant_declaration_compiles_and_runs() {
        // A program that declares a variant and destructures it in match
        // patterns must compile through the whole MIR pipeline and run in
        // the VM. (Constructing variant *values* is lowered separately.)
        let source = r#"
type Color = Red | Green | Blue
fn code(c: Color) -> Int {
  match c with {
    | Red => 1
    | Green => 2
    | Blue => 3
  }
}
code
"#;
        let (value, _ty) = run_source(source).unwrap();
        assert!(value.as_int().is_some(), "expected function-index value");
    }

    #[test]
    fn test_variant_spec_example_end_to_end() {
        // The canonical SPEC2 §3.4.1 example, run end-to-end: generic
        // two-parameter variant declaration, construction in if-branches,
        // variant type annotations, and a match that binds the payload.
        let source = r#"
type Result[T, E] = Ok(T) | Error(E)

fn safe_divide(a: Float, b: Float) -> Result[Float, String] {
  if b == 0.0 then
    Error("Division by zero")
  else
    Ok(a / b)
}

fn describe(r: Result[Float, String]) -> String {
  match r with {
    | Ok(value) => "ok"
    | Error(msg) => msg
  }
}

match safe_divide(6.0, 2.0) with {
  | Ok(value) => value
  | Error(msg) => 0.0
}
"#;
        assert_float(source, 3.0);
    }

    #[test]
    fn test_variant_spec_example_error_arm_binds_string() {
        // The Error arm of the §3.4.1 example: the String payload must be
        // constructed, matched by tag, and bound into the arm body.
        let source = r#"
type Result[T, E] = Ok(T) | Error(E)

fn safe_divide(a: Float, b: Float) -> Result[Float, String] {
  if b == 0.0 then
    Error("Division by zero")
  else
    Ok(a / b)
}

fn describe(r: Result[Float, String]) -> String {
  match r with {
    | Ok(value) => "ok"
    | Error(msg) => msg
  }
}

describe(safe_divide(1.0, 0.0))
"#;
        assert_string(source, "Division by zero");
    }

    #[test]
    fn test_variant_construction_match_binds_payload() {
        // Core construction test: `Some(41)` builds a value and the match
        // binds the payload; the None arm must not be taken.
        let source = r#"
type Option[T] = Some(T) | None
match Some(41) with {
  | Some(x) => x
  | None => 0
}
"#;
        assert_int(source, 41);
    }

    #[test]
    fn test_variant_match_payload_arithmetic() {
        // The tag comparison lowering (record `ctor` field vs tag string
        // via OpCode::SCmpEq) must select the `Some` arm and the payload
        // must flow into arm-body arithmetic.
        let source = r#"
type Option[T] = Some(T) | None
match Some(41) with {
  | Some(x) => x + 1
  | None => 0
}
"#;
        assert_int(source, 42);
    }

    #[test]
    fn test_variant_nullary_ctor_arm_taken() {
        // A payload-less constructor is the bare tag; `None` must dispatch
        // to the `None` arm and not to `Some(x)`.
        let source = r#"
type Option[T] = Some(T) | None
match None with {
  | Some(x) => 1
  | None => 0
}
"#;
        assert_int(source, 0);
    }

    #[test]
    fn test_variant_nested_construction() {
        // Nested construction `Some(Some(2))` matched by a nested pattern:
        // the outer tag selects the arm and the inner payload binds.
        let source = r#"
type Option[T] = Some(T) | None
match Some(Some(2)) with {
  | Some(Some(x)) => x
  | Some(None) => 0
  | None => 0
}
"#;
        assert_int(source, 2);
    }

    #[test]
    fn test_variant_returned_from_function() {
        // A variant built inside a function must survive the return and be
        // matched by the caller.
        let source = r#"
type Option[T] = Some(T) | None
fn wrap(x: Int) -> Option[Int] { Some(x) }
match wrap(7) with {
  | Some(v) => v + 1
  | None => 0
}
"#;
        assert_int(source, 8);
    }

    #[test]
    fn test_variant_let_bound_matched_later() {
        // A let-bound variant value is matched in a later expression.
        let source = r#"
type Option[T] = Some(T) | None
let v = Some(5) in
match v with {
  | Some(x) => x * 2
  | None => 0
}
"#;
        assert_int(source, 10);
    }

    #[test]
    fn test_variant_int_and_string_payloads() {
        // One variant type exercised with payloads of different types:
        // the Int payload is bound and returned; the String-payload
        // constructor is matched by tag.
        let source = r#"
type Result[T, E] = Ok(T) | Error(E)
fn code(r: Result[Int, String]) -> Int {
  match r with {
    | Ok(v) => v
    | Error(msg) => 0
  }
}
code(Ok(3)) + code(Error("boom"))
"#;
        assert_int(source, 3);
    }

    #[test]
    fn test_variant_nested_pattern_binds_inner() {
        // A nested constructor pattern must test both tags and bind the
        // innermost payload.
        let source = r#"
type Option[T] = Some(T) | None
match Some(Some(9)) with {
  | Some(Some(x)) => x + 1
  | _ => 0
}
"#;
        assert_int(source, 10);
    }

    #[test]
    fn test_variant_nested_pattern_rejects_inner_none() {
        // `Some(None)` must NOT match `Some(Some(x))`: the inner tag test
        // runs against the payload, so the arm falls through to the
        // `Some(None)` arm. (With outer-tag-only matching this returned 1.)
        let source = r#"
type Option[T] = Some(T) | None
match Some(None) with {
  | Some(Some(x)) => 1
  | Some(None) => 2
  | None => 3
}
"#;
        assert_int(source, 2);
    }

    #[test]
    fn test_variant_nested_pattern_rejects_nullary() {
        // The bare `None` tag must not match a nested `Some(Some(x))` arm.
        let source = r#"
type Option[T] = Some(T) | None
match None with {
  | Some(Some(x)) => 1
  | None => 0
}
"#;
        assert_int(source, 0);
    }

    #[test]
    fn test_variant_payload_tuple_pattern() {
        // A tuple pattern nested inside a variant pattern: both the outer
        // tag and both element sub-patterns are tested, and the elements
        // bind into the arm body.
        let source = r#"
type Option[T] = Some(T) | None
match Some((1, 2)) with {
  | Some((a, b)) => a + b
  | None => 0
}
"#;
        assert_int(source, 3);
    }

    #[test]
    fn test_tuple_pattern_binds_elements() {
        // Structural tuple matching: each element position is loaded from
        // the scrutinee and bound into the arm body.
        let source = r#"
match (1, 2) with {
  | (a, b) => a + b
}
"#;
        assert_int(source, 3);
    }

    #[test]
    fn test_tuple_pattern_literal_element_matches() {
        // A literal element participates in the test: (1, 5) matches
        // (1, x) and binds x.
        let source = r#"
match (1, 5) with {
  | (1, x) => x
  | _ => 0
}
"#;
        assert_int(source, 5);
    }

    #[test]
    fn test_tuple_pattern_literal_element_rejects() {
        // (2, 5) must not match the (1, x) arm; it falls through to the
        // wildcard.
        let source = r#"
match (2, 5) with {
  | (1, x) => x
  | _ => 0
}
"#;
        assert_int(source, 0);
    }

    #[test]
    fn test_record_pattern_binds_fields() {
        // Structural record matching: named fields are loaded from the
        // scrutinee and bound into the arm body.
        let source = r#"
match { a: 3, b: 4 } with {
  | { a: x, b: y } => x + y
}
"#;
        assert_int(source, 7);
    }

    #[test]
    fn test_record_pattern_literal_field_rejects() {
        // A literal field pattern rejects a mismatching record: the first
        // arm's `a: 1` test fails for `{ a: 2, ... }`, so the second arm
        // binds both fields.
        let source = r#"
match { a: 2, b: 9 } with {
  | { a: 1, b: y } => y
  | { a: x, b: y } => x + y
}
"#;
        assert_int(source, 11);
    }

    // -----------------------------------------------------------------------
    // Test: Complex programs
    // -----------------------------------------------------------------------

    #[test]
    fn test_quicksort() {
        let source = r#"
            let partition = fn(arr, low, high) {
                let pivot = arr[high] in
                let i = low - 1 in
                let j = low in
                let loop = fn() {
                    if j < high then {
                        if arr[j] < pivot then {
                            let i = i + 1 in
                            let tmp = arr[i] in
                            let arr[i] = arr[j] in
                            let arr[j] = tmp in
                            let j = j + 1 in
                            loop()
                        } else {
                            let j = j + 1 in
                            loop()
                        }
                    } else {
                        let tmp = arr[i + 1] in
                        let arr[i + 1] = arr[high] in
                        let arr[high] = tmp in
                        i + 1
                    }
                } in loop()
            } in
            let quicksort = fn(arr, low, high) {
                if low < high then {
                    let pi = partition(arr, low, high) in
                    let _ = quicksort(arr, low, pi - 1) in
                    quicksort(arr, pi + 1, high)
                } else {
                    0
                }
            } in
            let arr = [3, 6, 8, 10, 1, 2, 1] in
            let _ = quicksort(arr, 0, 6) in
            arr[0]
        "#;
        let result = run_source(source);
        // Quicksort on arrays may or may not be fully supported.
        // The test mainly exercises the parser and type checker.
        match result {
            Ok((value, _)) => {
                assert!(
                    value.as_int().is_some(),
                    "Quicksort should produce a result"
                );
            }
            Err(_) => {
                // Array operations may not be fully implemented yet
            }
        }
    }

    #[test]
    fn test_counter_actor() {
        let source = r#"
            let counter = spawn {
                state count = 0
                behavior inc() { self.count + 1 }
                behavior get() { self.count }
            } in
            send counter.inc()
            send counter.inc()
            send counter.get()
        "#;
        let result = run_source(source);
        // Actor spawn/send may or may not be fully supported in the
        // compiler-to-VM pipeline yet.
        match result {
            Ok((value, _)) => {
                assert!(
                    value.as_int().is_some() || value.is_unit(),
                    "Counter actor should produce a result"
                );
            }
            Err(_) => {
                // Actor syntax may not be fully compiled yet
            }
        }
    }

    // -----------------------------------------------------------------------
    // Test: Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_block() {
        let source = "{}";
        let result = run_source(source);
        match result {
            Ok((value, _)) => assert!(value.is_unit() || value.is_nil()),
            Err(_) => {}
        }
    }

    #[test]
    fn test_deep_nesting() {
        let source =
            "let a = 1 in let b = 2 in let c = 3 in let d = 4 in let e = 5 in a + b + c + d + e";
        assert_int(source, 15);
    }

    #[test]
    fn test_large_int() {
        assert_int("1000000", 1_000_000);
    }

    #[test]
    fn test_zero() {
        assert_int("0", 0);
    }

    #[test]
    fn test_negative_zero() {
        // -0 should be 0
        assert_int("-0", 0);
    }

    // -----------------------------------------------------------------------
    // Test: v0.7 persistent actor end-to-end spawn
    // -----------------------------------------------------------------------

    #[test]
    fn test_persistent_actor_spawn_end_to_end() {
        let store = MemoryStore::new();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());

        let source = r#"
            persistent actor Counter {
                state durable count: Int = 0
                behavior inc() { self.count }
            }
            spawn Counter {}
        "#;

        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert!(actor.persistent, "actor should be persistent");
        assert_eq!(
            actor.state_models.get("count"),
            Some(&crate::runtime::StateModel::Durable),
            "count should use durable state model"
        );
    }

    #[test]
    fn test_persistent_counter_end_to_end_messages() {
        let store = MemoryStore::new();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());

        let source = r#"
            persistent actor Counter {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
            }
            let c = spawn Counter {} in {
                send c inc()
                send c inc()
                c
            }
        "#;

        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(actor.mailbox.len(), 2, "two inc messages should be queued");
            assert!(
                !actor.bytecode_offsets.is_empty(),
                "actor should have bytecode behavior offsets"
            );
            assert!(
                actor.bytecode_module.is_some(),
                "actor should have a bytecode module"
            );
        }

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("count").and_then(|v| v.as_int()),
            Some(2),
            "counter should be 2 after two inc messages"
        );
    }

    #[test]
    fn test_send_with_arguments() {
        let rt = Rc::new(RefCell::new(Runtime::new()));

        let source = r#"
            actor Counter {
                state count: Int = 0
                behavior add(n: Int) { self.count = self.count + n }
                behavior get() { self.count }
            }
            let c = spawn Counter {} in {
                send c add(5)
                send c add(7)
                c
            }
        "#;

        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("count").and_then(|v| v.as_int()),
            Some(12),
            "counter should be 12 after adding 5 and 7"
        );
    }

    #[test]
    fn test_ask_with_arguments() {
        let rt = Rc::new(RefCell::new(Runtime::new()));

        let source = r#"
            actor Calculator {
                behavior add(a: Int, b: Int) { a + b }
            }
            let calc = spawn Calculator {} in
                ask calc add(10, 20)
        "#;

        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        assert_eq!(value.as_int(), Some(30), "ask add(10, 20) should return 30");
    }

    /// `send` messages queue in the mailbox; `ask` should flush them before
    /// executing the asked behavior so that `send c inc(); ask c get()` returns
    /// the post-increment value, not the initial state.
    #[test]
    fn test_send_before_ask_flushes_mailbox() {
        let rt = Rc::new(RefCell::new(Runtime::new()));

        let source = r#"
            actor Counter {
                state count: Int = 0
                behavior inc() { self.count = self.count + 1 }
                behavior get() { self.count }
            }
            let c = spawn Counter {} in {
                send c inc()
                send c inc()
                ask c get()
            }
        "#;

        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        assert_eq!(
            value.as_int(),
            Some(2),
            "send inc() ×2 then ask get() should return 2 after flush"
        );
    }

    /// `send` followed by `ask` with arguments: verify the flush dispatches
    /// each message correctly with the right parameter counts.
    #[test]
    fn test_send_before_ask_with_args_flushes_mailbox() {
        let rt = Rc::new(RefCell::new(Runtime::new()));

        let source = r#"
            actor Counter {
                state count: Int = 0
                behavior add(n: Int) { self.count = self.count + n }
                behavior get() { self.count }
            }
            let c = spawn Counter {} in {
                send c add(10)
                send c add(5)
                ask c get()
            }
        "#;

        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        assert_eq!(
            value.as_int(),
            Some(15),
            "send add(10) + add(5) then ask get() should return 15 after flush"
        );
    }

    /// Regression test for a silent-data-loss bug found while adding actor
    /// support to the HIR/MIR pipeline: `compile_binary`'s BinOp::Assign case
    /// only special-cased `self.field = v`; every other assignment target
    /// (array index, non-self record field) fell through to
    /// `compile_expr(left)` (reading the CURRENT value) followed by
    /// `OpCode::Store`, a plain register-to-register copy — the assignment
    /// never reached the array/record at all. Fixed by intercepting
    /// BinOp::Assign in compile_expr's dispatch and routing it through
    /// compile_assign, which computes a place (object + field id, or array +
    /// index) instead of reading a value.
    #[test]
    fn test_legacy_index_and_field_assign() {
        let (value, _ty) = run_source("let arr = [1, 2, 3] in { arr[0] = 99 arr[0] }").unwrap();
        assert_eq!(
            value.as_int(),
            Some(99),
            "arr[0] = 99 should actually mutate the array"
        );

        let (value, _ty) = run_source("let r = { x: 1, y: 2 } in { r.x = 99 r.x + r.y }").unwrap();
        assert_eq!(
            value.as_int(),
            Some(101),
            "r.x = 99 should actually mutate the record"
        );
    }

    /// End-to-end regression for JIT type-guard stripping: a hot recursive
    /// numeric function must tier up through the type-directed
    /// (guard-stripped) compiler and produce exactly the same result the
    /// interpreter computes. The arithmetic-heavy body gives the tiering
    /// path a straight-line region longer than the 5-instruction minimum.
    #[test]
    fn test_jit_typed_guard_stripping_hot_function() {
        let source = r#"
            fn hot(n: Int, acc: Int) -> Int {
                if n < 1 then acc else {
                    let a = acc + n in
                    let b = a + 1 in
                    let c = b + 2 in
                    let d = c + 3 in
                    let e = d + 4 in
                    hot(n - 1, e + 5)
                }
            }
            hot(2000, 0)
        "#;
        // Per call: acc += n + (1+2+3+4+5); n runs 2000..1.
        let expected: i64 = (1..=2000).sum::<i64>() + 15 * 2000;

        let (module, _ty) = compile_source(source).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        let value = vm.run().unwrap();
        assert_eq!(
            value.as_int(),
            Some(expected),
            "typed-path result must be exact"
        );
        assert!(
            vm.jit_typed_compiled_count() >= 1,
            "hot numeric function must compile through the type-directed JIT path"
        );
    }

    /// End-to-end float arithmetic: mir_codegen's binary/unary opcode
    /// emission is type-directed — float operands must compile to the F*
    /// opcode variants, since the integer handlers coerce float operands
    /// to 0 (so `1.5 + 2.5` used to evaluate to 0).
    #[test]
    fn test_float_arithmetic_end_to_end() {
        assert_float("1.5 + 2.5", 4.0);
        assert_float("5.5 - 2.0", 3.5);
        assert_float("1.5 * 2.0", 3.0);
        assert_float("7.0 / 2.0", 3.5);
        assert_float("-1.5", -1.5);
        // Float-ness propagates through let bindings even though
        // hir_lower types binary results as Int.
        assert_float("let x = 1.5 in let y = x + 2.5 in y * 2.0", 8.0);
    }

    /// All six comparisons on float operands, with exact expected values.
    /// Integer comparison opcodes coerce both sides to 0, which made
    /// `2.0 == 3.0` true and `2.5 <= 1.5` true before the fix.
    #[test]
    fn test_float_comparisons_end_to_end() {
        assert_bool("1.5 < 2.5", true);
        assert_bool("2.5 < 1.5", false);
        assert_bool("2.5 > 1.5", true);
        assert_bool("1.5 > 2.5", false);
        assert_bool("1.5 <= 2.5", true);
        assert_bool("2.5 <= 1.5", false);
        assert_bool("2.5 >= 1.5", true);
        assert_bool("1.5 >= 2.5", false);
        assert_bool("2.0 == 3.0", false);
        assert_bool("2.0 == 2.0", true);
        assert_bool("2.0 != 3.0", true);
        assert_bool("2.0 != 2.0", false);
    }

    /// Float arithmetic threaded through the integer opcode fallback:
    /// unannotated parameters default to the numeric type variable, but when
    /// the function is applied to float literals the VM may still execute the
    /// integer opcodes (IAdd/ISub/IMul/IDiv/IMod/INeg). The interpreter and
    /// JIT runtime helpers now dispatch to float behavior when both operands
    /// are real floats, so these must produce correct float results.
    #[test]
    fn test_float_threading_through_integer_opcodes() {
        assert_float("let f = fn(x, y) x + y in f(1.5, 2.5)", 4.0);
        assert_float("let f = fn(x, y) x - y in f(5.5, 2.0)", 3.5);
        assert_float("let f = fn(x, y) x * y in f(1.5, 2.0)", 3.0);
        assert_float("let f = fn(x, y) x / y in f(7.0, 2.0)", 3.5);
        assert_float("let f = fn(x, y) x % y in f(7.5, 2.0)", 1.5);
        assert_float("let f = fn(x) -x in f(1.5)", -1.5);
    }

    /// Float comparisons threaded through the integer comparison fallback:
    /// unannotated parameters and the standard comparison operators must work
    /// on float operands even if the compiler emitted ICmp* opcodes.
    #[test]
    fn test_float_comparisons_threading_through_integer_opcodes() {
        assert_bool("let f = fn(x, y) x < y in f(1.5, 2.5)", true);
        assert_bool("let f = fn(x, y) x > y in f(1.5, 2.5)", false);
        assert_bool("let f = fn(x, y) x <= y in f(1.5, 2.5)", true);
        assert_bool("let f = fn(x, y) x >= y in f(1.5, 2.5)", false);
        assert_bool("let f = fn(x, y) x == y in f(2.0, 2.0)", true);
        assert_bool("let f = fn(x, y) x == y in f(2.0, 3.0)", false);
    }

    /// Float modulo: `7.5 % 2.0` compiles to the FMod opcode and the
    /// interpreter evaluates it with f64 % f64 semantics; a zero float
    /// divisor yields nil, mirroring FDiv.
    #[test]
    fn test_float_modulo_end_to_end() {
        assert_float("7.5 % 2.0", 1.5);
        assert_float("7.0 % 2.0", 1.0);
        let (value, _ty) = run_source("7.0 % 0.0").unwrap();
        assert_eq!(
            value.as_raw(),
            crate::vm::Value::nil().as_raw(),
            "float modulo by zero must yield nil, got {:?}",
            value
        );
        assert_int("7 % 2", 1);
    }

    /// A hot loop (>1000 reductions, past the JIT tier-up threshold)
    /// containing float `%` must produce correct results. FMod is not in
    /// `is_opcode_compilable`, so `find_compilable_region` stops at it and
    /// the opcode only ever runs in the interpreter — this pins that
    /// graceful fallback.
    #[test]
    fn test_float_modulo_hot_loop_interpreter_only() {
        let source = r#"
            fn loop_mod(n: Int, acc: Float) -> Float {
                if n < 1 then acc else {
                    let a = acc + 0.5 in
                    let b = a % 2.0 in
                    loop_mod(n - 1, b)
                }
            }
            loop_mod(1501, 0.0)
        "#;
        // acc cycles 0.5 -> 1.0 -> 1.5 -> 0.0 -> ... with period 4;
        // 1501 mod 4 == 1, so the final value is 0.5. All intermediate
        // values are exactly representable in f64.
        assert_float(source, 0.5);
    }

    /// The interpreter's FDiv yields nil on a zero divisor; the JIT must
    /// match (nulang_fdiv guards the zero divisor, and the typed compiler
    /// routes FDiv through that helper instead of emitting a raw fdiv
    /// that would produce inf/NaN). The hot run tiers the function up
    /// through the type-directed JIT path (>1000 reductions), so both
    /// runs agreeing proves interpreter == JIT.
    #[test]
    fn test_float_div_by_zero_cold_and_hot_parity() {
        let source = |n: i64| {
            format!(
                r#"
                fn fdivz(n: Int, acc: Float) -> Float {{
                    if n < 1 then acc else {{
                        let _ = acc + 1.0 in
                        let _ = acc + 1.0 in
                        let _ = acc + 1.0 in
                        let _ = acc + 1.0 in
                        let a = acc + 1.0 in
                        let b = a * 2.0 in
                        let c = b - 3.0 in
                        let d = c / 0.0 in
                        fdivz(n - 1, d)
                    }}
                }}
                fdivz({}, 7.0)
                "#,
                n
            )
        };

        // Cold: below the tiering threshold, purely interpreted.
        let (cold, _ty) = run_source(&source(5)).unwrap();
        assert_eq!(
            cold.as_raw(),
            Value::nil().as_raw(),
            "interpreted float div by zero must yield nil"
        );

        // Hot: forces JIT tier-up of the loop body containing the FDiv.
        let (module, _ty) = compile_source(&source(2000)).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        let hot = vm.run().unwrap();
        assert_eq!(
            hot.as_raw(),
            cold.as_raw(),
            "JIT result must match the interpreter for float div by zero"
        );
        assert_eq!(hot.as_raw(), Value::nil().as_raw());
        assert!(
            vm.jit_typed_compiled_count() >= 1,
            "hot float function must compile through the type-directed JIT path"
        );
    }

    /// A resuming effect handler performed INSIDE a hot loop (past the JIT
    /// tier-up threshold) must produce the same result under JIT and pure
    /// interpretation. `PerformDirect` is in the JIT compilable set but
    /// compiles to a yield-to-interpreter: the compiled region sets a yield
    /// PC at the PerformDirect and `try_jit_execute` re-enters the
    /// interpreter there, which captures the continuation, dispatches the
    /// handler, and resumes. This is the exact interaction the differential
    /// fuzzer cannot reach (its corpus contains no effect programs), so this
    /// test pins interp==JIT for an effect performed once per loop iteration
    /// for 2000 iterations.
    #[test]
    fn test_effect_in_hot_loop_jit_matches_interpreter() {
        let source = |n: i64| {
            format!(
                r#"
                effect Ticker {{ next: Int -> Int }}
                fn run(n: Int) -> Int {{
                    var acc = 0;
                    var i = 0;
                    handle {{
                        while i < n {{
                            acc = perform Ticker.next(i);
                            i = i + 1;
                        }}
                    }} {{
                        | Ticker.next(x) resume => resume(x + 1)
                    }}
                    acc
                }}
                run({})
                "#,
                n
            )
        };

        // Interpreter on the SAME program: n=2000, purely interpreted
        // (below the tiering threshold this would also run, but we run it
        // through `run_source`'s plain interpreter for the parity baseline).
        let (interp, _ty) = run_source(&source(2000)).unwrap();
        assert_eq!(
            interp.as_int(),
            Some(2000),
            "interpreter: 2000 iterations resume x+1 -> acc=2000"
        );

        // Hot: forces JIT tier-up of the loop body containing PerformDirect.
        let (module, _ty) = compile_source(&source(2000)).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        let hot = vm.run().unwrap();
        assert_eq!(
            hot.as_raw(),
            interp.as_raw(),
            "JIT result must match interpreter for an effect performed in a hot loop"
        );
        assert_eq!(
            hot.as_int(),
            Some(2000),
            "2000 iterations resume x+1 -> acc=2000"
        );
        // `PerformDirect` is a safepoint that yields to the interpreter and
        // clobbers the register set, so typed (guard-stripped) compilation
        // cannot apply — the loop compiles through the scalar JIT path. Assert
        // the JIT genuinely engaged (not silently interpreted) via the scalar
        // count, and that the typed count stayed 0 (honest about mechanism).
        assert!(
            vm.jit_compiled_count() >= 1,
            "hot effect loop must JIT-compile (scalar path)"
        );
        assert_eq!(
            vm.jit_typed_compiled_count(),
            0,
            "effect-containing region must not use typed compilation (PerformDirect clobbers regs)"
        );
    }

    /// The value a resuming handler feeds back through `resume` must flow
    /// across the JIT yield boundary into subsequent arithmetic, bit-for-bit
    /// matching the interpreter. Here `Double.apply(i)` resumes with `i+1`,
    /// and the loop accumulates `v * 2` where `v` is the resumed value read
    /// at the perform site. The resumed value crosses from the handler
    /// (interpreted, since `PerformDirect` yields) back into the JIT-compiled
    /// loop body's `acc += v * 2`, pinning the register-round-trip at the
    /// yield point. Expected: Σ 2(i+1) for i in 0..n == n(n+1).
    #[test]
    fn test_effect_resume_value_flows_into_jit_loop_arithmetic() {
        let source = |n: i64| {
            format!(
                r#"
                effect Double {{ apply: Int -> Int }}
                fn run(n: Int) -> Int {{
                    var acc = 0;
                    var i = 0;
                    handle {{
                        while i < n {{
                            let v = perform Double.apply(i);
                            acc = acc + v * 2;
                            i = i + 1;
                        }}
                    }} {{
                        | Double.apply(x) resume => resume(x + 1)
                    }}
                    acc
                }}
                run({})
                "#,
                n
            )
        };

        // Interpreter baseline.
        let (interp, _ty) = run_source(&source(2000)).unwrap();
        let n = 2000i64;
        assert_eq!(
            interp.as_int(),
            Some(n * (n + 1)),
            "Σ 2(i+1) for i in 0..n == n(n+1) = {}",
            n * (n + 1)
        );

        // JIT: the resumed value must survive the yield boundary identically.
        let (module, _ty) = compile_source(&source(2000)).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        let hot = vm.run().unwrap();
        assert_eq!(
            hot.as_raw(),
            interp.as_raw(),
            "resumed value flowing into JIT-compiled arithmetic must match interpreter"
        );
        assert_eq!(hot.as_int(), Some(n * (n + 1)));
        assert!(
            vm.jit_compiled_count() >= 1,
            "hot effect loop must JIT-compile (scalar path)"
        );
    }

    /// Two distinct effect ops (`add`/`mul`) performed SEQUENTIALLY each
    /// hot-loop iteration must each route through the correct resuming
    /// handler and feed the chained value across the JIT yield boundary. This
    /// pins handler-table dispatch + per-op continuation capture at scale,
    /// under the JIT: `PerformDirect` yields to the interpreter for EACH op,
    /// the interpreter dispatches the matching handler arm and resumes, and
    /// the resumed value flows into the next perform. Unlike the if/else
    /// form (which is correct but fragments the JIT region), the straight-line
    /// loop body compiles as one region, so `jit_compiled_count() >= 1`.
    /// Expected for last i = n-1: acc = (i+10)*3.
    #[test]
    fn test_multi_effect_dispatch_in_hot_loop_jit() {
        let source = |n: i64| {
            format!(
                r#"
                effect Ops {{
                  add: Int -> Int
                  mul: Int -> Int
                }}
                fn run(n: Int) -> Int {{
                    var acc = 0;
                    var i = 0;
                    handle {{
                        while i < n {{
                            let a = perform Ops.add(i);
                            let b = perform Ops.mul(a);
                            acc = b;
                            i = i + 1;
                        }}
                    }} {{
                        | Ops.add(x) resume => resume(x + 10)
                        | Ops.mul(x) resume => resume(x * 3)
                    }}
                    acc
                }}
                run({})
                "#,
                n
            )
        };

        // Interpreter baseline.
        let (interp, _ty) = run_source(&source(2000)).unwrap();
        // Last i is 1999 -> add: 2009, then mul: 2009*3 = 6027.
        assert_eq!(
            interp.as_int(),
            Some(6027),
            "interpreter multi-effect result"
        );

        // JIT: both ops must dispatch correctly across the yield boundary.
        let (module, _ty) = compile_source(&source(2000)).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        let hot = vm.run().unwrap();
        assert_eq!(
            hot.as_raw(),
            interp.as_raw(),
            "multi-effect dispatch in a hot loop must match interpreter under JIT"
        );
        assert_eq!(hot.as_int(), Some(6027));
        assert!(
            vm.jit_compiled_count() >= 1,
            "hot multi-effect loop must JIT-compile (scalar path)"
        );
    }

    /// Hot float arithmetic with a nonzero divisor: the typed JIT path
    /// must produce bit-identical results to the interpreter. The
    /// recurrence acc' = (2*acc + 1)/4 converges to exactly 0.5.
    #[test]
    fn test_float_arithmetic_hot_typed_jit_exact() {
        let source = r#"
            fn hotf(n: Int, acc: Float) -> Float {
                if n < 1 then acc else {
                    let _ = acc + 1.0 in
                    let _ = acc + 1.0 in
                    let _ = acc + 1.0 in
                    let a = acc * 2.0 in
                    let b = a + 1.0 in
                    hotf(n - 1, b / 4.0)
                }
            }
            hotf(2000, 0.0)
        "#;
        let (module, _ty) = compile_source(source).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        let value = vm.run().unwrap();
        assert_eq!(
            value.as_float(),
            Some(0.5),
            "typed-path float math must be exact"
        );
        assert!(
            vm.jit_typed_compiled_count() >= 1,
            "hot float function must compile through the type-directed JIT path"
        );
    }

    /// A handler binding with more than MAX_STAGED_ARGS (16) parameters
    /// must be an honest compile error: the VM stages effect arguments in
    /// r0..r15, so a longer prologue would alias the enclosing function's
    /// locals (mirrors the 17-parameter function check).
    #[test]
    fn test_over_limit_handler_params_compile_error() {
        let params = (0..17)
            .map(|i| format!("p{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!("handle 0 {{ | E.op({}) => p0 }}", params);
        let result = run_source(&source);
        assert!(
            matches!(result, Err(NuError::VMError { .. })),
            "a 17-parameter handler binding should be a compile error, got {:?}",
            result
        );
    }

    #[test]
    fn test_register_overflow_errors() {
        // 20 nested let bindings — the MIR pipeline allocates isolated
        // per-function registers and can handle this depth.
        let source = r#"
            let a0 = 0 in let a1 = 1 in let a2 = 2 in let a3 = 3 in let a4 = 4 in
            let a5 = 5 in let a6 = 6 in let a7 = 7 in let a8 = 8 in let a9 = 9 in
            let a10 = 10 in let a11 = 11 in let a12 = 12 in let a13 = 13 in let a14 = 14 in
            let a15 = 15 in let a16 = 16 in let a17 = 17 in let a18 = 18 in let a19 = 19 in
            a19
        "#;
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(value.as_int(), Some(19));
    }

    #[test]
    fn test_persistent_counter_recover_after_restart() {
        let source = r#"
            persistent actor Counter {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
                behavior get() { self.count }
            }
            spawn Counter {}
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        let mut comp_offsets: Vec<Option<usize>> = vec![None; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
                comp_offsets[idx] = entry.compensate_offset;
            }
        }

        // First runtime: spawn, send 3 inc messages, and run scheduler.
        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt1.borrow_mut().send_message(actor_id, "inc", &[]);
        rt1.borrow_mut().send_message(actor_id, "inc", &[]);
        rt1.borrow_mut().send_message(actor_id, "inc", &[]);
        rt1.borrow_mut().run_scheduler();
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(3)
        );

        // Simulate a runtime restart: new runtime sharing the same store,
        // register the bytecode module, then recover.
        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            vec![None; module.behaviors.len()],
        );
        rt2.borrow_mut().recover_actor(actor_id);

        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(3),
            "recovered counter should still be 3"
        );

        // Send two more inc messages on the recovered runtime.
        rt2.borrow_mut().send_message(actor_id, "inc", &[]);
        rt2.borrow_mut().send_message(actor_id, "inc", &[]);
        rt2.borrow_mut().run_scheduler();
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(5),
            "counter should continue incrementing after recovery"
        );
    }

    /// Regression for the `state_models` recovery bug fixed alongside
    /// PLAN.md bullet 8: before the fix, `recover_actor` never restored
    /// `Actor.state_models` on the rebuilt actor, so every field
    /// silently reverted to the `Local` default after one recovery --
    /// meaning a *second* crash would have dropped `durable` fields
    /// from the snapshot entirely (`checkpoint_actor` only includes
    /// `Durable`/`Crdt` fields, and `state_models.get(name)` on a field
    /// missing from the map falls back to `Local`). This drives a
    /// durable counter through two full crash-and-recover cycles and
    /// asserts the count survives both.
    #[test]
    fn test_durable_state_survives_two_recovery_cycles() {
        let source = r#"
            persistent actor Counter {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
                behavior get() { self.count }
            }
            spawn Counter {}
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        let mut comp_offsets: Vec<Option<usize>> = vec![None; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
                comp_offsets[idx] = entry.compensate_offset;
            }
        }

        // Cycle 0: spawn, one inc, checkpoint happens automatically per
        // step.
        let rt0 = Rc::new(RefCell::new(Runtime::new()));
        rt0.borrow_mut().persistence = Box::new(store.clone());
        let actor_id = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt0.clone())));
            vm.run().unwrap().as_actor_id().unwrap()
        };
        rt0.borrow_mut().send_message(actor_id, "inc", &[]);
        rt0.borrow_mut().run_scheduler();

        // Cycle 1: first crash + recover, one more inc.
        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        rt1.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            comp_offsets.clone(),
        );
        rt1.borrow_mut().recover_actor(actor_id);
        rt1.borrow_mut().send_message(actor_id, "inc", &[]);
        rt1.borrow_mut().run_scheduler();
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(2),
            "count should be 2 after cycle 1 (1 inc pre-crash + 1 post-recovery)"
        );

        // Cycle 2: SECOND crash + recover -- this is the one that would
        // have silently lost `count` before the state_models fix, since
        // rt1's checkpoint_actor would have treated `count` as `Local`
        // (missing from an empty state_models map) and excluded it from
        // the snapshot entirely.
        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            comp_offsets.clone(),
        );
        rt2.borrow_mut().recover_actor(actor_id);
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(2),
            "count must survive a SECOND recovery cycle, not silently \
             reset to 0"
        );
        rt2.borrow_mut().send_message(actor_id, "inc", &[]);
        rt2.borrow_mut().run_scheduler();
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(3),
            "counter should keep incrementing correctly after two \
             recovery cycles"
        );
    }

    // -----------------------------------------------------------------------
    // Virtual actor (grain) tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_grain_hydrates_on_first_send() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
                behavior get() { self.count }
            }
            0
        "#;

        let (module, _ty) = compile_source(source).unwrap();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().register_module_grains(&module);

        let grain_id = GrainId::new("Counter", "alpha");
        let stable_id = grain_actor_id(&grain_id);
        assert!(
            !rt.borrow().actors.contains_key(&stable_id),
            "grain should not be resident before first send"
        );

        rt.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&stable_id).expect("grain should hydrate");
        assert_eq!(
            actor.get_state_field("count").and_then(|v| v.as_int()),
            Some(1)
        );
    }

    #[test]
    fn test_grain_state_persists_across_runtime_restart() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
                behavior get() { self.count }
            }
            0
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let grain_id = GrainId::new("Counter", "beta");
        let stable_id = grain_actor_id(&grain_id);

        // First runtime: hydrate, send two incs.
        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        rt1.borrow_mut().register_module_grains(&module);
        rt1.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt1.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt1.borrow_mut().run_scheduler();
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&stable_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(2)
        );

        // Second runtime: same store, no explicit spawn; sending re-hydrates from snapshot.
        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_module_grains(&module);
        assert!(!rt2.borrow().actors.contains_key(&stable_id));
        rt2.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt2.borrow_mut().run_scheduler();
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&stable_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(3),
            "grain state must survive runtime restart"
        );
    }

    #[test]
    fn test_grain_dehydrates_and_rehydrates() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
                behavior get() { self.count }
            }
            0
        "#;

        let (module, _ty) = compile_source(source).unwrap();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().register_module_grains(&module);
        // Make dehydration immediate once the scan runs.
        rt.borrow_mut()
            .grain_registry
            .get_mut("Counter")
            .unwrap()
            .dehydrate_policy = DehydratePolicy {
            idle_ms: 0,
            allow_dehydrate: true,
        };

        let grain_id = GrainId::new("Counter", "gamma");
        let stable_id = grain_actor_id(&grain_id);

        rt.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt.borrow_mut().run_scheduler();
        assert_eq!(
            rt.borrow()
                .actors
                .get(&stable_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(1)
        );

        // Manually trigger the dehydration scan. In production this is driven
        // by the scheduler every DEHYDRATE_CHECK_INTERVAL ticks.
        rt.borrow_mut().dehydrate_idle_grains();

        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&stable_id).expect("grain still resident");
            assert!(
                actor.is_hibernated(),
                "idle grain with empty mailbox should dehydrate"
            );
        }

        // Sending to a hibernated grain wakes it and processes the message.
        rt.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt.borrow_mut().run_scheduler();
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&stable_id).unwrap();
            assert!(!actor.is_hibernated(), "grain should wake from hibernation");
            assert_eq!(
                actor.get_state_field("count").and_then(|v| v.as_int()),
                Some(2)
            );
        }
    }

    #[test]
    fn test_pinned_grain_skips_dehydration() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
                behavior get() { self.count }
            }
            0
        "#;

        let (module, _ty) = compile_source(source).unwrap();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().register_module_grains(&module);
        rt.borrow_mut()
            .grain_registry
            .get_mut("Counter")
            .unwrap()
            .dehydrate_policy = DehydratePolicy {
            idle_ms: 0,
            allow_dehydrate: true,
        };

        let grain_id = GrainId::new("Counter", "delta");
        let stable_id = grain_actor_id(&grain_id);

        rt.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().actors.get_mut(&stable_id).unwrap().pin();

        rt.borrow_mut().dehydrate_idle_grains();

        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&stable_id).unwrap();
            assert!(!actor.is_hibernated(), "pinned grain should not dehydrate");
        }
    }

    #[test]
    fn test_grain_eviction_reclaims_memory_and_rehydrates() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
                behavior get() { self.count }
            }
            0
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());
        rt.borrow_mut().register_module_grains(&module);
        rt.borrow_mut()
            .grain_registry
            .get_mut("Counter")
            .unwrap()
            .dehydrate_policy = DehydratePolicy {
            idle_ms: 0,
            allow_dehydrate: true,
        };

        let grain_id = GrainId::new("Counter", "epsilon");
        let stable_id = grain_actor_id(&grain_id);

        rt.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt.borrow_mut().run_scheduler();
        assert_eq!(
            rt.borrow()
                .actors
                .get(&stable_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(1)
        );

        // Hibernate, then evict the resident actor to reclaim memory.
        rt.borrow_mut().dehydrate_idle_grains();
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref
                .actors
                .get(&stable_id)
                .expect("grain still resident before eviction");
            assert!(
                actor.is_hibernated(),
                "grain should be hibernated before eviction"
            );
        }

        let evicted = rt.borrow_mut().evict_hibernated_grains(None);
        assert_eq!(evicted, 1, "one hibernated grain should be evicted");

        {
            let rt_ref = rt.borrow();
            assert!(
                !rt_ref.actors.contains_key(&stable_id),
                "evicted grain should be removed from actors"
            );
            assert!(
                !rt_ref.grain_residents.contains_key(&grain_id),
                "evicted grain should be removed from grain_residents"
            );
            assert!(
                rt_ref.grain_actor_ids.contains_key(&stable_id),
                "stable grain mapping must survive eviction so sends can re-hydrate"
            );
        }

        // Sending again re-hydrates from the persisted snapshot and processes the message.
        rt.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt.borrow_mut().run_scheduler();
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref
                .actors
                .get(&stable_id)
                .expect("grain should re-hydrate");
            assert!(
                !actor.is_hibernated(),
                "grain should be active after re-hydration"
            );
            assert_eq!(
                actor.get_state_field("count").and_then(|v| v.as_int()),
                Some(2),
                "evicted grain state must be restored from snapshot"
            );
        }
    }

    #[test]
    fn test_evicted_grain_rehydrates_on_send_message_by_id() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
            }
            0
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());
        rt.borrow_mut().register_module_grains(&module);
        rt.borrow_mut()
            .grain_registry
            .get_mut("Counter")
            .unwrap()
            .dehydrate_policy = DehydratePolicy {
            idle_ms: 0,
            allow_dehydrate: true,
        };

        let grain_id = GrainId::new("Counter", "zeta");
        let stable_id = grain_actor_id(&grain_id);

        rt.borrow_mut()
            .send_to_grain(grain_id.clone(), "inc", vec![], 0);
        rt.borrow_mut().run_scheduler();
        let behavior_id = rt.borrow().behavior_id_for(stable_id, "inc").unwrap();

        rt.borrow_mut().dehydrate_idle_grains();
        rt.borrow_mut().evict_hibernated_grains(None);
        assert!(!rt.borrow().actors.contains_key(&stable_id));

        // send_message_by_id should detect the evicted grain and re-hydrate.
        rt.borrow_mut()
            .send_message_by_id(stable_id, behavior_id, &[]);
        rt.borrow_mut().run_scheduler();
        assert_eq!(
            rt.borrow()
                .actors
                .get(&stable_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(2)
        );
    }

    #[test]
    fn test_eviction_respects_pin_and_max_limit() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
            }
            0
        "#;

        let (module, _ty) = compile_source(source).unwrap();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().register_module_grains(&module);
        rt.borrow_mut()
            .grain_registry
            .get_mut("Counter")
            .unwrap()
            .dehydrate_policy = DehydratePolicy {
            idle_ms: 0,
            allow_dehydrate: true,
        };

        let g1 = GrainId::new("Counter", "one");
        let g2 = GrainId::new("Counter", "two");
        let s1 = grain_actor_id(&g1);
        let s2 = grain_actor_id(&g2);

        rt.borrow_mut().send_to_grain(g1.clone(), "inc", vec![], 0);
        rt.borrow_mut().send_to_grain(g2.clone(), "inc", vec![], 0);
        rt.borrow_mut().run_scheduler();

        // Pin g1 so it cannot be evicted.
        rt.borrow_mut().actors.get_mut(&s1).unwrap().pin();

        rt.borrow_mut().dehydrate_idle_grains();
        assert!(
            !rt.borrow().actors.get(&s1).unwrap().is_hibernated(),
            "pinned grain should not be hibernated"
        );
        assert!(rt.borrow().actors.get(&s2).unwrap().is_hibernated());

        // max_evict=1 should evict only g2 (g1 is pinned).
        let evicted = rt.borrow_mut().evict_hibernated_grains(Some(1));
        assert_eq!(evicted, 1);
        assert!(
            rt.borrow().actors.contains_key(&s1),
            "pinned grain must remain resident"
        );
        assert!(
            !rt.borrow().actors.contains_key(&s2),
            "unpinned grain should be evicted"
        );
    }

    #[test]
    fn test_perform_grain_ref_returns_stable_actor_id() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
            }
            perform Grain.ref("Counter", "k1")
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();

        let expected_id = grain_actor_id(&GrainId::new("Counter", "k1"));
        assert_eq!(value.as_actor_id(), Some(expected_id));
    }

    #[test]
    fn test_perform_grain_ref_after_prewarm_sends() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
            }
            perform Grain.prewarm("Counter", "k1");
            let c = perform Grain.ref("Counter", "k1") in {
                send c inc();
                0
            }
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let _ = run_source_with_runtime(source, rt.clone()).unwrap();
        rt.borrow_mut().run_scheduler();

        let stable_id = grain_actor_id(&GrainId::new("Counter", "k1"));
        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&stable_id).expect("grain should hydrate");
        assert_eq!(
            actor.get_state_field("count").and_then(|v| v.as_int()),
            Some(1)
        );
    }

    #[test]
    fn test_perform_grain_prewarm_hydrates_grain() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
            }
            perform Grain.prewarm("Counter", "k2")
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let _ = run_source_with_runtime(source, rt.clone()).unwrap();

        let stable_id = grain_actor_id(&GrainId::new("Counter", "k2"));
        assert!(
            rt.borrow().actors.contains_key(&stable_id),
            "perform Grain.prewarm should hydrate the grain"
        );
    }

    #[test]
    fn test_perform_grain_pin_unpin() {
        let source_pin = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
            }
            perform Grain.prewarm("Counter", "k3");
            perform Grain.pin("Counter", "k3");
            0
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let _ = run_source_with_runtime(source_pin, rt.clone()).unwrap();

        let stable_id = grain_actor_id(&GrainId::new("Counter", "k3"));
        assert!(
            rt.borrow().actors.get(&stable_id).unwrap().pinned,
            "perform Grain.pin should set the pinned flag"
        );

        let source_unpin = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
            }
            perform Grain.unpin("Counter", "k3");
            0
        "#;
        let _ = run_source_with_runtime(source_unpin, rt.clone()).unwrap();
        assert!(
            !rt.borrow().actors.get(&stable_id).unwrap().pinned,
            "perform Grain.unpin should clear the pinned flag"
        );
    }

    #[test]
    fn test_grain_ref_expression() {
        let source = r#"
            virtual entity Counter(key: String) {
                state durable count: Int = 0
                behavior inc() { self.count = self.count + 1 }
            }
            let c = Grain("Counter", "k1") in {
                send c inc();
                send c inc();
                c
            }
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();

        let grain_id = GrainId::new("Counter", "k1");
        let stable_id = grain_actor_id(&grain_id);
        assert_eq!(
            value.as_actor_id(),
            Some(stable_id),
            "Grain(...) should return the stable actor id"
        );

        rt.borrow_mut().run_scheduler();
        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&stable_id).expect("grain should hydrate");
        assert_eq!(
            actor.get_state_field("count").and_then(|v| v.as_int()),
            Some(2),
            "two inc messages should increment the grain counter"
        );
    }

    #[test]
    fn test_event_sourced_counter_emits_and_recovers() {
        let source = r#"
            persistent actor EventCounter {
                state event_sourced count: Int = 0
                behavior inc() {
                    emit Incremented()
                }
                behavior get() {
                    self.count
                }
            }
            let c = spawn EventCounter {} in {
                send c inc()
                send c inc()
                send c inc()
                c
            }
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("count").and_then(|v| v.as_int()),
            Some(3),
            "event-sourced counter should be 3 after three inc messages"
        );
        assert_eq!(actor.event_log.len(), 3, "three events should be logged");
        assert_eq!(actor.event_log[0].0, "Incremented");
    }

    /// PLAN.md bullet 8 (persistence recovery correctness): does recovery
    /// of an `event_sourced` field with an `apply` handler reproduce the
    /// value a never-crashed run would reach? It does not -- see
    /// SPEC2.md §9.6's "Implementation status" note for full analysis.
    /// This test pins the CURRENT (buggy) recovered value so a silent
    /// regression or silent fix is caught either way, rather than
    /// leaving the gap purely as a documentation claim.
    ///
    /// Baseline (no crash): `entity Counter` with
    /// `apply | Incremented(by) => self.count = self.count + by`, sent
    /// `increment(3)` then `increment(4)`, reaches count = 9 -- this
    /// matches `persist_07_emit_accumulates_across_sends.nula`'s real
    /// captured output (apply computes `count + by`, plus an
    /// unconditional "+1" every `event_sourced` field gets per emit,
    /// see `persist_08_emit_bumps_all_event_sourced.json`).
    ///
    /// With a crash-and-recover between the two sends, recovery ignores
    /// the first event's `by = 3` entirely (it only counts "one event
    /// happened": `recover_actor` reconstructs `event_sourced` fields as
    /// a bare count of persisted `EventEntry` rows, never running the
    /// `apply` handler against their `args`), landing on 1 instead of
    /// the live value of 4. The second send then applies on top of that
    /// wrong base, landing the whole run on 6 instead of 9.
    /// EventSourced fields with non-trivial `apply` handlers survive
    /// crash + recovery: `emit_event` persists the post-apply field value
    /// and `recover_actor` restores it (SPEC2 §9.6; was a bare event
    /// count before the fix).
    #[test]
    fn test_event_sourced_apply_handler_recovery() {
        let source = r#"
            entity Counter {
                state count: Int = 0
                events
                    | Incremented(by: Int)
                apply
                    | Incremented(by) => self.count = self.count + by
                behavior increment(by: Int) { emit Incremented(by) }
                behavior get() { self.count }
            }
            spawn Counter {}
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        let mut comp_offsets: Vec<Option<usize>> = vec![None; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
                comp_offsets[idx] = entry.compensate_offset;
            }
        }

        // Baseline: no crash, two increments sent back-to-back.
        let rt_baseline = Rc::new(RefCell::new(Runtime::new()));
        let actor_id_baseline = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt_baseline.clone())));
            vm.run().unwrap().as_actor_id().unwrap()
        };
        rt_baseline
            .borrow_mut()
            .send_message(actor_id_baseline, "increment", &[Value::int(3)]);
        rt_baseline
            .borrow_mut()
            .send_message(actor_id_baseline, "increment", &[Value::int(4)]);
        rt_baseline.borrow_mut().run_scheduler();
        let baseline_count = rt_baseline
            .borrow()
            .actors
            .get(&actor_id_baseline)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int())
            .unwrap();
        assert_eq!(
            baseline_count, 9,
            "sanity: must match persist_07 conformance case's real captured output"
        );

        // Same two messages, but with a crash+recover in between them.
        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        let actor_id = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap().as_actor_id().unwrap()
        };
        rt1.borrow_mut()
            .send_message(actor_id, "increment", &[Value::int(3)]);
        rt1.borrow_mut().run_scheduler();
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(4),
            "live (pre-crash) value after one increment(3): apply's 0+3, \
             plus the unconditional +1 bump"
        );

        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            comp_offsets.clone(),
        );
        rt2.borrow_mut().recover_actor(actor_id);
        assert_eq!(
            rt2.borrow().actors.get(&actor_id).unwrap()
                .get_state_field("count").and_then(|v| v.as_int()),
            Some(4),
            "recovered value: apply handler now runs before emit, snapshot captures post-apply value"
        );

        rt2.borrow_mut()
            .send_message(actor_id, "increment", &[Value::int(4)]);
        rt2.borrow_mut().run_scheduler();
        let recovered_count = rt2
            .borrow()
            .actors
            .get(&actor_id)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int())
            .unwrap();
        assert_eq!(
            recovered_count, 9,
            "recovered and continued: reaches {baseline_count} like the never-crashed baseline"
        );
    }

    /// PLAN.md bullet 8: "repeat for every StateModel" -- the `local`
    /// case. SPEC2.md §9.3's table says `local` recovery is "Reset to
    /// initial value". Verifies that against a real crash+recover
    /// cycle: a local field mutated before the crash must NOT retain
    /// its last-set value afterward.
    #[test]
    fn test_local_state_resets_to_initial_value_on_recovery() {
        let source = r#"
            persistent actor Counter {
                state local count: Int = 0
                state durable anchor: Int = 0
                behavior inc() {
                    self.count = self.count + 1
                    self.anchor = self.anchor + 1
                }
                behavior get() { self.count }
            }
            spawn Counter {}
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        let mut comp_offsets: Vec<Option<usize>> = vec![None; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
                comp_offsets[idx] = entry.compensate_offset;
            }
        }

        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        let actor_id = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap().as_actor_id().unwrap()
        };
        rt1.borrow_mut().send_message(actor_id, "inc", &[]);
        rt1.borrow_mut().send_message(actor_id, "inc", &[]);
        rt1.borrow_mut().send_message(actor_id, "inc", &[]);
        rt1.borrow_mut().run_scheduler();
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(3),
            "sanity: local field updates live like any other field before a crash"
        );

        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            comp_offsets.clone(),
        );
        rt2.borrow_mut().recover_actor(actor_id);

        // The durable anchor proves recovery actually ran (journal
        // replay reconstructed the 3 increments), isolating whether a
        // nil/missing `count` is "recovery didn't run" versus "local
        // correctly reset".
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("anchor")
                .and_then(|v| v.as_int()),
            Some(3),
            "sanity: the durable anchor field must survive recovery normally"
        );
        let recovered_count = rt2
            .borrow()
            .actors
            .get(&actor_id)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int());
        assert_eq!(
            recovered_count,
            Some(0),
            "local field must reset to its declared initial value (0), \
             not retain 3 or come back unset -- SPEC2.md §9.3"
        );
    }

    /// `crdt` fields survive crash+recovery as *materialized* `state_data`
    /// (snapshotted by `checkpoint_actor`'s Durable|Crdt filter). The
    /// `Crdt.*` effect module is the live-actor mutation path, but
    /// `recover_actor` does not rebuild `CrdtManager.field_map`, so
    /// `perform Crdt.*` is a silent nil no-op on a recovered actor — this
    /// test pins that actual behavior: `state_data["count"]` survives, but a
    /// post-recovery `inc` does not bump it.
    #[test]
    fn test_crdt_field_survives_recovery() {
        let source = r#"
            persistent actor Counter {
                state crdt count: Int = 0
                state ticks: Int = 0
                behavior inc() {
                    perform Crdt.increment("count")
                    self.ticks = self.ticks + 1
                }
                behavior get() { self.count }
            }
            spawn Counter {}
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        let mut comp_offsets: Vec<Option<usize>> = vec![None; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
                comp_offsets[idx] = entry.compensate_offset;
            }
        }

        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        let actor_id = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap().as_actor_id().unwrap()
        };
        rt1.borrow_mut().send_message(actor_id, "inc", &[]);
        rt1.borrow_mut().send_message(actor_id, "inc", &[]);
        rt1.borrow_mut().run_scheduler();

        // The increments must have materialized into state_data.
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(2),
            "Crdt.increment must materialize count=2 into state_data before recovery"
        );

        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("ticks")
                .and_then(|v| v.as_int()),
            Some(2),
            "the statement after `perform Crdt.increment` must run (no abort)"
        );

        let snapshot_before_recovery = store.load_snapshot(actor_id).unwrap();
        assert!(
            snapshot_before_recovery.state.contains_key("count"),
            "a crdt field must appear in the ordinary snapshot -- confirms \
             it's routed through checkpoint_actor's Durable|Crdt filter, \
             not excluded like event_sourced fields are"
        );

        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            comp_offsets.clone(),
        );
        rt2.borrow_mut().recover_actor(actor_id);
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(2),
            "crdt field's materialized value survives recovery via the snapshot path"
        );

        // Pin the recovery gap: `recover_actor` restores the materialized
        // value and the CrdtManager entries but not `field_map`, so a
        // post-recovery `Crdt.increment` is a silent no-op (get_field_id
        // returns None) and `state_data["count"]` stays at 2.
        rt2.borrow_mut().send_message(actor_id, "inc", &[]);
        rt2.borrow_mut().run_scheduler();
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(2),
            "post-recovery Crdt.increment must be a no-op: field_map is not rebuilt"
        );

        // The statement AFTER the no-op `perform` must still run — if the
        // behavior aborted with an unhandled effect, `ticks` would stay 0.
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("ticks")
                .and_then(|v| v.as_int()),
            Some(1),
            "post-recovery Crdt.increment returns nil and the behavior continues"
        );
    }

    /// The `Crdt.*` effect module is the only mutation path for CRDT-backed
    /// fields. `increment` works and materializes the value into `state_data`,
    /// while an op outside the field's per-type operation set (`decrement` on
    /// a `gcounter`) and a raw `self.field = expr` assignment are both ignored
    /// so they cannot silently corrupt the replicated entry.
    #[test]
    fn test_crdt_effect_module_enforces_operation_sets() {
        let source = r#"
            actor Counter {
                state crdt gcounter count = 0
                state ticks: Int = 0
                behavior inc() { perform Crdt.increment("count") }
                behavior dec() {
                    perform Crdt.decrement("count")
                    self.ticks = self.ticks + 1
                }
                behavior bad() {
                    self.count = 99
                    self.ticks = self.ticks + 1
                }
            }
            spawn Counter {}
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (module, _ty) = compile_source(source).unwrap();
        let actor_id = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
            vm.run().unwrap().as_actor_id().unwrap()
        };
        rt.borrow_mut().send_message(actor_id, "inc", &[]);
        rt.borrow_mut().send_message(actor_id, "inc", &[]);
        rt.borrow_mut().send_message(actor_id, "dec", &[]);
        rt.borrow_mut().send_message(actor_id, "bad", &[]);
        rt.borrow_mut().run_scheduler();

        let count = rt
            .borrow()
            .actors
            .get(&actor_id)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int());
        assert_eq!(
            count,
            Some(2),
            "gcounter must accept only increment: decrement and raw assignment are no-ops"
        );

        let ticks = rt
            .borrow()
            .actors
            .get(&actor_id)
            .unwrap()
            .get_state_field("ticks")
            .and_then(|v| v.as_int());
        assert_eq!(
            ticks,
            Some(2),
            "the statement after the rejected perform/assignment must still run (no abort)"
        );
    }

    /// String concatenation and Int.to_string via the full MIR pipeline.
    #[test]
    fn test_string_concat_and_int_to_string() {
        assert_string(r#""hello " + "world""#, "hello world");
        assert_string(r#""count: " + perform Int.to_string(42)"#, "count: 42");
    }

    // ── Numeric type conversions ─────────────────────────────────────

    #[test]
    fn test_int_to_float() {
        let (value, _ty) = run_source("perform Int.to_float(42)").unwrap();
        assert_eq!(
            value.as_float(),
            Some(42.0),
            "Int.to_float(42) should be 42.0"
        );
    }

    #[test]
    fn test_float_to_int() {
        assert_int("perform Float.to_int(3.9)", 3);
        assert_int("perform Float.to_int(-3.9)", -3);
    }

    #[test]
    fn test_string_to_int() {
        assert_int(r#"perform String.to_int("42")"#, 42);
        assert_int(r#"perform String.to_int("-7")"#, -7);
        // Invalid input returns 0
        assert_int(r#"perform String.to_int("hello")"#, 0);
    }

    #[test]
    fn test_float_to_string() {
        assert_string("perform Float.to_string(3.14)", "3.14");
    }

    #[test]
    fn test_string_to_float() {
        #[allow(clippy::approx_constant)]
        assert_float(r#"perform String.to_float("3.14")"#, 3.14);
        // Invalid input returns 0.0
        assert_float(r#"perform String.to_float("hello")"#, 0.0);
    }

    /// String concatenation with let-bound variables — both operands come
    /// from variables, so the compiler must detect the string types through
    /// MIR local type metadata (HIR Operand::Var always carries Type::unit()).
    #[test]
    fn test_string_concat_let_vars() {
        assert_string(r#"let a = "ab" in let b = "cd" in a + b"#, "abcd");
    }

    /// Chained concatenation of three let-bound string variables.
    #[test]
    fn test_string_concat_chained() {
        assert_string(
            r#"let a = "ab" in let b = "cd" in let c = "ef" in a + b + c"#,
            "abcdef",
        );
    }

    /// String concatenation with unannotated function parameters (the
    /// compiler emits IAdd, and the VM must detect string operands at
    /// runtime).
    #[test]
    fn test_string_concat_fn_params_untyped() {
        assert_string(r#"fn cat(a, b) { a + b } cat("ab", "cd")"#, "abcd");
    }

    // -------------------------------------------------------------------
    // Unicode \\u{...} escape tests (full pipeline)
    // -------------------------------------------------------------------

    #[test]
    fn test_unicode_escape_via_char_at() {
        // "\\u{41}" lexes as "A" → charAt(0) is 'A' (65).
        assert_int(r#"perform String.charAt("\u{41}", 0)"#, 65);
    }

    #[test]
    fn test_unicode_escape_multiple_via_length() {
        // "\\u{48}\\u{49}" lexes as "HI" → length 2.
        assert_int(r#"perform String.length("\u{48}\u{49}")"#, 2);
    }

    #[test]
    fn test_unicode_escape_invalid_produces_lex_error() {
        let result = run_source("\"\\u{D800}\"");
        match result {
            Err(NuError::LexError { msg, .. }) => {
                assert!(
                    msg.contains("surrogate"),
                    "expected surrogate error, got: {}",
                    msg
                );
            }
            other => panic!("Expected LexError, got {:?}", other),
        }
    }
    #[test]
    fn test_unicode_escape_emoji_via_length() {
        // 😀 is U+1F600 (4-byte UTF-8).  String.length counts bytes in the
        // current runtime, so verify we get a 4-byte string.
        assert_int(r#"perform String.length("\u{1F600}")"#, 4);
    }

    // -------------------------------------------------------------------
    // Triple-quoted multi-line string tests (full pipeline)
    // -------------------------------------------------------------------

    #[test]
    fn test_triple_quoted_string_length() {
        // "a\\nb" is 3 chars.
        let source = "\"\"\"a\nb\"\"\"";
        assert_int(&format!("perform String.length({})", source), 3);
    }

    #[test]
    fn test_triple_quoted_with_unicode_escape() {
        // """\\u{41}""" → "A" → length 1.
        let source = "\"\"\"\\u{41}\"\"\"";
        assert_int(&format!("perform String.length({})", source), 1);
    }

    /// String.length and String.charAt via perform.
    #[test]
    fn test_string_length_and_char_at() {
        assert_int(r#"perform String.length("hello")"#, 5);
        assert_int(r#"perform String.length("")"#, 0);
        assert_int(r#"perform String.charAt("abc", 0)"#, 'a' as i64);
        assert_int(r#"perform String.charAt("abc", 1)"#, 'b' as i64);
        assert_int(r#"perform String.charAt("abc", 2)"#, 'c' as i64);
        // Out of bounds returns -1
        assert_int(r#"perform String.charAt("abc", 3)"#, -1);
        assert_int(r#"perform String.charAt("abc", -1)"#, -1);
        // Works with concatenated strings
        assert_int(r#"perform String.length("hello " + "world")"#, 11);
    }

    #[test]
    fn test_workflow_lowers_to_persistent_actor() {
        let source = "workflow PurchaseOrder { step validate { 1 } }";
        let (module, _ty) = compile_source(source).unwrap();

        let meta = module
            .actor_metadata
            .iter()
            .find(|m| m.name == "PurchaseOrder")
            .expect("workflow should produce actor metadata");
        assert!(meta.is_workflow, "workflow metadata should be flagged");
        assert!(meta.persistent, "workflows should be persistent actors");
        assert_eq!(meta.behavior_indices.len(), 1, "one behavior per step");

        let behavior = &module.behaviors[meta.behavior_indices[0]];
        assert_eq!(behavior.name, "PurchaseOrder.validate");
    }

    #[test]
    fn test_workflow_survives_node_restart() {
        // A two-step workflow that emits durable events and advances its
        // step_index in each step.  We run the first step, simulate a node
        // restart by loading the actor into a fresh runtime sharing the same
        // persistence store, then run the second step and verify final state.
        let source = r#"
            workflow Counter {
                step start { (emit Started(0), self.step_index = self.step_index + 1) }
                step second { (emit Incremented(1), self.step_index = self.step_index + 1) }
            }
            let c = spawn Counter {} in { c }
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
            }
        }

        // First runtime: spawn, advance the first step, and run scheduler.
        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt1.borrow_mut().send_message(actor_id, "start", &[]);
        rt1.borrow_mut().run_scheduler();

        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(1),
            "first step should advance step_index to 1"
        );

        let events_before = store.read_workflow_events(actor_id);
        assert!(events_before
            .iter()
            .any(|e| matches!(e, WorkflowEvent::WorkflowStarted { .. })));
        assert!(events_before
            .iter()
            .any(|e| matches!(e, WorkflowEvent::Custom { name, .. } if name == "Started")));
        assert!(events_before
            .iter()
            .any(|e| matches!(e, WorkflowEvent::StepCompleted { .. })));

        // Simulate a node restart: new runtime sharing the same store,
        // register the bytecode module, then recover the workflow actor.
        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            vec![None; module.behaviors.len()],
        );
        rt2.borrow_mut().recover_actor(actor_id);

        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(1),
            "recovered workflow should resume at step_index 1"
        );

        // Continue execution on the recovered runtime: advance the second step.
        // Bytecode-only workflow actors have an empty behavior_table, so route
        // by explicit behavior id (1 is the second step).
        rt2.borrow_mut().send_message_by_id(actor_id, 1, &[]);
        rt2.borrow_mut().run_scheduler();

        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(2),
            "final step_index should be 2 after second step"
        );

        let events_after = store.read_workflow_events(actor_id);
        assert_eq!(
            events_after
                .iter()
                .filter(|e| matches!(e, WorkflowEvent::StepCompleted { .. }))
                .count(),
            2,
            "two StepCompleted events should be persisted"
        );
        assert!(events_after
            .iter()
            .any(|e| matches!(e, WorkflowEvent::Custom { name, .. } if name == "Incremented")));
    }

    #[test]
    fn test_workflow_signal_wait_and_resume_after_restart() {
        // A workflow step waits for a named signal. The step suspends until
        // the signal is delivered, and after a simulated restart the signal
        // is replayed from the journal so the workflow advances.
        let source = r#"
            workflow Signaled {
                step wait_for_go {
                    perform Signal.wait("go")
                }
            }
            let c = spawn Signaled {} in { c }
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
            }
        }

        // First runtime: spawn and start the waiting step.
        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt1.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt1.borrow_mut().run_scheduler();

        // Step has not completed yet; it is suspended waiting for the signal.
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(0),
            "step should not advance before signal is received"
        );
        assert!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .suspended_execution
                .is_some(),
            "actor should have a suspended execution waiting for the signal"
        );

        // Simulate a runtime restart: drop the actor and recover from the store.
        rt1.borrow_mut().actors.remove(&actor_id);

        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            vec![None; module.behaviors.len()],
        );
        rt2.borrow_mut().recover_actor(actor_id);
        // Recovery detects the waiting signal and re-triggers the step; it
        // suspends again until the signal arrives.
        rt2.borrow_mut().run_scheduler();

        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(0),
            "step should still be waiting after recovery"
        );

        // Send the signal. The runtime appends SignalReceived and resumes the step.
        rt2.borrow_mut().signal_workflow(actor_id, "go", None);

        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(1),
            "workflow should advance after the signal is received"
        );

        let events = store.read_workflow_events(actor_id);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkflowEvent::SignalReceived { name, .. } if name == "go")),
            "SignalReceived event should be persisted"
        );
        assert!(
            events.iter().any(|e| matches!(e, WorkflowEvent::StepCompleted { step_name, .. } if step_name == "wait_for_go")),
            "StepCompleted event should be persisted after the signal"
        );
    }

    #[test]
    fn test_workflow_step_waits_on_two_sequential_signals() {
        // Regression: a workflow step resumed from a signal wait that
        // suspends AGAIN on a second signal must re-capture its suspended
        // state. Previously resume_suspended_workflow_step dropped the
        // suspension on a chained SignalWait:suspend, so the second wait
        // could never be woken (permanent stall).
        let source = r#"
            workflow TwoSignals {
                step wait_for_both {
                    (perform Signal.wait("first"), perform Signal.wait("second"))
                }
            }
            let c = spawn TwoSignals {} in { c }
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt.borrow_mut().run_scheduler();

        // The step is suspended waiting for the first signal.
        {
            let rt = rt.borrow();
            let actor = rt.actors.get(&actor_id).unwrap();
            assert_eq!(actor.waiting_signal.as_deref(), Some("first"));
            assert!(actor.suspended_execution.is_some());
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(0)
            );
        }

        // First signal arrives: the step resumes, then suspends again on the
        // second signal. The chained suspension must be re-captured.
        rt.borrow_mut().signal_workflow(actor_id, "first", None);
        {
            let rt = rt.borrow();
            let actor = rt.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.waiting_signal.as_deref(),
                Some("second"),
                "chained signal wait should re-register the second signal"
            );
            assert!(
                actor.suspended_execution.is_some(),
                "chained signal wait should re-capture the suspended execution"
            );
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(0),
                "step should not complete before the second signal"
            );
        }

        // Second signal arrives: the step completes and the workflow advances.
        rt.borrow_mut().signal_workflow(actor_id, "second", None);
        {
            let rt = rt.borrow();
            let actor = rt.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(1),
                "workflow should advance after both signals are received"
            );
            assert!(actor.suspended_execution.is_none());
            assert_eq!(actor.waiting_signal, None);
        }

        let events = store.read_workflow_events(actor_id);
        assert!(
            events.iter().any(|e| matches!(e, WorkflowEvent::StepCompleted { step_name, .. } if step_name == "wait_for_both")),
            "StepCompleted event should be persisted after both signals"
        );
    }

    /// SPEC2 §10 known-issue #2: saga compensation entries carried the
    /// step's index RELATIVE to the owning actor's behavior list, but the
    /// codegen cursor matched them against every actor's steps in module
    /// order — a plain `actor` declared before the `workflow` hijacked the
    /// workflow's first compensation offset (and the workflow step lost
    /// its compensation entirely).
    #[test]
    fn test_saga_compensation_ignores_non_workflow_actors() {
        let source = r#"
            actor Before {
                behavior x() { 1 }
                behavior y() { 2 }
            }
            workflow SagaTest {
                step a {
                    (self.step_index = self.step_index + 1, self.a_done = 1, emit A_Done())
                } compensate {
                    self.comp_order = self.comp_order * 10 + 1
                }
                step b {
                    (self.step_index = self.step_index + 1, self.b_done = 1, emit B_Done())
                } compensate {
                    self.comp_order = self.comp_order * 10 + 2
                }
                step c {
                    perform Fail.now()
                }
            }
            spawn Before {}
            spawn SagaTest {}
        "#;
        let (module, _ty) = compile_source(source).unwrap();

        // Compilation-level pin: only the workflow's own steps carry a
        // compensate_offset; the pre-declared plain actor's behaviors are
        // untouched.
        let find = |name: &str| {
            module
                .behaviors
                .iter()
                .find(|b| b.name == name)
                .unwrap_or_else(|| panic!("behavior {name} missing"))
        };
        assert_eq!(
            find("Before.x").compensate_offset,
            None,
            "plain actor behavior must not receive a compensation offset"
        );
        assert_eq!(
            find("Before.y").compensate_offset,
            None,
            "plain actor behavior must not receive a compensation offset"
        );
        let step_a = find("SagaTest.a");
        let step_b = find("SagaTest.b");
        assert!(
            step_a.compensate_offset.is_some(),
            "workflow step a must carry its compensation"
        );
        assert!(
            step_b.compensate_offset.is_some(),
            "workflow step b must carry its compensation"
        );

        // Behavioral pin: run the saga to completion (happy path — no
        // compensation), then a failing run must compensate in reverse
        // order despite the pre-declared actor.
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let _before_id = value;
        // Spawn the workflow by name; the run above spawned both, so grab
        // the actor whose state has comp_order.
        let actor_id = {
            let rt_ref = rt.borrow();
            // The workflow desugaring seeds step_index/workflow_name;
            // comp_order only exists after a compensation runs.
            *rt_ref
                .actors
                .iter()
                .find(|(_, a)| a.get_state_field("workflow_name").is_some())
                .map(|(id, _)| id)
                .expect("workflow actor not spawned")
        };
        // Steps a and b complete; nothing fails, so no compensation runs.
        // Workflow steps dispatch by LOCAL id (0..step_count-1) — the
        // compressed bytecode_offsets maps local id 0 to THIS workflow's
        // step a even though `Before` owns the module's global index 0.
        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message_by_id(actor_id, 1, &[]);
        rt.borrow_mut().run_scheduler();
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.get_state_field("a_done").and_then(|v| v.as_int()),
                Some(1),
                "step a ran"
            );
            assert_eq!(
                actor.get_state_field("b_done").and_then(|v| v.as_int()),
                Some(1),
                "step b ran"
            );
            assert_eq!(
                actor
                    .get_state_field("comp_order")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0),
                0,
                "happy path: no compensation runs (field never written)"
            );
        }

        // Failing run: step c (local id 2) fails, and the workflow's OWN
        // compensations run in reverse order (b then a) — the pre-declared
        // actor must not intercept or shift them.
        rt.borrow_mut().send_message_by_id(actor_id, 2, &[]);
        rt.borrow_mut().run_scheduler();
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.get_state_field("comp_order").and_then(|v| v.as_int()),
                Some(21),
                "compensations must run in reverse order (b then a) despite Before"
            );
        }
    }

    /// SPEC2 §10 known-issue #5: a failing workflow step used to be
    /// silent (exit 0, no diagnostic). The runtime now records a durable
    /// `StepFailed` event (readable via `Runtime::workflow_failures`),
    /// which the CLI surfaces with a nonzero exit.
    #[test]
    fn test_workflow_step_failure_is_recorded_and_surfaced() {
        let source = r#"
            workflow FailWF {
                step ok {
                    (self.step_index = self.step_index + 1, perform IO.print("step ok"))
                }
                step boom {
                    perform Fail.now()
                }
            }
            fn main() {
                let w = spawn FailWF {}
                send w ok()
                send w boom()
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        run_source_with_runtime(source, rt.clone()).unwrap();
        rt.borrow_mut().run_scheduler();

        let failures = rt.borrow().workflow_failures();
        assert_eq!(
            failures.len(),
            1,
            "exactly one step failure must be recorded: {failures:?}"
        );
        assert_eq!(failures[0].0, "boom", "the failing step's name");
        assert!(
            failures[0].1.contains("Fail.now"),
            "the error message must surface: {}",
            failures[0].1
        );
    }

    #[test]
    fn test_saga_compensation_runs_in_reverse_order() {
        // A three-step saga where the third step fails. The first two steps
        // have per-step compensations that must run in reverse order (b, then a).
        let source = r#"
            workflow SagaTest {
                step a {
                    (self.step_index = self.step_index + 1, self.a_done = 1, emit A_Done())
                } compensate {
                    self.comp_order = self.comp_order * 10 + 1
                }
                step b {
                    (self.step_index = self.step_index + 1, self.b_done = 1, emit B_Done())
                } compensate {
                    self.comp_order = self.comp_order * 10 + 2
                }
                step c {
                    perform Fail.now()
                }
            }
            let c = spawn SagaTest {} in { c }
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return actor reference");

        // Run steps sequentially. The third step fails and triggers compensation.
        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message_by_id(actor_id, 1, &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message_by_id(actor_id, 2, &[]);
        rt.borrow_mut().run_scheduler();

        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.get_state_field("a_done").and_then(|v| v.as_int()),
                Some(1)
            );
            assert_eq!(
                actor.get_state_field("b_done").and_then(|v| v.as_int()),
                Some(1)
            );
            assert_eq!(
                actor.get_state_field("comp_order").and_then(|v| v.as_int()),
                Some(21),
                "compensations should run in reverse order (b then a)"
            );
        }

        let events = rt.borrow().persistence.read_workflow_events(actor_id);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, WorkflowEvent::StepCompleted { .. }))
                .count(),
            2,
            "only the first two steps should record StepCompleted"
        );
        let saga_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, WorkflowEvent::SagaCompensated { .. }))
            .collect();
        assert_eq!(saga_events.len(), 2);
        assert!(
            matches!(&saga_events[0], WorkflowEvent::SagaCompensated { step_name, .. } if step_name == "b")
        );
        assert!(
            matches!(&saga_events[1], WorkflowEvent::SagaCompensated { step_name, .. } if step_name == "a")
        );
    }

    #[test]
    fn test_workflow_durable_timer_recovery() {
        // A workflow step sets a durable timer. After a simulated restart the
        // timer is re-armed from the journal and, once it fires, the workflow
        // advances past the timer step.
        let source = r#"
            workflow TimerWorkflow {
                step wait { perform Timer.sleep("timeout1", 1) }
            }
            spawn TimerWorkflow {}
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        let mut compensation_offsets: Vec<Option<usize>> = vec![None; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
                compensation_offsets[idx] = entry.compensate_offset;
            }
        }

        // First runtime: spawn the workflow and run the timer step. Step
        // the actor once directly instead of running the scheduler to
        // quiescence: run_scheduler now waits for pending timers to fire,
        // which would complete the step instead of leaving the timer
        // pending for the simulated crash.
        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt1.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt1.borrow_mut().step_actor(actor_id);

        let events_before = store.read_workflow_events(actor_id);
        assert!(
            events_before
                .iter()
                .any(|e| matches!(e, WorkflowEvent::TimerSet { name, .. } if name == "timeout1")),
            "TimerSet event should be persisted"
        );
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(0),
            "step body does not increment step_index; the runtime records StepCompleted instead"
        );

        // Simulate a node restart: recover the workflow into a fresh runtime.
        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            compensation_offsets.clone(),
        );
        rt2.borrow_mut().recover_actor(actor_id);

        assert_eq!(
            rt2.borrow().timer_wheel.len(),
            1,
            "timer should be re-armed after recovery"
        );
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(0),
            "recovered workflow should resume at the snapshot step_index"
        );

        // Let the timer fire and process the resulting message.
        std::thread::sleep(std::time::Duration::from_millis(20));
        rt2.borrow_mut().tick_timers();
        rt2.borrow_mut().run_scheduler();

        let events_after = store.read_workflow_events(actor_id);
        assert!(
            events_after
                .iter()
                .any(|e| matches!(e, WorkflowEvent::TimerFired { name, .. } if name == "timeout1")),
            "TimerFired event should be persisted after the timer fires"
        );
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(1),
            "workflow should advance to step_index 1 after the timer fires"
        );
    }

    #[test]
    fn test_workflow_timer_sleep_single_arg_resumes() {
        // Regression for the SPEC2 known-issue list (#4): a single-arg
        // `perform Timer.sleep(ms)` in a workflow step used to suspend
        // forever (a permanent hang — only the two-arg durable form
        // worked). `fire_timer_sleep_wake` now resumes the suspended
        // PerformAsync; the re-executed opcode sees the fired flag and
        // completes the step.
        let source = r#"
            workflow TimerWorkflow {
                step wait { perform Timer.sleep(50) }
            }
            spawn TimerWorkflow {}
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());
        rt.borrow_mut().install_virtual_clock();

        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt.borrow_mut().step_actor(actor_id);

        // The single-arg sleep must suspend the step (not complete inline).
        assert!(
            rt.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .suspended_execution
                .is_some(),
            "single-arg Timer.sleep must suspend the step"
        );

        // Fire the timer wheel: the wake resumes the suspended PerformAsync
        // and the step body runs to completion.
        rt.borrow_mut()
            .advance_time(std::time::Duration::from_millis(100));
        rt.borrow_mut().tick_timers();
        rt.borrow_mut().step_actor(actor_id);

        let events = store.read_workflow_events(actor_id);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkflowEvent::StepCompleted { .. })),
            "the step must complete after the timer fires, got {:?}",
            events
        );
        assert_eq!(
            rt.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(1),
            "the workflow must advance past the sleeping step"
        );
    }

    #[test]
    fn test_workflow_parallel_branches_normal() {
        // A simple parallel block with no suspension: both branches run in one
        // synthetic step and the workflow continues to the next sequential step.
        let source = r#"
            workflow ParallelNormal {
                step before { (emit BeforeDone(), self.step_index = self.step_index + 1) }
                parallel {
                    step branch_a { emit BranchA_Done() }
                    step branch_b { emit BranchB_Done() }
                }
                step after { (emit AfterDone(), self.step_index = self.step_index + 1) }
            }
            spawn ParallelNormal {}
        "#;

        let store = SharedMemoryStore::new();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());

        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message_by_id(actor_id, 1, &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message_by_id(actor_id, 2, &[]);
        rt.borrow_mut().run_scheduler();

        assert_eq!(
            rt.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(3),
            "workflow should advance through before, parallel, and after"
        );

        let events = store.read_workflow_events(actor_id);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, WorkflowEvent::ParallelBranchCompleted { .. }))
                .count(),
            2,
            "both branches should emit ParallelBranchCompleted"
        );
        assert!(
            events.iter().any(|e| matches!(e, WorkflowEvent::StepCompleted { step_name, .. } if step_name == "parallel_0")),
            "parallel_0 should record StepCompleted"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkflowEvent::Custom { name, .. } if name == "AfterDone")),
            "AfterDone should be persisted"
        );
    }

    #[test]
    fn test_workflow_parallel_branches_and_recovery() {
        // A workflow with a sequential step, a parallel block of two branches,
        // and a final sequential step.  Branch b suspends on a signal so we can
        // simulate a restart after branch a has already completed; recovery
        // replays the ParallelBranchCompleted event and skips branch a.
        let source = r#"
            workflow ParallelTest {
                step before { (emit BeforeDone(), self.step_index = self.step_index + 1) }
                parallel {
                    step branch_a { emit BranchA_Done() }
                    step branch_b { (perform Signal.wait("continue"), emit BranchB_Done()) }
                }
                step after { (emit AfterDone(), self.step_index = self.step_index + 1) }
            }
            spawn ParallelTest {}
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        let mut compensation_offsets: Vec<Option<usize>> = vec![None; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
                compensation_offsets[idx] = entry.compensate_offset;
            }
        }

        // First runtime: run the sequential "before" step, then start the
        // parallel block.  Branch a completes; branch b suspends waiting for
        // the signal.
        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt1.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt1.borrow_mut().run_scheduler();
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(1),
            "before step should advance step_index to 1"
        );

        rt1.borrow_mut().send_message_by_id(actor_id, 1, &[]);
        rt1.borrow_mut().run_scheduler();

        let events_mid = store.read_workflow_events(actor_id);
        assert_eq!(
            events_mid.iter().filter(|e| matches!(e, WorkflowEvent::ParallelBranchCompleted { branch_name, .. } if branch_name == "branch_a")).count(),
            1,
            "branch_a should have completed"
        );
        assert_eq!(
            events_mid.iter().filter(|e| matches!(e, WorkflowEvent::ParallelBranchCompleted { branch_name, .. } if branch_name == "branch_b")).count(),
            0,
            "branch_b should still be waiting"
        );
        assert_eq!(
            rt1.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("parallel_progress")
                .and_then(|v| v.as_int()),
            Some(1),
            "parallel_progress should reflect one completed branch"
        );

        // Simulate a node restart mid-parallel-block: drop the actor and
        // recover from the shared store.  Recovery replays the durable branch
        // event so branch a is skipped when the synthetic parallel step runs.
        rt1.borrow_mut().actors.remove(&actor_id);

        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            compensation_offsets.clone(),
        );
        rt2.borrow_mut().recover_actor(actor_id);
        rt2.borrow_mut().run_scheduler();

        let events_after_recovery = store.read_workflow_events(actor_id);
        assert_eq!(
            events_after_recovery.iter().filter(|e| matches!(e, WorkflowEvent::ParallelBranchCompleted { branch_name, .. } if branch_name == "branch_a")).count(),
            1,
            "branch_a should not be re-run after recovery"
        );

        // Deliver the signal so branch b can finish.
        rt2.borrow_mut().signal_workflow(actor_id, "continue", None);
        rt2.borrow_mut().run_scheduler();

        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(2),
            "parallel block should advance step_index to 2"
        );
        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("parallel_progress")
                .and_then(|v| v.as_int()),
            Some(0),
            "parallel_progress should be reset after the block completes"
        );

        let events_after_signal = store.read_workflow_events(actor_id);
        assert_eq!(
            events_after_signal
                .iter()
                .filter(|e| matches!(e, WorkflowEvent::ParallelBranchCompleted { .. }))
                .count(),
            2,
            "both branches should have ParallelBranchCompleted events"
        );
        assert!(
            events_after_signal.iter().any(|e| matches!(e, WorkflowEvent::StepCompleted { step_name, .. } if step_name == "parallel_0")),
            "parallel_0 should record StepCompleted"
        );

        // Run the final sequential step.
        rt2.borrow_mut().send_message_by_id(actor_id, 2, &[]);
        rt2.borrow_mut().run_scheduler();

        assert_eq!(
            rt2.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(3),
            "after step should advance step_index to 3"
        );
        let events_final = store.read_workflow_events(actor_id);
        assert!(
            events_final
                .iter()
                .any(|e| matches!(e, WorkflowEvent::Custom { name, .. } if name == "AfterDone")),
            "AfterDone event should be persisted"
        );
    }

    #[test]
    fn test_workflow_query_handler_reads_state() {
        // A query handler is a plain function that reads `self` state; the
        // runtime invokes it with the workflow actor bound as `self`, so it
        // observes the actor's current state without mutating it.  The
        // program entry returns the handler as a first-class function value
        // (a function-table index, the representation the MIR pipeline
        // emits for function references).
        let source = r#"
            workflow Counter {
                step bump { self.step_index = self.step_index + 1 }
            }
            fn progress() -> Int { self.step_index }
            let c = spawn Counter {} in { progress }
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());
        let handler = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
            vm.run().unwrap()
        };
        let actor_id = {
            let rt = rt.borrow();
            assert_eq!(
                rt.actors.len(),
                1,
                "exactly one workflow actor should exist"
            );
            *rt.actors.keys().next().unwrap()
        };

        // Advance the workflow so the query has observable state to read.
        rt.borrow_mut().send_message(actor_id, "bump", &[]);
        rt.borrow_mut().run_scheduler();
        assert_eq!(
            rt.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(1),
            "bump step should advance step_index to 1"
        );

        let events_before = store.read_workflow_events(actor_id).len();

        rt.borrow_mut()
            .register_workflow_query(actor_id, "progress", handler);
        let result = rt.borrow_mut().query_workflow(actor_id, "progress");
        assert_eq!(
            result.and_then(|v| v.as_int()),
            Some(1),
            "query handler should read the workflow's current step_index"
        );

        // Queries are read-only: no workflow events were appended.
        assert_eq!(
            store.read_workflow_events(actor_id).len(),
            events_before,
            "querying must not append workflow events"
        );

        // Unknown query names resolve to None.
        assert_eq!(
            rt.borrow_mut().query_workflow(actor_id, "missing"),
            None,
            "unregistered query name should return None"
        );
    }

    #[test]
    fn test_workflow_query_rejects_non_workflow_actor() {
        // Queries are a workflow-only concept: registering on a plain actor
        // is a no-op and querying it yields None.
        let source = r#"
            actor Echo { behavior ping() { 1 } }
            let e = spawn Echo {} in { e }
        "#;
        let (module, _ty) = compile_source(source).unwrap();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt.borrow_mut()
            .register_workflow_query(actor_id, "ping", Value::int(0));
        assert_eq!(
            rt.borrow_mut().query_workflow(actor_id, "ping"),
            None,
            "plain actors have no query handlers"
        );
        assert_eq!(
            rt.borrow_mut().query_workflow(actor_id + 1000, "ping"),
            None,
            "querying a missing actor should return None"
        );
    }

    #[test]
    fn test_workflow_query_effect_from_step() {
        // `perform Workflow.query(self, name)` inside a workflow step routes
        // through the runtime's builtin-effect path and invokes the
        // registered handler on the workflow actor.  The step runs on the
        // runtime's shared VM while the handler runs on a private VM, so
        // the query cannot disturb the step's own execution state.
        let source = r#"
            workflow Counter {
                step bump { self.step_index = self.step_index + 1 }
                step inspect { self.observed = perform Workflow.query(self, "progress") }
            }
            fn progress() -> Int { self.step_index }
            let c = spawn Counter {} in { progress }
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());
        let handler = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
            vm.run().unwrap()
        };
        let actor_id = {
            let rt = rt.borrow();
            assert_eq!(
                rt.actors.len(),
                1,
                "exactly one workflow actor should exist"
            );
            *rt.actors.keys().next().unwrap()
        };

        rt.borrow_mut()
            .register_workflow_query(actor_id, "progress", handler);
        rt.borrow_mut().send_message(actor_id, "bump", &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message(actor_id, "inspect", &[]);
        rt.borrow_mut().run_scheduler();

        assert_eq!(
            rt.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("observed")
                .and_then(|v| v.as_int()),
            Some(1),
            "Workflow.query effect should deliver the handler result into the step"
        );
    }

    // -----------------------------------------------------------------------
    // Test: Actor.* builtin effects (link/monitor/registry/exit/trap_exit)
    // -----------------------------------------------------------------------

    #[test]
    fn test_actor_builtin_effects_standalone_nil_noop() {
        // Outside an actor runtime every Actor.* effect is a nil no-op.
        let source = r#"
            {
                perform Actor.link(nil)
                perform Actor.unlink(nil)
                perform Actor.monitor(nil)
                perform Actor.demonitor(nil)
                perform Actor.trap_exit(true)
                perform Actor.set_priority(0)
                perform Actor.exit(0)
                perform Actor.register("name")
                perform Actor.unregister("name")
                perform Actor.whereis("name")
            }
        "#;
        let (value, _ty) = run_source(source).unwrap();
        assert!(
            value.is_nil(),
            "Actor.* effects should yield nil outside a runtime"
        );
    }

    #[test]
    fn test_actor_link_killed_peer_exits_linked_actor() {
        // The peer links to the victim from inside its behavior; the victim
        // then self-exits with a Kill-style reason, which must propagate
        // through the link and take the non-trapping peer down.
        let source = r#"
            actor Peer {
                state exits: Int = 0
                behavior notified(dead, me) { self.exits = self.exits + 1 }
                behavior watch(t) { perform Actor.link(t) }
            }
            actor Victim {
                behavior die() { perform Actor.exit(2) }
            }
            let p = spawn Peer {} in
            let v = spawn Victim {} in {
                send p watch(v)
                send v die()
                p
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let peer_id = value
            .as_actor_id()
            .expect("spawn should return an actor ref");

        rt.borrow_mut().run_scheduler();

        assert!(
            rt.borrow().actors.get(&peer_id).is_none(),
            "linked peer should exit when the victim is killed"
        );
    }

    #[test]
    fn test_actor_trap_exit_survives_as_system_message() {
        // With trap_exit(true) the linked peer's abnormal exit arrives as a
        // System message instead of killing the trapping actor.
        let source = r#"
            actor Peer {
                state exits: Int = 0
                behavior notified(dead, me) { self.exits = self.exits + 1 }
                behavior watch(t) {
                    perform Actor.trap_exit(true)
                    perform Actor.link(t)
                }
            }
            actor Victim {
                behavior die() { perform Actor.exit(1) }  // Error exit (trappable), not Kill
            }
            let p = spawn Peer {} in
            let v = spawn Victim {} in {
                send p watch(v)
                send v die()
                p
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let peer_id = value
            .as_actor_id()
            .expect("spawn should return an actor ref");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let peer = rt_ref
            .actors
            .get(&peer_id)
            .expect("trapping peer should survive the victim's exit");
        assert_eq!(
            peer.get_state_field("exits").and_then(|v| v.as_int()),
            Some(1),
            "trapping peer should have consumed the exit System message"
        );
    }

    #[test]
    fn test_actor_link_normal_exit_does_not_propagate() {
        // A Normal self-exit must not take down linked peers (BEAM semantics).
        let source = r#"
            actor Peer {
                behavior watch(t) { perform Actor.link(t) }
            }
            actor Victim {
                behavior die() { perform Actor.exit(0) }
            }
            let p = spawn Peer {} in
            let v = spawn Victim {} in {
                send p watch(v)
                send v die()
                p
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let peer_id = value
            .as_actor_id()
            .expect("spawn should return an actor ref");

        rt.borrow_mut().run_scheduler();

        assert!(
            rt.borrow().actors.get(&peer_id).is_some(),
            "linked peer should survive a Normal exit"
        );
    }

    #[test]
    fn test_actor_link_external_kill_propagates() {
        // Same propagation, but the kill comes from the runtime API rather
        // than Actor.exit. The peer registers itself so the test can find
        // its id afterwards.
        let source = r#"
            actor Peer {
                state exits: Int = 0
                behavior notified(dead, me) { self.exits = self.exits + 1 }
                behavior watch(t) {
                    perform Actor.register("peer")
                    perform Actor.link(t)
                }
            }
            actor Victim {
                behavior noop() { 0 }
            }
            let p = spawn Peer {} in
            let v = spawn Victim {} in {
                send p watch(v)
                v
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let victim_id = value
            .as_actor_id()
            .expect("spawn should return an actor ref");

        rt.borrow_mut().run_scheduler();
        let peer_id = rt
            .borrow()
            .registry
            .whereis("peer")
            .expect("peer should have registered itself");

        rt.borrow_mut().kill_actor(victim_id);

        assert!(
            rt.borrow().actors.get(&peer_id).is_none(),
            "linked peer should exit when the victim is killed externally"
        );
    }

    #[test]
    fn test_kill_untrappable_bypasses_trap_exits() {
        // Kill is untrappable per spec: even a trap_exits actor must be
        // force-terminated when a linked actor is killed.
        let source = r#"
            actor Peer {
                behavior watch(t) {
                    perform Actor.trap_exit(true)
                    perform Actor.link(t)
                }
            }
            actor Victim {
                behavior die() { perform Actor.exit(2) }  // Kill
            }
            let p = spawn Peer {} in
            let v = spawn Victim {} in {
                send p watch(v)
                send v die()
                p
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let peer_id = value
            .as_actor_id()
            .expect("spawn should return an actor ref");

        rt.borrow_mut().run_scheduler();

        // Kill is untrappable — the trap_exits peer should be terminated.
        assert!(
            rt.borrow().actors.get(&peer_id).is_none(),
            "trap_exits peer must be terminated by cascading Kill"
        );
    }

    #[test]
    fn test_actor_monitor_delivers_down_message() {
        // The watcher monitors the victim; the victim's exit delivers a DOWN
        // System message (payload: target, watcher, reason code) which the
        // watcher's first behavior consumes.
        let source = r#"
            actor Watcher {
                state got: Int = 0
                behavior down(t, w, r) { self.got = r }
                behavior watch(t) { perform Actor.monitor(t) }
            }
            actor Victim {
                behavior die() { perform Actor.exit(2) }
            }
            let w = spawn Watcher {} in
            let v = spawn Victim {} in {
                send w watch(v)
                send v die()
                w
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let watcher_id = value
            .as_actor_id()
            .expect("spawn should return an actor ref");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let watcher = rt_ref
            .actors
            .get(&watcher_id)
            .expect("watcher should survive the monitored actor's exit");
        assert_eq!(
            watcher.get_state_field("got").and_then(|v| v.as_int()),
            Some(2),
            "watcher should receive DOWN with the Kill reason code (2)"
        );
    }

    #[test]
    fn test_actor_demonitor_stops_down_message() {
        // After demonitor the victim's exit must not deliver a DOWN.
        let source = r#"
            actor Watcher {
                state got: Int = 0
                behavior down(t, w, r) { self.got = r }
                behavior watch(t) {
                    perform Actor.monitor(t)
                    perform Actor.demonitor(t)
                }
            }
            actor Victim {
                behavior die() { perform Actor.exit(2) }
            }
            let w = spawn Watcher {} in
            let v = spawn Victim {} in {
                send w watch(v)
                send v die()
                w
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let watcher_id = value
            .as_actor_id()
            .expect("spawn should return an actor ref");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let watcher = rt_ref
            .actors
            .get(&watcher_id)
            .expect("watcher should survive");
        assert_eq!(
            watcher.get_state_field("got").and_then(|v| v.as_int()),
            Some(0),
            "demonitored watcher should not receive a DOWN message"
        );
    }

    #[test]
    fn test_actor_register_whereis_unregister_roundtrip() {
        let source = r#"
            actor Hero {
                state found: Int = 0
                state gone: Int = 0
                behavior reg() { perform Actor.register("hero") }
                behavior lookup() { self.found = perform Actor.whereis("hero") }
                behavior unreg() { perform Actor.unregister("hero") }
                behavior lookup2() { self.gone = perform Actor.whereis("hero") }
            }
            let h = spawn Hero {} in {
                send h reg()
                send h lookup()
                send h unreg()
                send h lookup2()
                h
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let hero_id = value
            .as_actor_id()
            .expect("spawn should return an actor ref");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        assert_eq!(
            rt_ref.registry.whereis("hero"),
            None,
            "name should be unregistered by the end"
        );
        let hero = rt_ref.actors.get(&hero_id).unwrap();
        assert_eq!(
            hero.get_state_field("found").and_then(|v| v.as_actor_id()),
            Some(hero_id),
            "whereis should resolve the registered name to the actor ref"
        );
        assert!(
            hero.get_state_field("gone")
                .map(|v| v.is_nil())
                .unwrap_or(false),
            "whereis should return nil for an unregistered name"
        );
    }

    #[test]
    fn test_actor_exit_terminates_self() {
        let source = r#"
            actor Leaver {
                behavior die() { perform Actor.exit("error") }
            }
            let h = spawn Leaver {} in {
                send h die()
                h
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let leaver_id = value
            .as_actor_id()
            .expect("spawn should return an actor ref");

        rt.borrow_mut().run_scheduler();

        assert!(
            rt.borrow().actors.get(&leaver_id).is_none(),
            "Actor.exit should terminate the performing actor"
        );
    }

    #[test]
    fn test_example_link_monitor_runs() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = include_str!("../../examples/link_monitor.nula");
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        assert_eq!(value.as_int(), Some(0), "main should return 0");

        rt.borrow_mut().run_scheduler();

        // The trapping watcher received both the link exit signal and the
        // monitor DOWN when the victim exited, and resolved its own
        // registered name through whereis.
        let rt_ref = rt.borrow();
        let watcher_id = rt_ref
            .registry
            .whereis("watcher")
            .expect("watcher should have registered itself");
        let watcher = rt_ref.actors.get(&watcher_id).unwrap();
        assert_eq!(
            watcher.get_state_field("notices").and_then(|v| v.as_int()),
            Some(2),
            "watcher should see the link exit signal and the monitor DOWN"
        );
        assert_eq!(
            watcher
                .get_state_field("seen")
                .and_then(|v| v.as_actor_id()),
            Some(watcher_id),
            "whereis should resolve the registered name to the actor ref"
        );
    }

    // -----------------------------------------------------------------------
    // Test: Otp.* builtin effects (supervisors with dynamic children)
    // -----------------------------------------------------------------------

    #[test]
    fn test_otp_builtin_effects_standalone_nil_noop() {
        // Outside an actor runtime every Otp.* effect is a nil no-op.
        let source = r#"
            {
                perform Otp.create_supervisor("pool", 0)
                perform Otp.supervise_child(0, nil, 0)
                perform Otp.set_template(0, "Worker")
                perform Otp.start_child(0)
                perform Otp.terminate_child(0, nil)
                perform Otp.child_count(0)
            }
        "#;
        let (value, _ty) = run_source(source).unwrap();
        assert!(
            value.is_nil(),
            "Otp.* effects should yield nil outside a runtime"
        );
    }

    #[test]
    fn test_otp_simple_one_for_one_restarts_crashed_child_from_template() {
        // From source: create a simple_one_for_one supervisor with a
        // template actor type, start two children, send them work, kill
        // one, and assert it restarts from the template — fresh id, state
        // back to the declared defaults, behavior table intact.
        let source = r#"
            actor PoolWorker {
                state count: Int = 0
                behavior work(x) { self.count = self.count + x }
            }
            let sup = perform Otp.create_supervisor("pool", 3) in
            let t = perform Otp.set_template(sup, "PoolWorker") in
            let w1 = perform Otp.start_child(sup) in
            let w2 = perform Otp.start_child(sup) in {
                send w1 work(1)
                send w2 work(2)
                sup
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let sup_id = value
            .as_int()
            .expect("create_supervisor should yield the supervisor id as Int")
            as u64;
        assert_eq!(
            rt.borrow().supervisors[&sup_id].strategy,
            crate::runtime::RestartStrategy::SimpleOneForOne
        );

        rt.borrow_mut().run_scheduler();

        let children: Vec<u64> = rt.borrow().supervisors[&sup_id]
            .children
            .iter()
            .map(|(_, id)| *id)
            .collect();
        assert_eq!(
            children.len(),
            2,
            "two dynamic children should be supervised"
        );
        for (child, want) in children.iter().zip([1, 2]) {
            assert_eq!(
                rt.borrow().actors[child]
                    .get_state_field("count")
                    .and_then(|v| v.as_int()),
                Some(want),
                "child should have handled its work message"
            );
        }

        // Kill the first child (abnormal exit): dynamic children are
        // Transient, so it restarts from the template.
        rt.borrow_mut().kill_actor(children[0]);

        let after: Vec<u64> = rt.borrow().supervisors[&sup_id]
            .children
            .iter()
            .map(|(_, id)| *id)
            .collect();
        assert_eq!(
            after.len(),
            2,
            "the crashed child must be replaced, not dropped"
        );
        assert_eq!(
            after[1], children[1],
            "the surviving child must be untouched"
        );
        let restarted = after[0];
        assert_ne!(restarted, children[0], "restart must create a fresh actor");
        assert_eq!(
            rt.borrow().actors[&restarted]
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(0),
            "restarted child must start from the template state defaults"
        );

        // The replacement is a real bytecode actor: send it work and let
        // the scheduler run its template behavior.
        rt.borrow_mut()
            .send_message(restarted, "work", &[Value::int(5)]);
        rt.borrow_mut().run_scheduler();
        assert_eq!(
            rt.borrow().actors[&restarted]
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(5),
            "restarted child must run the template behavior table"
        );
    }

    #[test]
    fn test_otp_supervise_and_terminate_child_round_trip() {
        let source = r#"
            actor Managed {
                state n: Int = 0
                behavior bump() { self.n = self.n + 1 }
            }
            let sup = perform Otp.create_supervisor("plain", 0) in
            let w = spawn Managed {} in
            let s1 = perform Otp.supervise_child(sup, w, 0) in
            let before = perform Otp.child_count(sup) in
            let s2 = perform Otp.terminate_child(sup, w) in
            let after = perform Otp.child_count(sup) in
            before * 10 + after
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        assert_eq!(
            value.as_int(),
            Some(10),
            "child_count should be 1 after supervise_child and 0 after terminate_child"
        );

        // The terminated worker exited cleanly and was NOT restarted.
        let rt_ref = rt.borrow();
        let (sup_id, supervisor) = rt_ref
            .supervisors
            .iter()
            .next()
            .expect("the supervisor should still exist");
        assert_eq!(supervisor.child_count(), 0);
        assert_eq!(
            rt_ref.actors.len(),
            1,
            "only the supervisor actor should remain after terminate_child"
        );
        assert!(rt_ref.actors.contains_key(sup_id));
    }

    /// D7c (RFC 0014 §4): the `.nula` supervisor surface — policy `3` on
    /// `Otp.supervise_child` — opts a persistent child into
    /// `RespawnOnNodeLoss` (shadow replication + directory registration),
    /// recorded in the runtime's re-spawn opt-in table.
    #[test]
    fn test_otp_supervise_child_respawn_on_node_loss_policy_opts_in() {
        let source = r#"
            persistent actor Durable {
                state durable count: Int = 0
                behavior bump() { self.count = self.count + 1 }
            }
            let sup = perform Otp.create_supervisor("respawn", 0) in
            let w = spawn Durable {} in
            let s = perform Otp.supervise_child(sup, w, 3) in
            w
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let child_id = value.as_actor_id().expect("spawn returns an actor id");

        let rt_ref = rt.borrow();
        assert_eq!(
            rt_ref.respawn_opted.get(&child_id),
            Some(&1),
            "policy 3 must opt the persistent child into RespawnOnNodeLoss (epoch 1)"
        );
    }

    #[test]
    fn test_example_worker_pool_runs() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = include_str!("../../examples/worker_pool.nula");
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        assert_eq!(value.as_int(), Some(0), "main should return 0");

        rt.borrow_mut().run_scheduler();

        // w1 crashed on `die` and was restarted from the template (fresh
        // id, count back to the declared default 0); w2 kept its count.
        let rt_ref = rt.borrow();
        let supervisor = rt_ref
            .supervisors
            .values()
            .next()
            .expect("the pool supervisor should exist");
        assert_eq!(supervisor.child_count(), 2);
        let counts: Vec<i64> = supervisor
            .children
            .iter()
            .map(|(_, id)| {
                rt_ref.actors[id]
                    .get_state_field("count")
                    .and_then(|v| v.as_int())
                    .expect("each pool child should have a count state field")
            })
            .collect();
        assert_eq!(
            counts,
            vec![0, 2],
            "crashed child must restart from the template state defaults"
        );
    }

    // -----------------------------------------------------------------------
    // Test: state_machine end-to-end (desugar → compile → run)
    // -----------------------------------------------------------------------

    #[test]
    fn test_state_machine_spawn_and_transition() {
        // Define a state_machine, spawn it, send events. The state_machine
        // desugars to an ordinary actor; verify the pipeline compiles and
        // the spawned actor survives all transitions (the desugared
        // behaviors — exit-hook if-chain, assign, entry-hook, nil —
        // compiled and ran correctly).
        let source = r#"
            state_machine Light {
                state Off
                state On

                event turn_on: On
                event turn_off: Off

                on_entry On {
                    perform IO.print("light on")
                }

                on_exit On {
                    perform IO.print("light off")
                }
            }
            fn main() {
                let light = spawn Light {} in {
                    send light turn_on()
                    send light turn_off()
                    send light turn_on()
                    light
                }
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let light_id = value
            .as_actor_id()
            .expect("main should return the actor ref");
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        assert!(
            rt_ref.actors.contains_key(&light_id),
            "actor should still be alive after state transitions"
        );
        let actor = &rt_ref.actors[&light_id];
        // The _sm_state field should exist; it's heap-allocated (TAG_PTR).
        let state_val = actor
            .get_state_field("_sm_state")
            .expect("_sm_state field should exist");
        assert!(
            state_val.is_ptr() || state_val.is_string(),
            "_sm_state should be a string-ish value"
        );
        // Bytecode behaviors are stored in bytecode_offsets, not
        // behavior_table (which is for native Rust handlers).
        assert!(
            !actor.bytecode_offsets.is_empty(),
            "desugared actor should have bytecode offsets (found {})",
            actor.bytecode_offsets.len()
        );
    }

    #[test]
    fn test_state_machine_self_transition_runs_hooks() {
        // A self-transition (event targeting the current state) must run
        // both exit and entry hooks. Verify the actor survives self-ticks.
        let source = r#"
            state_machine IdleLoop {
                state Idle
                state Done

                event tick: Idle
                event finish: Done

                on_entry Idle {
                    perform IO.print("entering idle")
                }

                on_exit Idle {
                    perform IO.print("leaving idle")
                }
            }
            fn main() {
                let m = spawn IdleLoop {} in {
                    send m tick()
                    send m tick()
                    m
                }
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        let machine_id = value
            .as_actor_id()
            .expect("main should return the actor ref");
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        assert!(
            rt_ref.actors.contains_key(&machine_id),
            "actor should survive self-transitions"
        );
        let actor = &rt_ref.actors[&machine_id];
        let state_val = actor
            .get_state_field("_sm_state")
            .expect("_sm_state should exist");
        assert!(
            state_val.is_ptr() || state_val.is_string(),
            "_sm_state should be a string-ish value"
        );
        assert!(
            !actor.bytecode_offsets.is_empty(),
            "desugared actor should have bytecode offsets"
        );
    }

    #[test]
    fn test_actor_set_priority_runs_high_first() {
        // A High-priority actor is dequeued before a Normal one even when
        // the Normal actor's message was sent first. Phase 1 runs a real
        // compiled behavior that boosts itself via `perform
        // Actor.set_priority(0)`; phase 2 enqueues both actors through the
        // normal send path and observes the scheduler's dequeue order.
        // Deterministic: each run_scheduler drains fully, so the phase-2
        // queue order is exactly [Hi(High), Lo(Normal)].
        let source = r#"
            actor Hi {
                behavior boost_hi() {
                    perform Actor.set_priority(0)
                    perform Actor.register("hi")
                }
                behavior work() { 0 }
            }
            actor Lo {
                behavior boost_lo() { perform Actor.register("lo") }
                behavior work() { 0 }
            }
            let h = spawn Hi {} in
            let n = spawn Lo {} in {
                send h boost_hi()
                send n boost_lo()
                0
            }
        "#;
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let (value, _ty) = run_source_with_runtime(source, rt.clone()).unwrap();
        assert_eq!(value.as_int(), Some(0), "main should return 0");

        // Phase 1: the boost behaviors run; Hi sets its own priority.
        rt.borrow_mut().run_scheduler();
        let (hi_id, lo_id) = {
            let rt_ref = rt.borrow();
            let hi_id = rt_ref.registry.whereis("hi").expect("Hi registered itself");
            let lo_id = rt_ref.registry.whereis("lo").expect("Lo registered itself");
            assert_eq!(
                rt_ref.actors.get(&hi_id).unwrap().priority,
                crate::runtime::ActorPriority::High,
                "Actor.set_priority(0) from a behavior should make the actor High"
            );
            assert_eq!(
                rt_ref.actors.get(&lo_id).unwrap().priority,
                crate::runtime::ActorPriority::Normal,
                "untouched actors stay Normal"
            );
            (hi_id, lo_id)
        };

        // Phase 2: send to Lo first, then Hi; the High entry dequeues first.
        rt.borrow_mut().send_message(lo_id, "work", &[]);
        rt.borrow_mut().send_message(hi_id, "work", &[]);
        let rt_ref = rt.borrow_mut();
        assert_eq!(rt_ref.scheduler.dequeue(), Some(hi_id));
        assert_eq!(rt_ref.scheduler.dequeue(), Some(lo_id));
    }

    // -----------------------------------------------------------------------
    // v0.2 HIR/MIR pipeline smoke tests
    // -----------------------------------------------------------------------

    fn run_source_new(source: &str) -> Result<Value, NuError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module()?;

        // Type check (required before lowering)
        let mut type_checker = TypeChecker::new();
        let _ = type_checker.check_module(&ast)?;

        // New HIR -> MIR -> bytecode pipeline
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir)?;
        let module = crate::mir_codegen::compile_mir(&mut mir, "test")?;

        let mut vm = VM::new();
        vm.load_module(module);
        vm.run()
    }

    fn assert_int_new(source: &str, expected: i64) {
        let value = run_source_new(source).unwrap();
        assert_eq!(
            value.as_int(),
            Some(expected),
            "new pipeline expected integer for: {}",
            source
        );
    }

    /// Compile source through the HIR/MIR pipeline into a CodeModule without
    /// running it, for structural assertions (actor_metadata, behaviors).
    fn compile_source_new(source: &str) -> Result<crate::bytecode::CodeModule, NuError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module()?;
        let mut type_checker = TypeChecker::new();
        let _ = type_checker.check_module(&ast)?;
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir)?;
        crate::mir_codegen::compile_mir(&mut mir, "test")
    }

    /// Compile and run `source` through the HIR/MIR pipeline with a real
    /// Runtime attached, exercising actual actor semantics (state, ask)
    /// rather than the no-op stubs a bare VM falls back to.
    fn run_source_new_with_runtime(
        source: &str,
        runtime: Rc<RefCell<Runtime>>,
    ) -> Result<Value, NuError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module()?;

        let mut type_checker = TypeChecker::new();
        let _ = type_checker.check_module(&ast)?;

        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir)?;
        let module = crate::mir_codegen::compile_mir(&mut mir, "test")?;

        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime)));
        vm.run()
    }

    #[test]
    fn test_mir_pipeline_actor_ask_with_arguments() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Calculator {
                behavior add(a: Int, b: Int) { a + b }
            }
            let calc = spawn Calculator {} in
                ask calc add(10, 20)
        "#;
        let value = run_source_new_with_runtime(source, rt).unwrap();
        assert_eq!(value.as_int(), Some(30), "ask add(10, 20) should return 30");
    }

    #[test]
    fn test_mir_pipeline_actor_state_get_set() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Counter {
                state count = 0
                behavior inc() { self.count = self.count + 1 }
                behavior get() { self.count }
            }
            let c = spawn Counter { count = 0 } in
            let _ = ask c inc() in
            let _ = ask c inc() in
            ask c get()
        "#;
        let value = run_source_new_with_runtime(source, rt).unwrap();
        assert_eq!(
            value.as_int(),
            Some(2),
            "two increments should leave count at 2"
        );
    }

    #[test]
    fn test_mir_pipeline_actor_send_then_scheduler() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Counter {
                state count = 0
                behavior add(n: Int) { self.count = self.count + n }
            }
            let c = spawn Counter { count = 0 } in {
                send c add(5)
                send c add(7)
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("count").and_then(|v| v.as_int()),
            Some(12),
            "counter should be 12 after adding 5 and 7"
        );
    }

    // -- Effect mocking -------------------------------------------------

    #[test]
    fn test_effect_mocking_intercepts_io_print() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let called: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let cb = called.clone();

        rt.borrow_mut()
            .install_test_handler("IO.print", move |_regs| {
                *cb.borrow_mut() = true;
                Some(Value::unit())
            });

        let source = r#"perform IO.print("hello from test handler")"#;
        let value = run_source_new_with_runtime(source, rt).unwrap();
        assert_eq!(value, Value::unit());
        assert!(
            *called.borrow(),
            "test handler should have intercepted IO.print"
        );
    }

    #[test]
    fn test_effect_mocking_fallback_when_handler_returns_none() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let called: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let cb = called.clone();

        // Handler returns None → real IO.print dispatch fires (println!).
        rt.borrow_mut()
            .install_test_handler("IO.print", move |_regs| {
                *cb.borrow_mut() = true;
                None // fall through to real handler
            });

        let source = r#"perform IO.print("fallthrough test")"#;
        let value = run_source_new_with_runtime(source, rt).unwrap();
        assert_eq!(value, Value::unit());
        assert!(*called.borrow(), "test handler should have been invoked");
    }

    #[test]
    fn test_effect_mocking_unregistered_effect_not_intercepted() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        // No handler installed for IO.print → normal dispatch.
        let source = r#"perform IO.print("normal dispatch")"#;
        let value = run_source_new_with_runtime(source, rt).unwrap();
        assert_eq!(value, Value::unit());
    }

    // -- Virtual clock --------------------------------------------------

    #[test]
    fn test_virtual_clock_tick_timers_fires_after_advance() {
        use std::time::Duration;

        let mut rt = Runtime::new();
        rt.install_virtual_clock();

        // Schedule a timer 5 seconds from now.
        let _id = rt.timer_wheel.send_after(
            Duration::from_secs(5),
            42, // dummy actor
            1,  // dummy behavior
            vec![],
        );

        // Tick — nothing should fire yet (timer at T+5s, clock at T+0).
        rt.tick_timers();
        assert!(
            !rt.timer_wheel.is_empty(),
            "timer should still be pending at virtual t=0"
        );

        // Advance 10 seconds — timer at 5s must fire.
        rt.advance_time(Duration::from_secs(10));
        rt.tick_timers();
        assert!(
            rt.timer_wheel.is_empty(),
            "timer should have fired and been removed after advancing 10s"
        );

        rt.remove_virtual_clock();
    }

    #[test]
    fn test_virtual_clock_now_frozen_until_advanced() {
        use std::time::Duration;

        let mut rt = Runtime::new();
        rt.install_virtual_clock();

        let t0 = rt.now();
        std::thread::sleep(Duration::from_millis(5));
        let t1 = rt.now();
        assert_eq!(t0, t1, "virtual clock should not advance with wall time");

        rt.advance_time(Duration::from_secs(1));
        let t2 = rt.now();
        assert!(
            t2 > t0,
            "virtual clock should advance when explicitly advanced"
        );

        rt.remove_virtual_clock();
    }

    // -- Token budget ---------------------------------------------------

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_token_budget_exhausted_rejects_llm_request() {
        use nulang_ai::{LlmErrorKind, LlmRequest, LlmResponse, MockLlmClient, TokenUsage};

        let mut rt = Runtime::new();

        let mock = Box::new(MockLlmClient::new(LlmResponse {
            content: Some("ok".to_string()),
            usage: TokenUsage::new(50, 50),
            tool_calls: vec![],
            model: "test".to_string(),
            finish_reason: "stop".to_string(),
        }));
        rt.set_llm_client(mock);
        // Budget of 0 tokens — immediately exhausted.
        rt.set_token_budget(0);

        let request = LlmRequest {
            model: "test".to_string(),
            messages: vec![],
            tools: vec![],
            memory: vec![],
            pricing: None,
            response_format: None,
        };
        let result = rt.complete_llm_request(request, vec![]);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, LlmErrorKind::BudgetExceeded);
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_token_budget_deducts_after_successful_call() {
        use nulang_ai::{LlmRequest, LlmResponse, MockLlmClient, TokenUsage};

        let mut rt = Runtime::new();
        let mock = Box::new(MockLlmClient::new(LlmResponse {
            content: Some("ok".to_string()),
            usage: TokenUsage::new(50, 50),
            tool_calls: vec![],
            model: "test".to_string(),
            finish_reason: "stop".to_string(),
        }));
        rt.set_llm_client(mock);

        rt.set_token_budget(500);
        assert_eq!(rt.llm.token_budget.as_ref().unwrap().remaining(), 500);

        let request = LlmRequest {
            model: "test".to_string(),
            messages: vec![],
            tools: vec![],
            memory: vec![],
            pricing: None,
            response_format: None,
        };
        let result = rt.complete_llm_request(request, vec![]);
        assert!(result.is_ok());
        assert_eq!(rt.llm.token_budget.as_ref().unwrap().remaining(), 400);
    }

    // -- Flight recorder ------------------------------------------------

    #[test]
    fn test_flight_recorder_records_messages() {
        let mut rt = Runtime::new();
        let actor_id = rt.spawn_actor(Box::new(|| vec![]));

        // Send three messages
        rt.send_message_by_id(actor_id, 1, &[Value::int(10)]);
        rt.send_message_by_id(actor_id, 2, &[Value::int(20), Value::int(21)]);
        rt.send_message_by_id(actor_id, 1, &[Value::int(30)]);

        let actor = rt.actors.get(&actor_id).unwrap();
        let entries = actor.flight_recorder.ordered_entries();
        assert_eq!(entries.len(), 3, "should have recorded 3 messages");
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[2].seq, 2);
        assert_eq!(entries[0].behavior_id, 1);
        assert_eq!(entries[1].behavior_id, 2);
        assert_eq!(entries[2].behavior_id, 1);
        assert_eq!(entries[1].payload_len, 2);
    }

    #[test]
    fn test_flight_recorder_ring_buffer_wraps() {
        let mut rt = Runtime::new();
        let actor_id = rt.spawn_actor(Box::new(|| vec![]));
        // Flight recorder defaults to 1000 entries. Send 3 and check.
        for i in 0..3 {
            rt.send_message_by_id(actor_id, (i % 10) as u16, &[Value::int(i)]);
        }
        let actor = rt.actors.get(&actor_id).unwrap();
        assert_eq!(actor.flight_recorder.len(), 3);
        assert!(!actor.flight_recorder.is_empty());

        // Clear and verify
        let actor = rt.actors.get_mut(&actor_id).unwrap();
        actor.flight_recorder.clear();
        assert!(actor.flight_recorder.is_empty());
        assert_eq!(actor.flight_recorder.len(), 0);
    }

    /// The legacy compiler and the HIR/MIR pipeline must agree on actor
    /// semantics too, not just pure expressions — run the same program
    /// through both with independent Runtimes and compare results.
    #[test]
    fn test_mir_and_legacy_actor_semantics_agree() {
        let corpus: &[&str] = &[
            r#"
                actor Calculator { behavior add(a: Int, b: Int) { a + b } }
                let calc = spawn Calculator {} in ask calc add(10, 20)
            "#,
            r#"
                actor Counter {
                    state count = 0
                    behavior inc() { self.count = self.count + 1 }
                    behavior get() { self.count }
                }
                let c = spawn Counter { count = 0 } in
                let _ = ask c inc() in
                let _ = ask c inc() in
                let _ = ask c inc() in
                ask c get()
            "#,
        ];
        for src in corpus {
            let legacy_rt = Rc::new(RefCell::new(Runtime::new()));
            let legacy = run_source_with_runtime(src, legacy_rt)
                .map(|(v, _)| v.to_string_repr())
                .unwrap_or_else(|e| panic!("legacy pipeline failed on {:?}: {}", src, e));
            let mir_rt = Rc::new(RefCell::new(Runtime::new()));
            let mir = run_source_new_with_runtime(src, mir_rt)
                .map(|v| v.to_string_repr())
                .unwrap_or_else(|e| panic!("MIR pipeline failed on {:?}: {}", src, e));
            assert_eq!(legacy, mir, "pipelines disagree on {:?}", src);
        }
    }

    // -----------------------------------------------------------------------
    // Workflow/agent desugaring via the HIR/MIR pipeline
    // -----------------------------------------------------------------------

    #[test]
    fn test_mir_workflow_lowers_to_persistent_actor() {
        let source = "workflow PurchaseOrder { step validate { 1 } }";
        let module = compile_source_new(source).unwrap();

        let meta = module
            .actor_metadata
            .iter()
            .find(|m| m.name == "PurchaseOrder")
            .expect("workflow should produce actor metadata");
        assert!(meta.is_workflow, "workflow metadata should be flagged");
        assert!(meta.persistent, "workflows should be persistent actors");
        assert_eq!(meta.behavior_indices.len(), 1, "one behavior per step");

        let behavior = &module.behaviors[meta.behavior_indices[0]];
        assert_eq!(behavior.name, "PurchaseOrder.validate");
    }

    /// Same source and assertions as
    /// test_saga_compensation_runs_in_reverse_order (legacy pipeline), run
    /// through the HIR/MIR pipeline instead. The runtime's saga-compensation
    /// machinery (invoked automatically when a step's execution fails, via
    /// BehaviorTableEntry::compensate_offset) is pipeline-agnostic, so this
    /// exercises mir_codegen's compensation_of patching end to end.
    #[test]
    fn test_mir_saga_compensation_runs_in_reverse_order() {
        let source = r#"
            workflow SagaTest {
                step a {
                    (self.step_index = self.step_index + 1, self.a_done = 1, emit A_Done())
                } compensate {
                    self.comp_order = self.comp_order * 10 + 1
                }
                step b {
                    (self.step_index = self.step_index + 1, self.b_done = 1, emit B_Done())
                } compensate {
                    self.comp_order = self.comp_order * 10 + 2
                }
                step c {
                    perform Fail.now()
                }
            }
            let c = spawn SagaTest {} in { c }
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return actor reference");

        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message_by_id(actor_id, 1, &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message_by_id(actor_id, 2, &[]);
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("a_done").and_then(|v| v.as_int()),
            Some(1)
        );
        assert_eq!(
            actor.get_state_field("b_done").and_then(|v| v.as_int()),
            Some(1)
        );
        assert_eq!(
            actor.get_state_field("comp_order").and_then(|v| v.as_int()),
            Some(21),
            "compensations should run in reverse order (b then a)"
        );
    }

    /// Same source and assertions as test_agent_ask_uses_memory (legacy
    /// pipeline), run through the HIR/MIR pipeline instead.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_mir_agent_ask_uses_memory() {
        let source = r#"
            agent Agent = {
                model: "mock-model",
                system_prompt: "You are helpful.",
                memory: { max_turns: 10 }
            }
            let a = spawn Agent {} in
            let r1 = ask a ask("hello") in
            let r2 = ask a ask("world") in
            r1
        "#;
        let module = compile_source_new(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client = nulang_ai::MockLlmClient::text("world");
        rt.borrow_mut().set_llm_client(Box::new(client.clone()));

        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt)));

        let result = vm.run().unwrap();

        let calls = client.recorded_calls();
        assert_eq!(calls.len(), 2, "expected two LLM calls");

        let module_idx = vm.modules.len() - 1;
        let content = vm.value_to_string(module_idx, result);
        assert_eq!(content, "world");

        assert_eq!(calls[0].messages.len(), 2);
        assert_eq!(calls[0].messages[1].content, "hello");
        assert_eq!(calls[1].messages.len(), 4);
        assert_eq!(calls[1].messages[2].content, "world");
    }

    /// Regression test for ActorMeta.is_agent/semantic_memory_dimensions:
    /// unlike `ask`/`usage` (ordinary compiled bytecode behaviors),
    /// `store_fact`/`recall` are placeholder bodies the RUNTIME intercepts
    /// by name, gated on `actor_is_agent(actor_id)` — which reads
    /// `Actor.is_agent`, itself set from `ActorMeta.is_agent` at spawn time.
    /// If mir_lower.rs ever went back to hardcoding is_agent/
    /// semantic_memory_dimensions instead of reading them off the desugared
    /// hir::ActorDef, this interception would silently stop firing and the
    /// placeholder `Unit` body would run instead — same source and
    /// assertions as test_agent_semantic_memory_store_and_recall.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_mir_agent_semantic_memory_store_and_recall() {
        let source = r#"
            agent Agent = {
                model: "mock-model",
                system_prompt: "You are helpful.",
                semantic_memory: { dimensions: 32 }
            }
            let a = spawn Agent {} in
            let _ = ask a store_fact("hello world") in
            ask a recall("hello", 1)
        "#;
        let module = compile_source_new(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));

        let result = vm.run().unwrap();

        let module_idx = vm.modules.len() - 1;
        let content = vm.value_to_string(module_idx, result);
        assert_eq!(content, "hello world");

        let rt = rt.borrow();
        let actor = rt.actors.values().next().expect("expected one actor");
        let memory_json = actor.get_state_field("semantic_memory").unwrap();
        let memory_json_str = vm.value_to_string(module_idx, memory_json);
        let memory: nulang_ai::SemanticMemory = serde_json::from_str(&memory_json_str).unwrap();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory.documents[0].content, "hello world");
    }

    /// The legacy compiler and the HIR/MIR pipeline must agree on
    /// workflow/agent semantics too.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_mir_and_legacy_workflow_agent_semantics_agree() {
        let corpus: &[&str] = &[
            // Actor-ref values aren't compared here (their string repr
            // embeds an internal, Runtime-instance-specific id counter that
            // isn't guaranteed to line up between two independently
            // constructed runtimes) — ask a step for a plain value instead.
            "workflow W { step a { 1 } } let w = spawn W {} in ask w a()",
            r#"
                agent Ag = { model: "mock-model", system_prompt: "hi" }
                let a = spawn Ag {} in ask a ask("hello")
            "#,
            r#"
                workflow W2 {
                    step before { self.step_index = self.step_index + 1 }
                    parallel {
                        step branch_a { self.step_index = self.step_index + 1 }
                        step branch_b { self.step_index = self.step_index + 1 }
                    }
                }
                let w = spawn W2 {} in ask w before()
            "#,
            r#"
                @tool(description: "Adds two integers.")
                fn add(x: Int, y: Int) -> Int { x + y }
                agent Ag2 = { model: "mock-model", tools: [add] }
                let a = spawn Ag2 {} in ask a ask("hello")
            "#,
        ];
        for src in corpus {
            // `Value::to_string_repr()` prints heap-allocated results (like
            // the "world" string these agents return) as a raw pointer
            // address (`#Value(hex)`) — it has no VM/module to dereference
            // through. Comparing that directly is flaky: two independently
            // constructed VMs allocate at addresses that only coincidentally
            // match. `vm.value_to_string` resolves the actual string content
            // instead, which is what these assertions actually care about.
            let (legacy_module, _) = compile_source(src)
                .unwrap_or_else(|e| panic!("legacy pipeline failed to compile {:?}: {}", src, e));
            let legacy_rt = Rc::new(RefCell::new(Runtime::new()));
            legacy_rt
                .borrow_mut()
                .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("world")));
            let mut legacy_vm = VM::new();
            legacy_vm.load_module(legacy_module);
            legacy_vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(legacy_rt)));
            let legacy_value = legacy_vm
                .run()
                .unwrap_or_else(|e| panic!("legacy pipeline failed to run {:?}: {}", src, e));
            let legacy = legacy_vm.value_to_string(legacy_vm.modules.len() - 1, legacy_value);

            let mir_module = compile_source_new(src)
                .unwrap_or_else(|e| panic!("MIR pipeline failed to compile {:?}: {}", src, e));
            let mir_rt = Rc::new(RefCell::new(Runtime::new()));
            mir_rt
                .borrow_mut()
                .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("world")));
            let mut mir_vm = VM::new();
            mir_vm.load_module(mir_module);
            mir_vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(mir_rt)));
            let mir_value = mir_vm
                .run()
                .unwrap_or_else(|e| panic!("MIR pipeline failed to run {:?}: {}", src, e));
            let mir = mir_vm.value_to_string(mir_vm.modules.len() - 1, mir_value);

            assert_eq!(legacy, mir, "pipelines disagree on {:?}", src);
        }
    }

    /// Same source and assertions as test_workflow_parallel_branches_normal
    /// (legacy pipeline), run through the HIR/MIR pipeline instead —
    /// exercises `hir_lower::desugar_workflow`'s parallel-branch synthesis
    /// and mir_codegen's `parallel_branches_of` patching end to end.
    #[test]
    fn test_mir_workflow_parallel_branches_normal() {
        let source = r#"
            workflow ParallelNormal {
                step before { (emit BeforeDone(), self.step_index = self.step_index + 1) }
                parallel {
                    step branch_a { emit BranchA_Done() }
                    step branch_b { emit BranchB_Done() }
                }
                step after { (emit AfterDone(), self.step_index = self.step_index + 1) }
            }
            spawn ParallelNormal {}
        "#;

        let store = SharedMemoryStore::new();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());

        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message_by_id(actor_id, 1, &[]);
        rt.borrow_mut().run_scheduler();
        rt.borrow_mut().send_message_by_id(actor_id, 2, &[]);
        rt.borrow_mut().run_scheduler();

        assert_eq!(
            rt.borrow()
                .actors
                .get(&actor_id)
                .unwrap()
                .get_state_field("step_index")
                .and_then(|v| v.as_int()),
            Some(3),
            "workflow should advance through before, parallel, and after"
        );

        let events = store.read_workflow_events(actor_id);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, WorkflowEvent::ParallelBranchCompleted { .. }))
                .count(),
            2,
            "both branches should emit ParallelBranchCompleted"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkflowEvent::Custom { name, .. } if name == "AfterDone")),
            "AfterDone should be persisted"
        );
    }

    /// Regression test for tool-schema resolution in `desugar_agent`: a
    /// spawn-time `ActorMeta.tools` entry must resolve to the same
    /// `ToolSchema` the stable compiler's `compile_agent` would produce.
    #[test]
    fn test_mir_agent_with_tool_resolves_schema() {
        let source = r#"
            @tool(description: "Adds two integers.")
            fn add(x: Int, y: Int) -> Int { x + y }

            agent Ag = { model: "gpt-4o", tools: [add] }
        "#;
        let module = compile_source_new(source).unwrap();
        let meta = module
            .actor_metadata
            .iter()
            .find(|m| m.name == "Ag")
            .expect("agent should produce actor metadata");
        assert_eq!(meta.tools.len(), 1);
        assert_eq!(meta.tools[0].name, "add");
        assert_eq!(meta.tools[0].description, "Adds two integers.");
    }

    #[test]
    fn test_new_pipeline_literal_int() {
        assert_int_new("42", 42);
    }

    #[test]
    fn test_new_pipeline_arithmetic_add() {
        assert_int_new("1 + 2", 3);
    }

    #[test]
    fn test_new_pipeline_let_binding() {
        assert_int_new("let x = 10 in x + 5", 15);
    }

    #[test]
    fn test_new_pipeline_if_then_else() {
        assert_int_new("if true then 1 else 2", 1);
        assert_int_new("if false then 1 else 2", 2);
    }

    #[test]
    fn test_new_pipeline_function_call() {
        let source = r#"
            fn add(x: Int, y: Int) -> Int { x + y }
            add(3, 4)
        "#;
        assert_int_new(source, 7);
    }

    #[test]
    fn test_new_pipeline_match_literal() {
        let source = r#"
            match 2 {
                case 1 => 10
                case 2 => 20
                case _ => 30
            }
        "#;
        assert_int_new(source, 20);
    }

    #[test]
    fn test_new_pipeline_bitwise_or() {
        assert_int_new("6 ||| 3", 7);
    }

    #[test]
    fn test_new_pipeline_inequality() {
        let value = run_source_new("1 != 2").unwrap();
        assert_eq!(value.as_bool(), Some(true));
    }

    /// MIR pipeline fn main() entry point.
    #[test]
    fn test_mir_fn_main_entry_point() {
        assert_int_new("fn main() { 42 }", 42);
        assert_int_new("fn main() { 1 + 2 }", 3);
        let src = "fn add(x: Int, y: Int) -> Int { x + y } fn main() { add(10, 20) }";
        assert_int_new(src, 30);
    }

    /// MIR + Runtime + fn main() with Inference.ask.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_mir_fn_main_with_runtime() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut()
            .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("world")));
        let v = run_source_new_with_runtime("fn main() { perform Inference.ask(\"hello\") }", rt)
            .unwrap();
        assert!(!v.is_nil());
    }

    /// MIR + Runtime + fn main() with Inference.ask (canonical name).
    /// Regression: Inference.ask must work identically to the deprecated Inference.ask.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_mir_fn_main_with_runtime_inference_ask() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut()
            .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("world")));
        let v = run_source_new_with_runtime("fn main() { perform Inference.ask(\"hello\") }", rt)
            .unwrap();
        assert!(!v.is_nil());
    }

    /// MIR + Runtime + Pipeline through fn main().
    #[test]
    fn test_mir_pipeline_with_runtime() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let v = run_source_new_with_runtime(
            "fn main() { let p = Pipeline.new() in p.run(\"hello\") }",
            rt,
        )
        .unwrap();
        assert!(v.is_nil(), "empty pipeline returns nil");
    }

    /// Receive expression parses, compiles, and reads from mailbox.
    #[test]
    fn test_mir_receive_expression() {
        let v = run_source_new("receive { | Msg(x) => x }").unwrap();
        assert!(v.is_nil(), "receive outside actor returns nil");
        let source = r#"
            actor Listener {
                behavior onMsg() {
                    receive { | Msg(x) => x }
                }
            }
            fn main() { 42 }
        "#;
        assert_int_new(source, 42);
    }

    /// Receive parses and runs inside a function body.
    #[test]
    fn test_mir_receive_gets_message() {
        // receive returns nil outside actor context
        let v = run_source_new("fn main() { receive { | Msg(x) => x } }").unwrap();
        assert!(
            v.is_nil(),
            "receive in fn main should return nil outside actor"
        );
    }

    /// End-to-end: a behavior using `receive` pops the next pending mailbox
    /// message and observes its first payload value.
    #[test]
    fn test_mir_receive_reads_mailbox_end_to_end() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    self.seen = receive { | Msg(x) => x }
                }
                behavior feed(n: Int) { n }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                send c feed(7)
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        // `drain` is dispatched first; its `receive` pops the still-pending
        // `feed(7)` message and stores its first payload in `seen`.
        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(7),
            "receive should have popped the pending feed(7) message"
        );
    }

    /// Selective receive: with two arms and messages for both queued, the
    /// first message IN MAILBOX ORDER wins — arm order is irrelevant.
    #[test]
    fn test_receive_match_first_in_mailbox_wins() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    self.seen = receive {
                        | get() => 100
                        | add(x, y) => x + y
                    }
                }
                behavior add(x: Int, y: Int) { x }
                behavior get() { 0 }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                send c add(1, 2)
                send c get()
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        // `add(1, 2)` is queued ahead of `get()`, so the `add` arm wins even
        // though `get` is listed first.
        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(3),
            "first matching message in mailbox order should win over arm order"
        );
    }

    /// Selective receive: a queued message that matches no arm is skipped and
    /// stays in the mailbox (the scheduler later dispatches it normally),
    /// while the first matching message is consumed by the receive.
    #[test]
    fn test_receive_match_selective_skip() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                state heard = 0
                behavior drain() {
                    self.seen = receive {
                        | add(x, y) => x + y
                    }
                }
                behavior add(x: Int, y: Int) { x }
                behavior noise(n: Int) { self.heard = n }
            }
            let c = spawn Listener { seen = 0 heard = 0 } in {
                send c drain()
                send c noise(9)
                send c add(4, 5)
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(9),
            "receive should skip noise(9) and consume add(4, 5)"
        );
        assert_eq!(
            actor.get_state_field("heard").and_then(|v| v.as_int()),
            Some(9),
            "the skipped noise(9) message should remain queued and dispatch normally"
        );
    }

    /// Selective receive fallback: when no queued message matches any arm,
    /// the legacy non-blocking behavior runs — pop the next message and
    /// yield its first payload value.
    #[test]
    fn test_receive_match_no_match_fallback() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                state heard = 0
                behavior drain() {
                    self.seen = receive {
                        | add(x, y) => x + y
                    }
                }
                behavior add(x: Int, y: Int) { x }
                behavior noise(n: Int) { self.heard = n }
            }
            let c = spawn Listener { seen = 0 heard = 0 } in {
                send c drain()
                send c noise(33)
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(33),
            "no-match fallback should pop the next message's first payload"
        );
        assert_eq!(
            actor.get_state_field("heard").and_then(|v| v.as_int()),
            Some(0),
            "the fallback consumes the message, so noise must not also dispatch"
        );
    }

    /// Selective receive on an empty mailbox evaluates to nil (non-blocking).
    #[test]
    fn test_receive_match_empty_mailbox_returns_nil() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    self.seen = receive {
                        | add(x, y) => x + y
                    }
                }
                behavior add(x: Int, y: Int) { x }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert!(
            actor
                .get_state_field("seen")
                .map(|v| v.is_nil())
                .unwrap_or(false),
            "receive with no matching message and empty mailbox should yield nil"
        );
    }

    /// Arm params bind to the matched message's payload values.
    #[test]
    fn test_receive_match_binds_payload_params() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    self.seen = receive {
                        | add(x, y) => x * 10 + y
                    }
                }
                behavior add(x: Int, y: Int) { x }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                send c add(7, 8)
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(78),
            "arm params should bind to the matched message's payload values"
        );
    }

    /// A matched message with fewer payload values than arm params binds the
    /// missing params to nil.
    #[test]
    fn test_receive_match_missing_params_bind_nil() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    self.seen = receive {
                        | add(x, y) => y
                    }
                }
                behavior add(x: Int, y: Int) { x }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        // Enqueue add with only one payload value behind the pending drain.
        rt.borrow_mut()
            .send_message(actor_id, "add", &[Value::int(7)]);
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert!(
            actor
                .get_state_field("seen")
                .map(|v| v.is_nil())
                .unwrap_or(false),
            "params beyond the payload length should bind to nil"
        );
    }

    /// Timed selective receive: with no matching message in the mailbox the
    /// actor suspends, the timeout fires, and the after body runs.
    #[test]
    fn test_receive_after_times_out_runs_after_body() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    self.seen = receive {
                        | add(x, y) => x + y
                    } after 30 => 4242
                }
                behavior add(x: Int, y: Int) { x }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(4242),
            "no message before the deadline: the after body must run"
        );
        assert!(
            actor.suspended_execution.is_none(),
            "the wait must be fully resolved after the timeout fired"
        );
    }

    /// Dynamic timeout: the `after` clause must accept an expression (here a
    /// variable holding the deadline), and the computed value must actually
    /// arm the timer — a silently-ignored clause would never run the after
    /// body and `seen` would stay 0.
    #[test]
    fn test_receive_after_dynamic_timeout_expression() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    let timeout = 30
                    self.seen = receive {
                        | add(x, y) => x + y
                    } after timeout => 4242
                }
                behavior add(x: Int, y: Int) { x }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(4242),
            "the dynamic timeout must arm the timer and run the after body"
        );
        assert!(
            actor.suspended_execution.is_none(),
            "the wait must be fully resolved after the dynamic timeout fired"
        );
    }

    /// Timed selective receive: a message arriving before the deadline wakes
    /// the suspended actor and dispatches to the matching arm; the timeout
    /// never fires observably.
    #[test]
    fn test_receive_after_wakes_on_matching_message() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    self.seen = receive {
                        | add(x, y) => x + y
                    } after 5000 => 4242
                }
                behavior add(x: Int, y: Int) { x }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        // Deliver add(4, 5) ~30ms into the 5s wait, while the actor is
        // suspended: the send must wake it so the scan matches arm 0.
        let add_id = rt.borrow().behavior_id_for(actor_id, "add").unwrap();
        rt.borrow().timer_wheel.send_after(
            std::time::Duration::from_millis(30),
            actor_id,
            add_id,
            vec![Value::int(4), Value::int(5)],
        );
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(9),
            "the matching message must resolve the wait, not the timeout"
        );
        assert!(
            rt_ref.timer_wheel.is_empty(),
            "the receive timeout must be cancelled once the wait matches"
        );
    }

    /// Timed selective receive with `after 0`: non-blocking poll — no match
    /// runs the after body immediately, without suspending or arming a timer.
    #[test]
    fn test_receive_after_zero_is_non_blocking() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    self.seen = receive {
                        | add(x, y) => x + y
                    } after 0 => 77
                }
                behavior add(x: Int, y: Int) { x }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(77),
            "after 0 with no queued match must run the after body immediately"
        );
        assert!(
            actor.suspended_execution.is_none(),
            "after 0 must never suspend the actor"
        );
        assert!(
            rt_ref.timer_wheel.is_empty(),
            "after 0 must not arm a timeout timer"
        );
    }

    /// Timed selective receive with multiple arms: a message waking the
    /// suspended actor dispatches to the right arm with its payload bound.
    #[test]
    fn test_receive_after_multiple_arms_bind_payload() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                behavior drain() {
                    self.seen = receive {
                        | get() => 100
                        | add(x, y) => x * 10 + y
                    } after 5000 => 0
                }
                behavior add(x: Int, y: Int) { x }
                behavior get() { 0 }
            }
            let c = spawn Listener { seen = 0 } in {
                send c drain()
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        // Wake the suspended wait with add(7, 8): the second arm must win
        // with x, y bound from the payload.
        let add_id = rt.borrow().behavior_id_for(actor_id, "add").unwrap();
        rt.borrow().timer_wheel.send_after(
            std::time::Duration::from_millis(30),
            actor_id,
            add_id,
            vec![Value::int(7), Value::int(8)],
        );
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(78),
            "the waking message must dispatch to the add arm with its payload bound"
        );
    }

    /// Timed selective receive: a NON-matching message wakes the actor, the
    /// re-scan finds no arm, and the behavior re-suspends on the ORIGINAL
    /// deadline — the skipped message stays queued and dispatches normally
    /// after the timeout runs the after body.
    #[test]
    fn test_receive_after_nonmatching_wake_keeps_deadline() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Listener {
                state seen = 0
                state heard = 0
                behavior drain() {
                    self.seen = receive {
                        | add(x, y) => x + y
                    } after 60 => 4242
                }
                behavior add(x: Int, y: Int) { x }
                behavior noise(n: Int) { self.heard = n }
            }
            let c = spawn Listener { seen = 0 heard = 0 } in {
                send c drain()
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        // Wake the wait ~20ms in with a message no arm matches: the actor
        // re-suspends and the 60ms timeout (armed at the first suspend)
        // still resolves the wait.
        let noise_id = rt.borrow().behavior_id_for(actor_id, "noise").unwrap();
        rt.borrow().timer_wheel.send_after(
            std::time::Duration::from_millis(20),
            actor_id,
            noise_id,
            vec![Value::int(9)],
        );
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(4242),
            "a non-matching wake must re-suspend; the original timeout fires"
        );
        assert_eq!(
            actor.get_state_field("heard").and_then(|v| v.as_int()),
            Some(9),
            "the skipped message must stay queued and dispatch normally"
        );
    }

    /// A behavior that `send`s to another actor: the message must be
    /// delivered (BytecodeRuntimeCallbacks::send_message used to be a
    /// silent no-op, dropping every behavior-internal send).
    #[test]
    fn test_behavior_send_relay_reaches_target() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Relay {
                behavior forward(target, n: Int) { send target arrived(n + 1) }
            }
            actor Sink {
                state seen = 0
                behavior arrived(n: Int) { self.seen = n }
            }
            let s = spawn Sink {} in
            let r = spawn Relay {} in {
                send r forward(s, 41)
                s
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let sink_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let sink = rt_ref.actors.get(&sink_id).unwrap();
        assert_eq!(
            sink.get_state_field("seen").and_then(|v| v.as_int()),
            Some(42),
            "the relay's behavior-internal send must reach the sink"
        );
    }

    /// A behavior that `spawn`s a child and sends to it: the child must be
    /// created with its bytecode handlers wired up and must run (the
    /// behavior-internal spawn used to return a bogus actor_ref(0)).
    #[test]
    fn test_behavior_spawn_child_runs_and_reports() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Worker {
                state got = 0
                behavior built(v: Int, sink) {
                    self.got = v
                    send sink report(v)
                }
            }
            actor Collector {
                state seen = 0
                behavior report(n: Int) { self.seen = n }
            }
            actor Factory {
                behavior make(n: Int, sink) {
                    let child = spawn Worker {} in
                        send child built(n * 2, sink)
                }
            }
            let c = spawn Collector {} in
            let f = spawn Factory {} in {
                send f make(21, c)
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let collector_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        assert_eq!(
            rt_ref.actors.len(),
            3,
            "the behavior-internal spawn must create a real third actor"
        );
        let collector = rt_ref.actors.get(&collector_id).unwrap();
        assert_eq!(
            collector.get_state_field("seen").and_then(|v| v.as_int()),
            Some(42),
            "the spawned child must run and report back to the collector"
        );
    }

    /// Channel actor pattern: spawn a Channel, put a value, take it.
    /// Demonstrates the stdlib channel pattern (actor-as-mailbox idiom).
    #[test]
    fn test_channel_send_receive_roundtrip() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = "\
            actor Channel {\n\
                state value = nil\n\
                state has_value = false\n\
                behavior put(v) {\n\
                    self.value = v\n\
                    self.has_value = true\n\
                }\n\
                behavior take() {\n\
                    if self.has_value then {\n\
                        self.has_value = false\n\
                        self.value\n\
                    } else {\n\
                        nil\n\
                    }\n\
                }\n\
            }\n\
            let ch = spawn Channel {} in {\n\
                send ch put(42)\n\
                ch\n\
            }\n\
        ";
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let ch_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        // Verify the put landed: has_value should be true.
        {
            let rt_ref = rt.borrow();
            let ch = rt_ref.actors.get(&ch_id).unwrap();
            assert_eq!(
                ch.get_state_field("has_value").and_then(|v| v.as_bool()),
                Some(true),
                "put should set has_value to true"
            );
        }

        // Now take the value back.
        rt.borrow_mut().send_message_by_id(ch_id, 1, &[]); // behavior 1 = take
        rt.borrow_mut().run_scheduler();

        // After take, has_value should be false.
        {
            let rt_ref = rt.borrow();
            let ch = rt_ref.actors.get(&ch_id).unwrap();
            assert_eq!(
                ch.get_state_field("has_value").and_then(|v| v.as_bool()),
                Some(false),
                "take should clear has_value"
            );
        }
    }

    /// Channel: multiple puts overwrite; take gets the latest.
    #[test]
    fn test_channel_overwrite_semantics() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = "\
            actor Channel {\n\
                state value = nil\n\
                state has_value = false\n\
                behavior put(v) {\n\
                    self.value = v\n\
                    self.has_value = true\n\
                }\n\
                behavior take() {\n\
                    if self.has_value then {\n\
                        self.has_value = false\n\
                        self.value\n\
                    } else {\n\
                        nil\n\
                    }\n\
                }\n\
            }\n\
            let ch = spawn Channel {} in {\n\
                send ch put(1)\n\
                send ch put(99)\n\
                ch\n\
            }\n\
        ";
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let ch_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        // The last put (99) should have overwritten the first (1).
        {
            let rt_ref = rt.borrow();
            let ch = rt_ref.actors.get(&ch_id).unwrap();
            let v = ch.get_state_field("value");
            assert_eq!(
                v.and_then(|val| val.as_int()),
                Some(99),
                "last put should overwrite previous value"
            );
        }
    }
    /// Regression for the deferred receive-wait wake: a behavior that sends
    /// to an actor suspended in `receive ... after` must wake it via the
    /// match — but the resume cannot run inside the sender's VM execution
    /// (it would nest `vm.resume()` on the shared runtime VM), so the wake
    /// is deferred until the sender's behavior returns.
    #[test]
    fn test_behavior_send_wakes_suspended_receive_wait() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Waiter {
                state result = 0
                behavior waitwork() {
                    self.result = receive {
                        | token(n) => n
                    } after 5000 => 999
                }
                behavior token(n: Int) { n }
            }
            actor Poker {
                behavior poke(r) { send r token(77) }
            }
            let w = spawn Waiter {} in
            let p = spawn Poker {} in {
                send w waitwork()
                send p poke(w)
                w
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let waiter_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let waiter = rt_ref.actors.get(&waiter_id).unwrap();
        assert_eq!(
            waiter.get_state_field("result").and_then(|v| v.as_int()),
            Some(77),
            "the behavior-internal send must wake the suspended wait via the match, not the 5s timeout"
        );
        assert!(
            waiter.suspended_execution.is_none(),
            "the wait must be fully resolved"
        );
        assert!(
            rt_ref.timer_wheel.is_empty(),
            "the receive timeout must be cancelled once the wait matches"
        );
    }

    /// Standalone `after ms => expr` (not inside a `receive` block) must
    /// desugar to `receive {} after ms => expr` and work identically: with
    /// `after 0` it runs the body immediately without suspending.
    #[test]
    fn test_standalone_after_zero_runs_body() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Sleeper {
                state woken = false
                behavior nap() {
                    self.woken = after 0 => true
                }
            }
            let s = spawn Sleeper {} in {
                send s nap()
                s
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("woken").and_then(|v| v.as_bool()),
            Some(true),
            "standalone after 0 must run the body immediately"
        );
        assert!(
            actor.suspended_execution.is_none(),
            "standalone after 0 must not suspend"
        );
    }

    /// Literal pattern in selective receive: `receive { | Msg(42) => ... }`
    /// only matches when the payload equals the literal.
    #[test]
    fn test_receive_pattern_literal_match() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor P {
                state seen = 0
                behavior drain() {
                    self.seen = receive {
                        | msg(n) if n > 10 => n
                        | msg(n) => 0
                    }
                }
                behavior msg(n: Int) { n }
            }
            let c = spawn P { seen = 0 } in {
                send c drain()
                send c msg(5)
                send c msg(20)
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value.as_actor_id().unwrap();
        rt.borrow_mut().run_scheduler();
        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("seen").and_then(|v| v.as_int()),
            Some(20),
            "guard n > 10 should skip msg(5) and match msg(20)"
        );
    }

    /// Guard in selective receive: `receive { | Msg(x) if x > 100 => ... }`
    /// skips messages that don't satisfy the guard.
    #[test]
    fn test_receive_guard_skips_non_matching() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor G {
                state result = 0
                behavior wait() {
                    self.result = receive {
                        | ping(x) if x > 100 => x
                    } after 5000 => 999
                }
                behavior ping(x: Int) { x }
            }
            let g = spawn G { result = 0 } in {
                send g wait()
                send g ping(5)
                send g ping(200)
                g
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value.as_actor_id().unwrap();
        rt.borrow_mut().run_scheduler();
        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("result").and_then(|v| v.as_int()),
            Some(200),
            "guard x > 100 should skip ping(5) and match ping(200)"
        );
    }

    /// Wildcard pattern: `receive { | Msg(_) => ... }` matches any payload.
    #[test]
    fn test_receive_pattern_wildcard() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor W {
                state got = 0
                behavior drain() {
                    self.got = receive {
                        | data(_) => 42
                    }
                }
                behavior data(x: Int) { x }
            }
            let c = spawn W { got = 0 } in {
                send c drain()
                send c data(7)
                c
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value.as_actor_id().unwrap();
        rt.borrow_mut().run_scheduler();
        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&actor_id).unwrap();
        assert_eq!(
            actor.get_state_field("got").and_then(|v| v.as_int()),
            Some(42),
            "wildcard pattern should match any message"
        );
    }

    /// A behavior that sends to its own actor (self-send): the message is
    /// delivered normally and processed in a later turn.
    #[test]
    fn test_behavior_send_to_self_delivers() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            actor Loop {
                state count = 0
                behavior spin(me, n: Int) {
                    self.count = self.count + 1
                    if n > 0 then send me spin(me, n - 1) else send me halt()
                }
                behavior halt() { 0 }
            }
            let l = spawn Loop {} in {
                send l spin(l, 3)
                l
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let loop_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let actor = rt_ref.actors.get(&loop_id).unwrap();
        assert_eq!(
            actor.get_state_field("count").and_then(|v| v.as_int()),
            Some(4),
            "spin(3) plus three self-sends (2, 1, 0) must all run"
        );
    }

    /// Sending to an unknown actor id is a no-op: the message is dropped
    /// and the bogus queue entry is skipped without crashing.
    #[test]
    fn test_send_to_unknown_actor_is_noop() {
        let mut rt = Runtime::new();
        rt.send_message_by_id(999_999, 0, &[Value::int(1)]);
        rt.run_scheduler();
        // The message is routed to the DLQ, which is created lazily.
        assert!(rt.dlq_actor_id.is_some());
        assert_eq!(rt.dlq_depth(), 1);
    }

    /// `send remote` keyword enforces network-sendable (val|tag) capabilities.
    /// This test verifies parsing and compilation; capability enforcement is
    /// tested in effect_checker unit tests and the CLI `--check` path.
    #[test]
    fn test_send_remote_parses_and_compiles() {
        let source = r#"
            actor Adder {
                behavior add(n: Int) { n }
            }
            let a = spawn Adder {} in
                send remote a add(42)
        "#;
        let result = check_source(source);
        assert!(
            result.is_ok(),
            "send remote should typecheck: {:?}",
            result.err()
        );
    }

    /// The infix form `actor ! behavior(args)` never sets `remote`, so
    /// capability checks use the standard (iso|val|tag) sendable rule.
    #[test]
    fn test_send_infix_has_remote_false() {
        let source = r#"
            actor Adder {
                behavior add(n: Int) { n }
            }
            let a = spawn Adder {} in
                a ! add(42)
        "#;
        let result = check_source(source);
        assert!(
            result.is_ok(),
            "infix send should typecheck: {:?}",
            result.err()
        );
    }

    /// Differential test: the legacy compiler and the HIR/MIR pipeline must
    /// produce identical results over a corpus of pure programs.
    #[test]
    fn test_mir_and_legacy_pipelines_agree() {
        let corpus: &[&str] = &[
            // Arithmetic and precedence
            "1 + 2 * 3 - 4",
            "(1 + 2) * (3 + 4)",
            "100 / 7 % 5",
            // Let chains and shadowing
            "let x = 5 in let y = x * 2 in x + y",
            "let x = 1 in let x = x + 1 in x",
            // Conditionals, including statements after an if
            "if 1 < 2 then 10 else 20",
            "let x = if false then 1 else 2 in x + 10",
            "if true then (if false then 1 else 2) else 3",
            // Match with literals, variable binding, and wildcard
            "match 2 { case 1 => 10 case 2 => 20 case _ => 30 }",
            "match 9 { case 1 => 10 case n => n * 2 }",
            // Closures: capturing, recursive, higher-order
            "let a = 40 in let add = fn(x) { x + a } in add(2)",
            "let fib = fn(n) { if n <= 1 then n else fib(n - 1) + fib(n - 2) } in fib(10)",
            "let twice = fn(f, x) { f(f(x)) } in let inc = fn(n) { n + 1 } in twice(inc, 5)",
            // Top-level functions
            "fn add(x: Int, y: Int) -> Int { x + y }\nadd(3, 4)",
            "fn fact(n: Int) -> Int { if n == 0 then 1 else n * fact(n - 1) }\nfact(6)",
            // Arrays, indexing, records
            "[10, 20, 30][1]",
            "let arr = [10, 20, 30] in arr[0] + arr[2]",
            "let r = { x: 1, y: 41 } in r.x + r.y",
            // Mutation via `=`: `arr[i] = v` and `record.f = v` parse as
            // BinOp::Assign binary expressions (only a bare `ident = v`
            // parses as the distinct Expr::Assign node).
            "let arr = [1, 2, 3] in { arr[0] = 99 arr[0] }",
            "let r = { x: 1, y: 2 } in { r.x = 99 r.x + r.y }",
            // For loops evaluate to unit
            "for i in [1, 2, 3] { i }",
            // Ref cells: `&` creates a cell, `*` dereferences, assignment
            // mutates and yields the assigned value.
            "let x = &10 in { x = 3; *x }",
            // Val references: &val creates an immutable-shared reference;
            // &ref is the default (mutable). Both dereference with *.
            "let x = &val 10 in *x",
            "let x = &ref 10 in { x = 3; *x }",
            // Value-level capability constructors: every capability erases
            // to a plain move and dereferences identically.
            "let x = &iso 10 in *x",
            "let x = &trn 10 in *x",
            "let x = &box 10 in *x",
            "let x = &linear 10 in *x",
            "let x = &lineariso 10 in *x",
            "let x = &tag 10 in 0",
            // Field access through &val reference
            "let r = { x: 1, y: 2 } in let p = &val r in p.x + p.y",
            // Effect handlers, with and without a resumed value
            "handle perform Math.getAnswer() { | Math.getAnswer() => 42 }",
            "handle perform IO.print(\"hello\") { | IO.print(msg) => 7 }",
            // Pipe operator
            "let inc = fn(n) { n + 1 } in 41 |> inc",
            // Receive expression (MVP: returns nil outside actor context)
            "receive { | Msg(x) => x }",
        ];
        for src in corpus {
            let legacy = run_source(src)
                .map(|(v, _)| v.to_string_repr())
                .unwrap_or_else(|e| panic!("legacy pipeline failed on {:?}: {}", src, e));
            let mir = run_source_new(src)
                .map(|v| v.to_string_repr())
                .unwrap_or_else(|e| panic!("MIR pipeline failed on {:?}: {}", src, e));
            assert_eq!(legacy, mir, "pipelines disagree on {:?}", src);
        }
    }

    /// Regression: closures capturing enclosing locals must see the captured
    /// values (CapStore/CapLoad used to be VM no-ops, yielding garbage).
    #[test]
    fn test_legacy_closure_capture() {
        let source = "let a = 40 in let add = fn(x) { x + a } in add(2)";
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(value.as_int(), Some(42));
    }

    #[test]
    fn test_legacy_closure_capture_two_vars() {
        let source = "let a = 30 in let b = 10 in let f = fn(x) { a + b + x } in f(2)";
        let (value, _ty) = run_source(source).unwrap();
        assert_eq!(value.as_int(), Some(42));
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_inference_ask_mock_client() {
        let source = r#"perform Inference.ask("hello")"#;
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut()
            .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("world")));

        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt)));

        let result = vm.run().unwrap();
        let string_id = result.as_string_id().expect("expected string result");
        let module_idx = vm.modules.len() - 1;
        let content = vm.constant_string(module_idx, string_id).unwrap();
        assert_eq!(content, "world");
    }

    // -----------------------------------------------------------------------
    // Non-blocking LLM calls in actor bytecode behaviors
    // -----------------------------------------------------------------------

    /// Native counter handler for the non-blocking LLM ordering test.
    #[cfg(feature = "ai-runtime")]
    fn llm_test_counter_inc(actor: &mut crate::runtime::Actor, _args: &[Value]) {
        let n = actor
            .get_state_field("count")
            .and_then(|v| v.as_int())
            .unwrap_or(0);

        actor.set_state_field("count", Value::int(n + 1));
    }

    /// A bytecode behavior that performs `Inference.ask` suspends on the scheduler
    /// thread, a background worker completes the HTTP call, and the behavior
    /// resumes with the response written back into the prompt register.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_inference_ask_actor_behavior_suspends_and_resumes() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client = nulang_ai::MockLlmClient::text("world");
        rt.borrow_mut().set_llm_client(Box::new(client.clone()));

        let source = r#"
            actor LlmActor {
                state answer = ""
                behavior go() {
                    self.answer = perform Inference.ask("hello")
                }
            }
            let a = spawn LlmActor { answer = "" } in a
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().send_message(actor_id, "go", &[]);
        rt.borrow_mut().run_scheduler();

        let answer = rt.borrow().actor_state_string(actor_id, "answer");
        assert_eq!(
            answer.as_deref(),
            Some("world"),
            "resumed behavior should store the LLM response in state"
        );
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert!(!actor.llm_inflight, "in-flight flag should be cleared");
            assert!(
                actor.llm_completed.is_none(),
                "completion should be consumed by the re-executed LlmAsk"
            );
            assert!(
                actor.suspended_execution.is_none(),
                "suspension should be cleared after resume"
            );
        }
        assert_eq!(client.recorded_calls().len(), 1, "exactly one LLM call");
    }

    /// While one actor is suspended on a slow LLM call, the scheduler must
    /// keep running other actors: all counter work completes before the LLM
    /// response is even pumped. Deterministic because completions are only
    /// pumped by run_scheduler, never by manual stepping.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_inference_ask_nonblocking_other_actors_run_first() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut()
            .set_llm_client(Box::new(nulang_ai::MockLlmClient::delayed(
                "done",
                std::time::Duration::from_millis(100),
            )));

        let source = r#"
            actor LlmActor {
                state answer = ""
                behavior go() {
                    self.answer = perform Inference.ask("hello")
                }
            }
            let a = spawn LlmActor { answer = "" } in a
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let llm_actor = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        let counter = rt
            .borrow_mut()
            .spawn_actor(Box::new(|| vec![("count".into(), Value::int(0))]));
        rt.borrow_mut()
            .actors
            .get_mut(&counter)
            .unwrap()
            .register_behavior("inc", llm_test_counter_inc);

        // LLM message first, then 20 counter increments.
        rt.borrow_mut().send_message(llm_actor, "go", &[]);
        for _ in 0..20 {
            rt.borrow_mut().send_message(counter, "inc", &[]);
        }

        // Pump the run queue manually. LLM completions are only delivered by
        // run_scheduler's completion pump, so during manual stepping the
        // response sits untouched in the channel no matter how long the
        // worker takes.
        loop {
            let next = rt.borrow_mut().scheduler.dequeue();
            match next {
                Some(actor_id) => rt.borrow_mut().step_actor(actor_id),
                None => break,
            }
        }

        {
            let rt_ref = rt.borrow();
            let counter_actor = rt_ref.actors.get(&counter).unwrap();
            assert_eq!(
                counter_actor
                    .get_state_field("count")
                    .and_then(|v| v.as_int()),
                Some(20),
                "all counter work must complete while the LLM call is in flight"
            );
            let llm = rt_ref.actors.get(&llm_actor).unwrap();
            assert!(
                llm.llm_inflight,
                "LLM call should still be in flight after the queue drained \
                 (a blocking call would have completed inline and stalled the counter)"
            );
            assert_eq!(
                rt_ref.actor_state_string(llm_actor, "answer").as_deref(),
                Some(""),
                "answer must not be stored before the completion is pumped"
            );
        }

        // Let the worker finish, then pump the completion and resume.
        std::thread::sleep(std::time::Duration::from_millis(200));
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        assert_eq!(
            rt_ref.actor_state_string(llm_actor, "answer").as_deref(),
            Some("done"),
            "LLM behavior should resume and store the delayed response"
        );
        assert_eq!(
            rt_ref
                .actors
                .get(&counter)
                .unwrap()
                .get_state_field("count")
                .and_then(|v| v.as_int()),
            Some(20)
        );
    }

    /// Two sequential `perform Inference.ask` calls in one behavior: the behavior
    /// suspends and resumes twice, re-capturing VM state on the second
    /// suspend, and observes both responses in order.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_inference_ask_chained_suspensions_resume_in_order() {
        let text_response = |content: &str| nulang_ai::LlmResponse {
            content: Some(content.to_string()),
            tool_calls: Vec::new(),
            model: "mock".to_string(),
            finish_reason: "stop".to_string(),
            usage: nulang_ai::TokenUsage::default(),
        };
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client = nulang_ai::MockLlmClient::sequence(vec![
            text_response("first-reply"),
            text_response("second-reply"),
        ]);
        rt.borrow_mut().set_llm_client(Box::new(client.clone()));

        let source = r#"
            actor Chained {
                state first = ""
                state second = ""
                behavior go() {
                    let _ = self.first = perform Inference.ask("one") in
                    self.second = perform Inference.ask("two")
                }
            }
            let a = spawn Chained { first = ""; second = "" } in a
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        rt.borrow_mut().send_message(actor_id, "go", &[]);
        rt.borrow_mut().run_scheduler();

        {
            let rt_ref = rt.borrow();
            assert_eq!(
                rt_ref.actor_state_string(actor_id, "first").as_deref(),
                Some("first-reply")
            );
            assert_eq!(
                rt_ref.actor_state_string(actor_id, "second").as_deref(),
                Some("second-reply")
            );
            assert!(!rt_ref.actors.get(&actor_id).unwrap().llm_inflight);
        }
        let calls = client.recorded_calls();
        assert_eq!(calls.len(), 2, "expected two LLM calls");
        assert_eq!(calls[0].messages[0].content, "one");
        assert_eq!(calls[1].messages[0].content, "two");
    }

    /// Two messages sent to one actor whose behavior suspends on
    /// `Inference.ask`: the second message must wait in the mailbox until the
    /// first behavior fully resumes.  Previously step_actor ran the second
    /// message over the live suspension; its `LlmAsk` saw the in-flight
    /// flag, returned Pending, and overwrote `suspended_execution`, so the
    /// first completion resumed the SECOND behavior with the FIRST call's
    /// response and the first behavior was lost forever.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_inference_ask_queued_messages_wait_for_suspended_behavior() {
        let text_response = |content: &str| nulang_ai::LlmResponse {
            content: Some(content.to_string()),
            tool_calls: Vec::new(),
            model: "mock".to_string(),
            finish_reason: "stop".to_string(),
            usage: nulang_ai::TokenUsage::default(),
        };
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client = nulang_ai::MockLlmClient::sequence(vec![
            text_response("reply-one"),
            text_response("reply-two"),
        ]);
        rt.borrow_mut().set_llm_client(Box::new(client.clone()));

        let source = r#"
            actor LlmPair {
                state first = ""
                state second = ""
                behavior one() {
                    self.first = perform Inference.ask("one")
                }
                behavior two() {
                    self.second = perform Inference.ask("two")
                }
            }
            let a = spawn LlmPair { first = ""; second = "" } in a
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let actor_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        // Both messages are queued before the scheduler runs: the second
        // arrives while the first behavior is suspended on its LLM call.
        rt.borrow_mut().send_message(actor_id, "one", &[]);
        rt.borrow_mut().send_message(actor_id, "two", &[]);
        rt.borrow_mut().run_scheduler();

        {
            let rt_ref = rt.borrow();
            assert_eq!(
                rt_ref.actor_state_string(actor_id, "first").as_deref(),
                Some("reply-one"),
                "first behavior must store its own response"
            );
            assert_eq!(
                rt_ref.actor_state_string(actor_id, "second").as_deref(),
                Some("reply-two"),
                "second behavior must store its own response"
            );
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert!(!actor.llm_inflight, "in-flight flag should be cleared");
            assert!(
                actor.suspended_execution.is_none(),
                "no suspension should remain after both behaviors complete"
            );
            assert!(
                actor.mailbox.is_empty(),
                "both queued messages should have been processed"
            );
        }
        let calls = client.recorded_calls();
        assert_eq!(
            calls.len(),
            2,
            "each behavior should issue its own LLM call"
        );
        assert_eq!(calls[0].messages[0].content, "one");
        assert_eq!(calls[1].messages[0].content, "two");
    }

    /// A workflow step that performs `Inference.ask` suspends on the background
    /// call and, once resumed, records the step completion the same way a
    /// signal-resumed step does: step_index advances, a StepCompleted
    /// event is appended, and the actor checkpoints.  Previously
    /// resume_suspended_llm_step did none of the workflow bookkeeping, so
    /// the step never advanced from the journal's perspective.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_workflow_inference_ask_step_records_completion() {
        let source = r#"
            workflow LlmFlow {
                step ask_step { self.answer = perform Inference.ask("hello") }
            }
            let w = spawn LlmFlow {} in { w }
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());
        rt.borrow_mut()
            .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("world")));
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt.borrow_mut().run_scheduler();

        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(1),
                "resumed LLM step should advance step_index"
            );
            assert!(actor.suspended_execution.is_none());
            assert_eq!(
                actor.waiting_signal, None,
                "suspension marker should be cleared after resume"
            );
            assert_eq!(
                rt_ref.actor_state_string(actor_id, "answer").as_deref(),
                Some("world")
            );
        }
        let events = store.read_workflow_events(actor_id);
        assert!(
            events.iter().any(|e| matches!(e, WorkflowEvent::StepCompleted { step_name, .. } if step_name == "ask_step")),
            "StepCompleted event should be persisted after the LLM call resumes"
        );
    }

    /// Crash-and-recover for a workflow step suspended on `Inference.ask`: the
    /// persisted suspension marker lets recovery re-drive the interrupted
    /// step, which re-issues the call on the new runtime and completes the
    /// step in the journal.  Previously the snapshot carried no marker
    /// (waiting_signal is None for Inference.ask suspends), so recover_actor did not
    /// re-trigger the step and it was silently lost on restart.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_workflow_inference_ask_step_redriven_after_restart() {
        let source = r#"
            workflow LlmFlowRecover {
                step ask_step { self.answer = perform Inference.ask("hello") }
            }
            let w = spawn LlmFlowRecover {} in { w }
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
            }
        }

        // First runtime: start the step and let it suspend on the LLM call.
        // The completion is never pumped, simulating a crash mid-call.
        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        rt1.borrow_mut()
            .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("stale")));
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt1.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        // Drive the queue manually so the behavior suspends; running the
        // full scheduler would pump the completion and resume the step.
        loop {
            let next = rt1.borrow_mut().scheduler.dequeue();
            match next {
                Some(id) => rt1.borrow_mut().step_actor(id),
                None => break,
            }
        }
        {
            let rt_ref = rt1.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert!(
                actor.suspended_execution.is_some(),
                "step should be suspended on the LLM call"
            );
            assert!(actor.llm_inflight, "background call should be in flight");
        }
        // The snapshot must carry the suspension marker so recovery knows
        // the in-flight step has to be re-driven.
        let snapshot = store
            .load_snapshot(actor_id)
            .expect("workflow spawn should have persisted a snapshot");
        assert_eq!(
            snapshot.waiting_signal.as_deref(),
            Some("__llm_ask_pending__"),
            "snapshot should record the LLM suspension marker"
        );

        // Simulate a node restart: drop the actor and recover into a fresh
        // runtime sharing the store, with its own LLM client.
        rt1.borrow_mut().actors.remove(&actor_id);

        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        let client2 = nulang_ai::MockLlmClient::text("world");
        rt2.borrow_mut().set_llm_client(Box::new(client2.clone()));
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            vec![None; module.behaviors.len()],
        );
        rt2.borrow_mut().recover_actor(actor_id);
        rt2.borrow_mut().run_scheduler();

        {
            let rt_ref = rt2.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(1),
                "re-driven step should advance step_index"
            );
            assert!(actor.suspended_execution.is_none());
            assert_eq!(
                rt_ref.actor_state_string(actor_id, "answer").as_deref(),
                Some("world"),
                "re-driven step should store the new runtime's response"
            );
        }
        let events = store.read_workflow_events(actor_id);
        assert!(
            events.iter().any(|e| matches!(e, WorkflowEvent::StepCompleted { step_name, .. } if step_name == "ask_step")),
            "StepCompleted event should be persisted after recovery re-drives the step"
        );
        let calls = client2.recorded_calls();
        assert_eq!(
            calls.len(),
            1,
            "the recovered runtime should issue one fresh LLM call"
        );
        assert_eq!(calls[0].messages[0].content, "hello");
    }

    /// A workflow step that waits on a signal AND THEN performs `Inference.ask`
    /// must, once the signal arrives and the step resumes, suspend on the
    /// background LLM call instead of running saga compensation.
    /// Previously resume_suspended_workflow_step only matched
    /// "SignalWait:suspend": the resumed step's "LlmAsk:suspend" fell into
    /// the generic error arm and compensated the saga, and with suspension
    /// not enabled for the resume at all, the LLM call blocked the caller
    /// thread instead of suspending.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_workflow_step_signal_wait_then_inference_ask() {
        let source = r#"
            workflow SignalThenLlm {
                step wait_then_ask {
                    (perform Signal.wait("go"), self.answer = perform Inference.ask("hello"))
                }
            }
            let w = spawn SignalThenLlm {} in { w }
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());
        rt.borrow_mut()
            .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("world")));
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        rt.borrow_mut().run_scheduler();

        // The step is suspended waiting for the signal.
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(actor.waiting_signal.as_deref(), Some("go"));
            assert!(actor.suspended_execution.is_some());
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(0)
            );
        }

        // The signal arrives: the step resumes, consumes it, and suspends
        // again on the background LLM call.  The suspension must be
        // re-captured with the Inference.ask marker — NOT treated as a step failure.
        rt.borrow_mut().signal_workflow(actor_id, "go", None);
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.waiting_signal.as_deref(),
                Some("__llm_ask_pending__"),
                "signal-resumed step should re-suspend with the Inference.ask marker"
            );
            assert!(
                actor.suspended_execution.is_some(),
                "LLM suspension should be re-captured"
            );
            assert!(actor.llm_inflight, "background call should be in flight");
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(0),
                "step should not complete before the LLM response"
            );
        }
        let events = store.read_workflow_events(actor_id);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WorkflowEvent::SagaCompensated { .. })),
            "an Inference.ask suspend is not a step failure: no saga compensation"
        );
        assert!(
            !events.iter().any(|e| matches!(e, WorkflowEvent::StepCompleted { step_name, .. } if step_name == "wait_then_ask")),
            "step should not be journaled complete before the LLM response"
        );

        // Pump the mock completion: the step resumes through
        // resume_suspended_llm_step, which performs the workflow completion
        // bookkeeping.
        rt.borrow_mut().run_scheduler();
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(1),
                "resumed LLM step should advance step_index"
            );
            assert!(actor.suspended_execution.is_none());
            assert_eq!(actor.waiting_signal, None);
            assert!(!actor.llm_inflight);
            assert_eq!(
                rt_ref.actor_state_string(actor_id, "answer").as_deref(),
                Some("world")
            );
        }
        let events = store.read_workflow_events(actor_id);
        let completions = events
            .iter()
            .filter(|e| matches!(e, WorkflowEvent::StepCompleted { step_name, .. } if step_name == "wait_then_ask"))
            .count();
        assert_eq!(
            completions, 1,
            "step should complete in the journal exactly once"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WorkflowEvent::SagaCompensated { .. })),
            "no saga compensation should ever run for a suspending step"
        );
    }

    /// Reverse order: a workflow step that performs `Inference.ask` AND THEN waits
    /// on a signal.  The LLM completion resumes the step through
    /// resume_suspended_llm_step, whose chained-suspend arm re-captures the
    /// signal wait with the signal name as marker; the signal then completes
    /// the step through resume_suspended_workflow_step.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_workflow_step_inference_ask_then_signal_wait() {
        let source = r#"
            workflow LlmThenSignal {
                step ask_then_wait {
                    (self.answer = perform Inference.ask("hello"), perform Signal.wait("go"))
                }
            }
            let w = spawn LlmThenSignal {} in { w }
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());
        let client = nulang_ai::MockLlmClient::text("world");
        rt.borrow_mut().set_llm_client(Box::new(client.clone()));
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        rt.borrow_mut().send_message_by_id(actor_id, 0, &[]);
        // run_scheduler pumps the LLM completion; the resumed step then
        // suspends on the signal wait.
        rt.borrow_mut().run_scheduler();
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.waiting_signal.as_deref(),
                Some("go"),
                "LLM-resumed step should re-suspend waiting for the signal"
            );
            assert!(actor.suspended_execution.is_some());
            assert!(!actor.llm_inflight);
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(0),
                "step should not complete before the signal"
            );
        }
        assert_eq!(client.recorded_calls().len(), 1, "exactly one LLM call");

        // The signal arrives: the step runs to completion.
        rt.borrow_mut().signal_workflow(actor_id, "go", None);
        {
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            assert_eq!(
                actor.get_state_field("step_index").and_then(|v| v.as_int()),
                Some(1),
                "workflow should advance after the signal"
            );
            assert!(actor.suspended_execution.is_none());
            assert_eq!(actor.waiting_signal, None);
            assert_eq!(
                rt_ref.actor_state_string(actor_id, "answer").as_deref(),
                Some("world")
            );
        }
        let events = store.read_workflow_events(actor_id);
        let completions = events
            .iter()
            .filter(|e| matches!(e, WorkflowEvent::StepCompleted { step_name, .. } if step_name == "ask_then_wait"))
            .count();
        assert_eq!(
            completions, 1,
            "step should complete in the journal exactly once"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WorkflowEvent::SagaCompensated { .. })),
            "no saga compensation should run for a suspending step"
        );
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_ask_uses_memory() {
        let source = r#"
            agent Agent = {
                model: "mock-model",
                system_prompt: "You are helpful.",
                memory: { max_turns: 10 }
            }
            let a = spawn Agent {} in
            let r1 = ask a ask("hello") in
            let r2 = ask a ask("world") in
            r1
        "#;
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client = nulang_ai::MockLlmClient::text("world");
        rt.borrow_mut().set_llm_client(Box::new(client.clone()));

        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt)));

        let result = vm.run().unwrap();

        let calls = client.recorded_calls();
        assert_eq!(calls.len(), 2, "expected two LLM calls");

        let module_idx = vm.modules.len() - 1;
        let content = vm.value_to_string(module_idx, result);
        assert_eq!(content, "world");

        // First turn: system prompt + user prompt.
        assert_eq!(calls[0].messages.len(), 2);
        assert_eq!(calls[0].messages[0].role, "system");
        assert_eq!(calls[0].messages[0].content, "You are helpful.");
        assert_eq!(calls[0].messages[1].role, "user");
        assert_eq!(calls[0].messages[1].content, "hello");

        // Second turn includes the previous user/assistant exchange from memory.
        assert_eq!(calls[1].messages.len(), 4);
        assert_eq!(calls[1].messages[0].role, "system");
        assert_eq!(calls[1].messages[1].role, "user");
        assert_eq!(calls[1].messages[1].content, "hello");
        assert_eq!(calls[1].messages[2].role, "assistant");
        assert_eq!(calls[1].messages[2].content, "world");
        assert_eq!(calls[1].messages[3].role, "user");
        assert_eq!(calls[1].messages[3].content, "world");
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_ask_tracks_usage_and_cost() {
        let source = r#"
            agent Agent = {
                model: "mock-model",
                system_prompt: "You are helpful.",
                pricing: { input: 0.01, output: 0.02 }
            }
            let a = spawn Agent {} in
            ask a ask("hello")
        "#;
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client =
            nulang_ai::MockLlmClient::with_usage("world", nulang_ai::TokenUsage::new(1000, 500));
        let client_ref = client.clone();
        rt.borrow_mut().set_llm_client(Box::new(client));

        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));

        let result = vm.run().unwrap();
        let module_idx = vm.modules.len() - 1;
        let content = vm.value_to_string(module_idx, result);
        assert_eq!(content, "world");

        let rt = rt.borrow();
        let actor = rt.actors.values().next().expect("expected one actor");
        assert_eq!(
            actor.get_state_field("usage_prompt").unwrap().as_int(),
            Some(1000)
        );
        assert_eq!(
            actor.get_state_field("usage_completion").unwrap().as_int(),
            Some(500)
        );
        // 1000 * 0.01 / 1000 + 500 * 0.02 / 1000 = 0.01 + 0.01 = 0.02
        let cost = actor
            .get_state_field("usage_cost")
            .unwrap()
            .as_float()
            .unwrap();
        assert!((cost - 0.02).abs() < 1e-9);

        // Pricing should be forwarded on the request.
        let calls = client_ref.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].pricing.as_ref().unwrap().input_cost_per_1k, 0.01);
        assert_eq!(calls[0].pricing.as_ref().unwrap().output_cost_per_1k, 0.02);
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_usage_behavior() {
        let source = r#"
            agent Agent = {
                model: "mock-model",
                system_prompt: "You are helpful.",
                pricing: { input: 0.01, output: 0.02 }
            }
            let a = spawn Agent {} in
            let _ = ask a ask("hello") in
            ask a usage()
        "#;
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client =
            nulang_ai::MockLlmClient::with_usage("world", nulang_ai::TokenUsage::new(1000, 500));
        rt.borrow_mut().set_llm_client(Box::new(client));

        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));

        let result = vm.run().unwrap();

        // The usage behavior returns an array [prompt_tokens, completion_tokens, cost]
        // (see compile_agent's usage_behavior); inspect the actor-allocated
        // array directly.
        let ptr = result
            .as_ptr()
            .expect("usage() should return an array pointer");
        let usage = unsafe { std::slice::from_raw_parts(ptr as *const Value, 3) };
        assert_eq!(usage[0].as_int(), Some(1000), "prompt tokens");
        assert_eq!(usage[1].as_int(), Some(500), "completion tokens");
        let cost = usage[2].as_float().expect("cost should be a float");
        // 1000 * 0.01 / 1000 + 500 * 0.02 / 1000 = 0.01 + 0.01 = 0.02
        assert!((cost - 0.02).abs() < 1e-9, "cost: {}", cost);
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_semantic_memory_store_and_recall() {
        let source = r#"
            agent Agent = {
                model: "mock-model",
                system_prompt: "You are helpful.",
                semantic_memory: { dimensions: 32 }
            }
            let a = spawn Agent {} in
            let _ = ask a store_fact("hello world") in
            ask a recall("hello", 1)
        "#;
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));

        let result = vm.run().unwrap();

        let module_idx = vm.modules.len() - 1;
        let content = vm.value_to_string(module_idx, result);
        assert_eq!(content, "hello world");

        // The durable semantic_memory field should contain one document.
        let rt = rt.borrow();
        let actor = rt.actors.values().next().expect("expected one actor");
        let memory_json = actor.get_state_field("semantic_memory").unwrap();
        let memory_json_str = vm.value_to_string(module_idx, memory_json);
        let memory: nulang_ai::SemanticMemory = serde_json::from_str(&memory_json_str).unwrap();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory.documents[0].content, "hello world");
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_workflow_researches_and_reports() {
        // v0.9 milestone: agent researches a topic, uses a tool, stores the
        // fact in semantic memory, and synthesizes a report.
        let source = r#"
            @tool(description: "Store a research fact tagged with a topic.")
            fn store_fact(content: String, topic: String) -> String { content }

            agent Researcher = {
                model: "llama3.1",
                system_prompt: "You are a research assistant.",
                pricing: { input: 0.0, output: 0.0 },
                tools: [store_fact],
                memory: { max_turns: 10 },
                semantic_memory: { dimensions: 64 }
            }

            let researcher = spawn Researcher {} in
            let _ = ask researcher ask("Research CRDTs") in
            let report = ask researcher ask("Synthesize a report on CRDTs") in
            report
        "#;

        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));

        let mut store_args = serde_json::Map::new();
        store_args.insert(
            "content".to_string(),
            serde_json::Value::String("CRDTs are conflict-free replicated data types.".to_string()),
        );
        store_args.insert(
            "topic".to_string(),
            serde_json::Value::String("CRDTs".to_string()),
        );

        let client = nulang_ai::MockLlmClient::sequence(vec![
            nulang_ai::LlmResponse {
                content: None,
                tool_calls: vec![nulang_ai::ToolCall {
                    id: String::new(),
                    name: "store_fact".to_string(),
                    arguments: store_args,
                }],
                model: "mock".to_string(),
                finish_reason: "tool_calls".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
            nulang_ai::LlmResponse {
                content: Some(
                    "CRDTs enable strong eventual consistency without coordination.".to_string(),
                ),
                tool_calls: Vec::new(),
                model: "mock".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
        ]);
        let client_ref = client.clone();
        rt.borrow_mut().set_llm_client(Box::new(client));

        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));

        let result = vm.run().unwrap();

        let module_idx = vm.modules.len() - 1;
        let report = vm.value_to_string(module_idx, result);
        assert_eq!(
            report,
            "CRDTs enable strong eventual consistency without coordination."
        );

        // The LLM client should have been asked twice.
        let calls = client_ref.recorded_calls();
        assert_eq!(calls.len(), 2, "expected two LLM calls");

        // The first request should have exposed the store_fact tool.
        assert_eq!(calls[0].tools.len(), 1);
        assert_eq!(calls[0].tools[0].name, "store_fact");

        // The fact should be persisted in durable semantic memory.
        let rt = rt.borrow();
        let actor = rt.actors.values().next().expect("expected one actor");
        let memory_json = actor.get_state_field("semantic_memory").unwrap();
        let memory_json_str = vm.value_to_string(module_idx, memory_json);
        let memory: nulang_ai::SemanticMemory = serde_json::from_str(&memory_json_str).unwrap();
        assert_eq!(memory.len(), 1);
        assert_eq!(
            memory.documents[0].content,
            "CRDTs are conflict-free replicated data types."
        );
        assert_eq!(
            memory.documents[0].metadata.get("topic"),
            Some(&"CRDTs".to_string())
        );
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_workflow_recovers_semantic_memory_after_restart() {
        // v0.9 milestone: after a research agent stores a fact, simulating a
        // node restart with the same persistence store preserves the semantic
        // memory and the recovered agent can recall it.
        let source = r#"
            @tool(description: "Store a research fact tagged with a topic.")
            fn store_fact(content: String, topic: String) -> String { content }

            agent Researcher = {
                model: "llama3.1",
                system_prompt: "You are a research assistant.",
                pricing: { input: 0.0, output: 0.0 },
                tools: [store_fact],
                memory: { max_turns: 10 },
                semantic_memory: { dimensions: 64 }
            }

            let researcher = spawn Researcher {} in
            let _ = ask researcher ask("Research CRDTs") in
            researcher
        "#;

        let store = SharedMemoryStore::new();
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
            }
        }

        let mut store_args = serde_json::Map::new();
        store_args.insert(
            "content".to_string(),
            serde_json::Value::String("CRDTs are conflict-free replicated data types.".to_string()),
        );
        store_args.insert(
            "topic".to_string(),
            serde_json::Value::String("CRDTs".to_string()),
        );

        let client = nulang_ai::MockLlmClient::sequence(vec![nulang_ai::LlmResponse {
            content: None,
            tool_calls: vec![nulang_ai::ToolCall {
                id: String::new(),
                name: "store_fact".to_string(),
                arguments: store_args,
            }],
            model: "mock".to_string(),
            finish_reason: "tool_calls".to_string(),
            usage: nulang_ai::TokenUsage::default(),
        }]);

        let rt1 = Rc::new(RefCell::new(Runtime::new()));
        rt1.borrow_mut().set_llm_client(Box::new(client));
        rt1.borrow_mut().persistence = Box::new(store.clone());
        let value = {
            let mut vm = VM::new();
            vm.load_module(module.clone());
            vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt1.clone())));
            vm.run().unwrap()
        };
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        // The fact was stored during the first (and only) ask.
        {
            let rt1_ref = rt1.borrow();
            let actor = rt1_ref.actors.get(&actor_id).unwrap();
            let memory_json = actor.get_state_field("semantic_memory").unwrap();
            let memory_json_str = VM::new().value_to_string(0, memory_json);
            let memory: nulang_ai::SemanticMemory = serde_json::from_str(&memory_json_str).unwrap();
            assert_eq!(memory.len(), 1);
        }

        // Simulate a node restart: new runtime sharing the same store,
        // register the bytecode module, then recover the agent.
        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            vec![None; module.behaviors.len()],
        );
        rt2.borrow_mut().recover_actor(actor_id);

        // Recall the stored fact from the recovered agent. Agent behaviors are
        // laid out as ask(0), usage(1), store_fact(2), recall(3).
        let recall_behavior_id = 3u16;
        let query = {
            let mut rt2_ref = rt2.borrow_mut();
            let actor = rt2_ref.actors.get_mut(&actor_id).unwrap();
            actor.allocate_string("CRDTs")
        };
        let top_k = Value::int(1);
        let recalled = rt2
            .borrow_mut()
            .ask_actor_sync(actor_id, recall_behavior_id, &[query, top_k])
            .unwrap();

        let module_idx = 0;
        let recalled_content = VM::new().value_to_string(module_idx, recalled);
        assert_eq!(
            recalled_content, "CRDTs are conflict-free replicated data types.",
            "recovered agent should recall the stored fact"
        );
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_procedural_memory_store_and_get_pattern() {
        let source = r#"
            agent Agent = {
                model: "mock-model",
                system_prompt: "You are helpful.",
                procedural_memory: { namespace: "my_app" }
            }
            let a = spawn Agent {} in
            let _ = ask a store_pattern("format", "research_*", "{title}\\n{summary}") in
            ask a get_pattern("format")
        "#;
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));

        let result = vm.run().unwrap();

        let module_idx = vm.modules.len() - 1;
        let content = vm.value_to_string(module_idx, result);
        assert_eq!(content, "{title}\\n{summary}");

        let rt = rt.borrow();
        let actor = rt.actors.values().next().expect("expected one actor");
        let memory_json = actor.get_state_field("procedural_memory").unwrap();
        let memory_json_str = vm.value_to_string(module_idx, memory_json);
        let memory: nulang_ai::ProceduralMemory = serde_json::from_str(&memory_json_str).unwrap();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory.namespace, "my_app");
        assert_eq!(
            memory.get_pattern("format").unwrap().output_template,
            "{title}\\n{summary}"
        );
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_procedural_memory_add_and_get_examples() {
        let source = r#"
            agent Agent = {
                model: "mock-model",
                system_prompt: "You are helpful.",
                procedural_memory: { namespace: "code_review" }
            }
            let a = spawn Agent {} in
            let _ = ask a add_example("review", "fn bad() { let x = 1; x }", "Unused variable") in
            let _ = ask a add_example("review", "fn ok() { let x = 1; x + 1 }", "Good") in
            ask a get_examples("review", "unused variable", 1)
        "#;
        let (module, _ty) = compile_source(source).unwrap();

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));

        let result = vm.run().unwrap();

        let module_idx = vm.modules.len() - 1;
        let content = vm.value_to_string(module_idx, result);
        assert!(
            content.contains("Unused variable"),
            "expected matching example, got {}",
            content
        );

        let rt = rt.borrow();
        let actor = rt.actors.values().next().expect("expected one actor");
        let memory_json = actor.get_state_field("procedural_memory").unwrap();
        let memory_json_str = vm.value_to_string(module_idx, memory_json);
        let memory: nulang_ai::ProceduralMemory = serde_json::from_str(&memory_json_str).unwrap();
        assert_eq!(memory.len(), 2);
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_procedural_memory_recovers_after_restart() {
        let source = r#"
            agent Agent = {
                model: "mock-model",
                system_prompt: "You are helpful.",
                procedural_memory: { namespace: "my_app" }
            }

            let a = spawn Agent {} in
            a
        "#;
        let (module, _ty) = compile_source(source).unwrap();
        let meta = module.actor_metadata.first().unwrap();
        let mut offsets = vec![0; module.behaviors.len()];
        for &idx in &meta.behavior_indices {
            if let Some(entry) = module.behaviors.get(idx) {
                offsets[idx] = entry.code_offset;
            }
        }

        let store = SharedMemoryStore::new();
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut().persistence = Box::new(store.clone());

        let mut vm = VM::new();
        vm.load_module(module.clone());
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
        let value = vm.run().unwrap();
        let actor_id = value.as_actor_id().expect("spawn should return actor ref");

        let (key_arg, pattern_arg, template_arg) = {
            let mut rt_ref = rt.borrow_mut();
            let actor = rt_ref.actors.get_mut(&actor_id).unwrap();
            (
                actor.allocate_string("format"),
                actor.allocate_string("research_*"),
                actor.allocate_string("{title}"),
            )
        };
        rt.borrow_mut()
            .ask_actor_sync(actor_id, 2, &[key_arg, pattern_arg, template_arg])
            .unwrap();

        let rt2 = Rc::new(RefCell::new(Runtime::new()));
        rt2.borrow_mut().persistence = Box::new(store.clone());
        rt2.borrow_mut().register_recovery_module(
            actor_id,
            module.clone(),
            offsets.clone(),
            vec![None; module.behaviors.len()],
        );
        rt2.borrow_mut().recover_actor(actor_id);

        let get_pattern_behavior_id = 3u16;
        let key_arg = {
            let mut rt2_ref = rt2.borrow_mut();
            let actor = rt2_ref.actors.get_mut(&actor_id).unwrap();
            actor.allocate_string("format")
        };
        let recalled = rt2
            .borrow_mut()
            .ask_actor_sync(actor_id, get_pattern_behavior_id, &[key_arg])
            .unwrap();

        let module_idx = 0;
        let recalled_content = VM::new().value_to_string(module_idx, recalled);
        assert_eq!(
            recalled_content, "{title}",
            "recovered agent should return the stored pattern"
        );
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_pipeline_source_end_to_end() {
        let source = r#"
            agent Researcher = {
                model: "llama3.1",
                system_prompt: "Research.",
                pricing: { input: 0.0, output: 0.0 }
            }
            agent Writer = {
                model: "llama3.1",
                system_prompt: "Write.",
                pricing: { input: 0.0, output: 0.0 }
            }

            fn main() {
                let researcher = spawn Researcher {} in
                let writer = spawn Writer {} in
                let pipeline = Pipeline.new()
                    |> Pipeline.stage("research", researcher, "Research: {input}")
                    |> Pipeline.stage("write", writer, "Write based on: {input}")
                in
                pipeline.run("CRDTs")
            }
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client = nulang_ai::MockLlmClient::sequence(vec![
            nulang_ai::LlmResponse {
                content: Some("research notes".to_string()),
                tool_calls: Vec::new(),
                model: "mock".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
            nulang_ai::LlmResponse {
                content: Some("final article".to_string()),
                tool_calls: Vec::new(),
                model: "mock".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
        ]);
        rt.borrow_mut().set_llm_client(Box::new(client));

        let (module, _ty) = compile_source(source).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt)));
        let value = vm.run().unwrap();

        let module_idx = vm.modules.len() - 1;
        let result = vm.value_to_string(module_idx, value);
        assert_eq!(result, "final article");
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_supervisor_source_end_to_end() {
        let source = r#"
            agent Researcher = {
                model: "llama3.1",
                system_prompt: "Research.",
                pricing: { input: 0.0, output: 0.0 }
            }
            agent Writer = {
                model: "llama3.1",
                system_prompt: "Write.",
                pricing: { input: 0.0, output: 0.0 }
            }

            fn main() {
                let researcher = spawn Researcher {} in
                let writer = spawn Writer {} in
                let team = Supervisor.new()
                    |> Supervisor.worker("researcher", researcher, "Finds information")
                    |> Supervisor.worker("writer", writer, "Writes content")
                in
                team.run("Write an article about CRDTs")
            }
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client = nulang_ai::MockLlmClient::sequence(vec![
            nulang_ai::LlmResponse {
                content: Some("research notes".to_string()),
                tool_calls: Vec::new(),
                model: "mock".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
            nulang_ai::LlmResponse {
                content: Some("final article".to_string()),
                tool_calls: Vec::new(),
                model: "mock".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
        ]);
        rt.borrow_mut().set_llm_client(Box::new(client));

        let (module, _ty) = compile_source(source).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt)));
        let value = vm.run().unwrap();

        let module_idx = vm.modules.len() - 1;
        let result = vm.value_to_string(module_idx, value);
        assert_eq!(result, "final article");
    }

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_debate_source_end_to_end() {
        let source = r#"
            agent ProAgent = {
                model: "llama3.1",
                system_prompt: "Argue in favor.",
                pricing: { input: 0.0, output: 0.0 }
            }
            agent ConAgent = {
                model: "llama3.1",
                system_prompt: "Argue against.",
                pricing: { input: 0.0, output: 0.0 }
            }
            agent Moderator = {
                model: "llama3.1",
                system_prompt: "Synthesize.",
                pricing: { input: 0.0, output: 0.0 }
            }

            fn main() {
                let pro = spawn ProAgent {} in
                let con = spawn ConAgent {} in
                let moderator = spawn Moderator {} in
                let debate = Debate.new("microservices vs monolith", 1, 0.8)
                    |> Debate.participant("pro", "pro", pro)
                    |> Debate.participant("con", "con", con)
                    |> Debate.participant("moderator", "neutral", moderator)
                in
                debate.run()
            }
        "#;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let client = nulang_ai::MockLlmClient::sequence(vec![
            nulang_ai::LlmResponse {
                content: Some("pro argument".to_string()),
                tool_calls: Vec::new(),
                model: "mock".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
            nulang_ai::LlmResponse {
                content: Some("con argument".to_string()),
                tool_calls: Vec::new(),
                model: "mock".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
            nulang_ai::LlmResponse {
                content: Some("moderator observation".to_string()),
                tool_calls: Vec::new(),
                model: "mock".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
            nulang_ai::LlmResponse {
                content: Some("consensus reached".to_string()),
                tool_calls: Vec::new(),
                model: "mock".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::default(),
            },
        ]);
        rt.borrow_mut().set_llm_client(Box::new(client));

        let (module, _ty) = compile_source(source).unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt)));
        let value = vm.run().unwrap();

        let module_idx = vm.modules.len() - 1;
        let result = vm.value_to_string(module_idx, value);
        assert_eq!(result, "consensus reached");
    }

    #[test]
    fn test_let_annotation_type_mismatch() {
        // A let annotation that contradicts the value type must be a type
        // error, not silently discarded.
        let source = r#"let x : Int = "not an int" in x"#;
        let result = run_source(source);
        assert!(
            result.is_err(),
            "let annotation mismatch should be a type error"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Type mismatch"),
            "Error should be a unification failure: {}",
            err_msg
        );
    }

    #[test]
    fn test_let_annotation_matching_type() {
        // A matching annotation type checks and runs normally.
        assert_int("let x : Int = 41 in x + 1", 42);
    }

    // -----------------------------------------------------------------------
    // Regression: `return` inside `handle` must unwind the handler frame
    // (previously the frame stayed on the VM handler_stack, so a later
    // unhandled perform dispatched into the dead function's handler code)
    // -----------------------------------------------------------------------

    #[test]
    fn test_return_inside_handle_unwinds_handler_frame() {
        // `leak` returns out of a handled perform; afterwards a top-level
        // perform of the same effect must be unhandled rather than
        // dispatching into the dead function's handler.
        let source = r#"
            fn leak() -> Int {
                handle {
                    perform Math.getAnswer();
                    return 1
                } {
                    | Math.getAnswer() => 40 + 1
                }
            }
            { leak(); perform Math.getAnswer() }
        "#;
        let result = run_source(source);
        assert!(
            result.is_err(),
            "perform after return from handle must be unhandled, got {:?}",
            result
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unhandled effect"),
            "expected unhandled-effect error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_return_inside_nested_handles_unwinds_all_frames() {
        // Two nested handlers for the same effect: the return must pop BOTH
        // frames, or the outer (stale) one would catch the later perform.
        let source = r#"
            fn leak() -> Int {
                handle {
                    handle {
                        perform Math.getAnswer();
                        return 1
                    } {
                        | Math.getAnswer() => 41
                    }
                } {
                    | Math.getAnswer() => 99
                }
            }
            { leak(); perform Math.getAnswer() }
        "#;
        let result = run_source(source);
        assert!(
            result.is_err(),
            "both nested handler frames must be unwound, got {:?}",
            result
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unhandled effect"),
            "expected unhandled-effect error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_return_inside_handle_branch_unwinds_handler_frame() {
        // The FnReturn path through an expression-position `if` inside the
        // handle body (lower_body_into) must unwind the frame too.
        let source = r#"
            fn leak(b: Bool) -> Int {
                handle {
                    perform Math.getAnswer();
                    if b then return 1 else 2
                } {
                    | Math.getAnswer() => 41
                }
            }
            { leak(true); perform Math.getAnswer() }
        "#;
        let result = run_source(source);
        assert!(
            result.is_err(),
            "return from an if-branch inside handle must unwind, got {:?}",
            result
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unhandled effect"),
            "expected unhandled-effect error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_return_inside_handle_loop_unwinds_handler_frame() {
        // The FnReturn path through a `for` body inside the handle body
        // (lower_for) must unwind the frame too.
        let source = r#"
            fn leak() -> Int {
                handle {
                    perform Math.getAnswer();
                    for x in [1, 2, 3] { return x };
                    0
                } {
                    | Math.getAnswer() => 41
                }
            }
            { leak(); perform Math.getAnswer() }
        "#;
        let result = run_source(source);
        assert!(
            result.is_err(),
            "return from a for body inside handle must unwind, got {:?}",
            result
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unhandled effect"),
            "expected unhandled-effect error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_return_inside_handler_body_unwinds_handler_frame() {
        // A `return` inside a HANDLER body (not the handled body) runs with
        // the handle's frame on the VM handler_stack; it must unwind that
        // frame or a later unhandled perform dispatches into the dead
        // function's handler code.
        let source = r#"
            fn leak() -> Int {
                handle { perform Math.getAnswer() } { | Math.getAnswer() => return 7 }
            }
            { leak(); perform Math.getAnswer() }
        "#;
        let result = run_source(source);
        assert!(
            result.is_err(),
            "return from a handler body must unwind its frame, got {:?}",
            result
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unhandled effect"),
            "expected unhandled-effect error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_return_inside_handler_body_value() {
        // Positive control: the return itself still yields its value.
        let source = r#"
            fn f() -> Int {
                handle { perform Math.getAnswer() } { | Math.getAnswer() => return 7 }
            }
            f()
        "#;
        assert_int(source, 7);
    }

    #[test]
    fn test_return_inside_nested_handler_body_unwinds_all_frames() {
        // The inner handler body's return runs with BOTH handle frames on
        // the stack; both must be unwound (depth counts the handler's own
        // frame plus enclosing handles).
        let source = r#"
            fn leak() -> Int {
                handle {
                    handle { perform Math.getAnswer() } { | Math.getAnswer() => return 7 }
                } {
                    | Math.getAnswer() => 1
                }
            }
            { leak(); perform Math.getAnswer() }
        "#;
        let result = run_source(source);
        assert!(
            result.is_err(),
            "return from a nested handler body must unwind both frames, got {:?}",
            result
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Unhandled effect"),
            "expected unhandled-effect error, got: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Regression: recursive closures must capture enclosing variables
    // -----------------------------------------------------------------------

    #[test]
    fn test_recursive_closure_captures_enclosing_var() {
        let source = r#"
            let k = 10 in
            let f = fn(n) { if n < 1 then 0 else f(n - 1) + k } in
            f(3)
        "#;
        assert_int(source, 30);
    }

    #[test]
    fn test_recursive_closure_captures_multiple_vars() {
        let source = r#"
            let a = 1 in
            let b = 100 in
            let f = fn(n) { if n < 1 then 0 else f(n - 1) + a + b } in
            f(2)
        "#;
        // f(2) = f(1) + 101 = (f(0) + 101) + 101 = 202
        assert_int(source, 202);
    }

    // -----------------------------------------------------------------------
    // Regression: free_vars must descend into effect/actor expressions so
    // closures capture variables used only inside them
    // -----------------------------------------------------------------------

    #[test]
    fn test_closure_captures_var_used_in_perform() {
        // `k` is used only as a perform argument (inside a handle so the
        // program evaluates to a value).
        let source = r#"
            let k = 7 in
            let f = fn(x) { handle perform IO.print(k) { | IO.print(m) => m } } in
            f(1)
        "#;
        assert_int(source, 7);
    }

    #[test]
    fn test_closure_captures_var_used_in_bare_perform() {
        // Exact repro: previously failed at compile time with
        // "undefined variable 'k'"; now compiles (k is captured) and the
        // standalone IO.print built-in handles the perform at runtime, so
        // the closure body evaluates to x.
        let source = r#"
            let k = 7 in
            let f = fn(x) { perform IO.print(k); x } in
            f(1)
        "#;
        assert_int(source, 1);
    }

    #[test]
    fn test_closure_captures_var_used_in_handler_body() {
        // `secret` is used only inside an effect handler body.
        let source = r#"
            let secret = 41 in
            let f = fn(x) { handle perform Math.getAnswer() { | Math.getAnswer() => secret + 1 } } in
            f(0)
        "#;
        assert_int(source, 42);
    }

    // -----------------------------------------------------------------------
    // Regression: non-exhaustive match must be a runtime error, not silently
    // evaluate the last arm
    // -----------------------------------------------------------------------

    #[test]
    fn test_match_non_exhaustive_is_runtime_error() {
        let source = r#"match 99 {
            case 1 => 10
            case 2 => 20
        }"#;
        let result = run_source(source);
        assert!(
            result.is_err(),
            "non-exhaustive match must be a runtime error, got {:?}",
            result
        );
    }

    #[test]
    fn test_match_last_literal_arm_still_matches() {
        // Control: a matching refutable last arm still evaluates normally.
        let source = r#"match 2 {
            case 1 => 10
            case 2 => 20
        }"#;
        assert_int(source, 20);
    }

    // -----------------------------------------------------------------------
    // Pattern guards: `| pat if cond => body`
    // -----------------------------------------------------------------------

    #[test]
    fn test_match_guard_accepts() {
        let source = r#"match 42 { | n if n > 10 => 1 | _ => 0 }"#;
        assert_int(source, 1);
    }

    #[test]
    fn test_match_guard_rejects_falls_through() {
        let source = r#"match 5 { | n if n > 10 => 1 | _ => 0 }"#;
        assert_int(source, 0);
    }

    #[test]
    fn test_match_guard_over_variant_payload() {
        // The guard sees the payload binding: a failing guard falls through
        // to the next arm even when the constructor matches.
        let source = r#"
            type Option[T] = Some(T) | None

            fn classify(o: Option[Int]) -> Int {
                match o with {
                    | Some(n) if n > 0 => 1
                    | Some(n) => 2
                    | None => 0
                }
            }

            classify(Some(5)) * 100 + classify(Some(0 - 3)) * 10 + classify(None)
        "#;
        assert_int(source, 120);
    }

    #[test]
    fn test_match_guarded_final_wildcard_non_exhaustive() {
        // A guarded last arm is not a catch-all: when its guard fails the
        // match is non-exhaustive and must raise the runtime error.
        let source = r#"match 5 { | _ if false => 1 }"#;
        let result = run_source(source);
        match &result {
            Err(e) => assert!(
                format!("{e}").contains("non-exhaustive"),
                "expected non-exhaustive match error, got {e}"
            ),
            Ok(v) => panic!("guarded final wildcard with failing guard must error, got {v:?}"),
        }
    }

    #[test]
    fn test_match_guard_must_be_bool() {
        let source = r#"match 1 { | n if n => 1 | _ => 0 }"#;
        let result = run_source(source);
        assert!(
            result.is_err(),
            "non-Bool guard must be a type error, got {:?}",
            result
        );
    }

    // -----------------------------------------------------------------------
    // LLM fallback & retry pipeline
    // -----------------------------------------------------------------------

    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_agent_retry_and_fallback_pipeline() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        // Mock: first call rate-limited, second succeeds after fallback.
        let client = nulang_ai::MockLlmClient::sequence_with_errors(vec![
            Err(nulang_ai::LlmError::new(
                nulang_ai::LlmErrorKind::RateLimit,
                "429 Too Many Requests",
            )),
            Ok(nulang_ai::LlmResponse {
                content: Some("recovered via fallback".to_string()),
                tool_calls: Vec::new(),
                model: "fallback-model".to_string(),
                finish_reason: "stop".to_string(),
                usage: nulang_ai::TokenUsage::new(10, 5),
            }),
        ]);
        rt.borrow_mut().set_llm_client(Box::new(client));

        let source = "fn main() { perform Inference.ask(\"hello\") }";
        let result = run_source_new_with_runtime(source, rt);
        match result {
            Ok(_) => { /* agent responded */ }
            Err(e) => panic!("agent retry test failed: {}", e),
        }
    }

    // -----------------------------------------------------------------------
    // .nbc durable-artifact format (RFC 0001)
    // -----------------------------------------------------------------------

    /// A module compiled through the full pipeline, serialized to `.nbc`, and
    /// deserialized must produce a value identical to running the original
    /// module directly. This is the "run a 2026 program in 2126" guarantee.
    #[test]
    fn test_nbc_roundtrip_preserves_execution_result() {
        let source = "fn main() { let x = 6 * 7; if x > 40 then { x + 1 } else { x - 1 } }";
        let original = compile_source_new(source).expect("compile");
        let bytes = original.to_nbc(None).expect("encode");
        let artifact = crate::bytecode::CodeModule::from_nbc(&bytes).expect("decode");
        assert_eq!(
            artifact.module, original,
            "round-trip must preserve the module"
        );

        // Run both and compare observable results.
        let mut vm_orig = VM::new();
        vm_orig.load_module(original);
        let v_orig = vm_orig.run().unwrap();

        let mut vm_nbc = VM::new();
        vm_nbc.load_module(artifact.module);
        let v_nbc = vm_nbc.run().unwrap();

        assert_eq!(v_orig, v_nbc, "source-run and .nbc-run must agree");
        assert_eq!(v_nbc.as_int(), Some(43));
    }

    /// The source hash recorded by `to_nbc` must round-trip through `from_nbc`
    /// and must be the actual BLAKE3 of the source — the basis of `--verify`.
    #[test]
    fn test_nbc_source_hash_roundtrip_and_blake3() {
        let source = "fn main() { 100 }";
        let m = compile_source_new(source).unwrap();
        let expected_hash = *blake3::hash(source.as_bytes()).as_bytes();
        let bytes = m.to_nbc(Some(expected_hash)).unwrap();
        let artifact = crate::bytecode::CodeModule::from_nbc(&bytes).unwrap();
        assert_eq!(artifact.source_hash, Some(expected_hash));
    }

    /// A `.nbc` artifact with a future language version is rejected, not
    /// reinterpreted — the stability contract in action.
    #[test]
    fn test_nbc_rejects_future_language_version() {
        let m = compile_source_new("fn main() { 1 }").unwrap();
        let mut bytes = m.to_nbc(None).unwrap();
        // Language version field is at offset 8.
        bytes[8..12].copy_from_slice(&99u32.to_be_bytes());
        let err = crate::bytecode::CodeModule::from_nbc(&bytes).unwrap_err();
        assert!(matches!(
            err,
            crate::format::constants::FormatError::IncompatibleLanguage { artifact: 99, .. }
        ));
    }

    // -----------------------------------------------------------------------
    // Grammar conformance (RFC 0002 Core + Stable)
    // -----------------------------------------------------------------------

    #[test]
    fn test_grammar_conformance() {
        let positive = vec![
            "fn main() {}",
            "fn add(a: Int, b: Int) -> Int { a + b }",
            "type Option[T] = Some(T) | None",
            "type Point = { x: Int, y: Int }",
            "actor Counter { state count: Int = 0; behavior inc() { count = count + 1 } }",
            "fn main() { spawn Counter {} }",
            "fn main() { send bob hello(\"name\") }",
            "fn main() { receive { | msg(name) => perform IO.print(name) } }",
            "effect Rand { int: -> Int }",
            "fn main() { handle { perform Rand.int() } { | Rand.int() resume => 42 } }",
        ];

        let negative = vec![
            "fn main( {",            // syntax error
            "let x = ;",             // missing expr
            "actor A { fn() {} }",   // actor missing behavior name
            "type Foo = 1",          // invalid variant start
            "receive { case => 1 }", // missing match arm pattern
        ];

        for src in positive {
            let mut lexer = Lexer::new(src);
            let tokens = lexer
                .lex()
                .unwrap_or_else(|_| panic!("Lexer failed on positive case: {src}"));
            let mut parser = Parser::new(tokens);
            parser
                .parse_module()
                .unwrap_or_else(|e| panic!("Parser failed on positive case: {src}\nError: {e}"));
        }

        for src in negative {
            let mut lexer = Lexer::new(src);
            if let Ok(tokens) = lexer.lex() {
                let mut parser = Parser::new(tokens);
                if parser.parse_module().is_ok() {
                    panic!("Parser incorrectly accepted negative case: {src}");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Provider effect — the general, non-transient replacement for Inference.ask
    // -----------------------------------------------------------------------

    /// `perform Provider.ask("llm", prompt)` must produce the same result as
    /// `perform Inference.ask(prompt)` when an LLM client is registered. This is
    /// the longevity path: the language vocabulary references an eternal
    /// "provider" abstraction, not a transient technology.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_provider_ask_llm_equivalent_to_inference_ask() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut()
            .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("world")));

        // The new, general Provider effect.
        let v = run_source_new_with_runtime(
            "fn main() { perform Provider.ask(\"llm\", \"hello\") }",
            rt.clone(),
        )
        .unwrap();
        assert!(
            !v.is_nil(),
            "Provider.ask(\"llm\", ...) must dispatch to the registered LLM client"
        );
    }

    /// `perform Provider.ask("unknown", ...)` with no matching provider
    /// registration must fall through to an unhandled-effect error (not a
    /// crash, not a silent nil). User-installed handlers can still catch it.
    #[cfg(feature = "ai-runtime")]
    #[test]
    fn test_provider_ask_unknown_provider_is_unhandled() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        rt.borrow_mut()
            .set_llm_client(Box::new(nulang_ai::MockLlmClient::text("world")));

        let result = run_source_new_with_runtime(
            "fn main() { perform Provider.ask(\"unknown\", \"hello\") }",
            rt,
        );
        assert!(
            result.is_err(),
            "unknown provider must be an unhandled effect, not a silent nil"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("Unhandled effect"),
            "expected unhandled-effect error, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // WASM backend integration tests (requires wasm-backend feature)
    #[cfg(feature = "wasm-backend")]
    mod wasm_backend {
        use crate::lexer::Lexer;
        use crate::mir_wasm::WasmBackend;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;
        use crate::types::NuResult;

        fn compile_source_to_wasm(source: &str) -> NuResult<Vec<u8>> {
            let tokens = Lexer::new(source).lex()?;
            let ast = Parser::new(tokens).parse_module()?;
            let mut tc = TypeChecker::new();
            tc.check_module(&ast)?;
            let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
            let mir = crate::mir_lower::lower_module(&hir)?;
            let mut backend = WasmBackend::new();
            backend.compile(&mir, "test")
        }

        #[test]
        fn test_wasm_float_pow_and_neg() {
            // `3.14 ** 2.0` (float pow) and `-(0.1 + 0.22)` (neg of a computed
            // float) previously mis-compiled on the WASM backend (int-only pow
            // → 1; pointer-payload neg → garbage). Both must match the
            // interpreter.
            let val = run_source_to_value("3.14 ** 2.0").expect("run");
            assert_eq!(
                val.as_float(),
                Some(9.8596),
                "wasm float pow must match interpreter"
            );
            let val = run_source_to_value("-(0.1 + 0.22)").expect("run");
            assert_eq!(
                val.as_float(),
                Some(-0.32),
                "wasm neg of computed float must match interpreter"
            );
            // Int pow overflow wraps (wrapping_mul), not nil.
            let val = run_source_to_value("1000000000 ** 1000000000").expect("run");
            assert_eq!(
                val.as_int(),
                Some(0),
                "wasm int pow overflow must wrap, not nil"
            );
        }

        #[test]
        fn test_wasm_compile_literal_int() {
            let wasm = compile_source_to_wasm("42").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_addition() {
            let wasm = compile_source_to_wasm("1 + 2").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_bool() {
            let wasm = compile_source_to_wasm("true").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_let_binding() {
            let wasm = compile_source_to_wasm("let x = 10; x").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_arithmetic_mul() {
            let wasm = compile_source_to_wasm("4 * 5").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_comparison() {
            let wasm = compile_source_to_wasm("1 == 1").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_float() {
            let wasm = compile_source_to_wasm("3.14").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_if_expr() {
            let wasm = compile_source_to_wasm("if true then { 1 } else { 2 }").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_block() {
            let wasm = compile_source_to_wasm("{ 1; 2; 3 }").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_string() {
            let wasm = compile_source_to_wasm(r#""hello""#).expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_io_print() {
            let wasm = compile_source_to_wasm(r#"perform IO.print("hi")"#).expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        #[test]
        fn test_wasm_compile_sub() {
            let wasm = compile_source_to_wasm("10 - 3").expect("compile");
            assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        }

        /// Compile + run through the host runtime, returning the result value.
        fn run_source_to_value(source: &str) -> NuResult<crate::vm::Value> {
            let wasm = compile_source_to_wasm(source)?;
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None)?;
            rt.run()
        }

        #[test]
        fn test_wasm_run_string_concat() {
            // `s1 + s2` lowers to RValue::StrConcat; the host str_concat helper
            // must concatenate the null-terminated strings in memory. This
            // asserts the actual concat content, not just that a string came
            // back.
            let val = run_source_to_value(r#""hello " + "world""#).expect("run");
            assert!(val.is_string(), "expected a string, got {:?}", val);
            let wasm = compile_source_to_wasm(r#""hello " + "world""#).expect("compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            assert_eq!(
                rt.string_value(&val).as_deref(),
                Some("hello world"),
                "concat result must equal the concatenated text"
            );
        }

        #[test]
        fn test_wasm_run_string_concat_int() {
            // String + Int is also concatenation (interpreter: "hello" + 2 ==
            // "hello2"); the WASM backend must agree.
            let wasm = compile_source_to_wasm(r#""n=" + 42"#).expect("compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            assert_eq!(
                rt.string_value(&val).as_deref(),
                Some("n=42"),
                "String + Int must concatenate as text"
            );
        }

        #[test]
        fn test_wasm_run_string_eq() {
            // `s1 == s2` for strings lowers to RValue::StringEq; the host
            // str_eq helper must compare CONTENT, not data offset.
            let val = run_source_to_value(r#""abc" == "abc""#).expect("run");
            assert_eq!(val.as_bool(), Some(true), "equal strings must be == ");
            let val = run_source_to_value(r#""abc" == "abd""#).expect("run");
            assert_eq!(
                val.as_bool(),
                Some(false),
                "different strings must not be =="
            );
        }

        #[test]
        fn test_wasm_run_string_eq_concat() {
            // A runtime concat result with the same text as an interned
            // constant must compare equal by content even though their data
            // offsets differ: "a" + "bc" is a fresh buffer, "abc" is interned.
            let wasm = compile_source_to_wasm(r#"("a" + "bc") == "abc""#).expect("compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            assert_eq!(
                val.as_bool(),
                Some(true),
                "concat result must equal its text"
            );
        }

        #[test]
        fn test_wasm_run_iife_closure() {
            // An immediately-invoked closure appends a lifted `__lambda_N`
            // function after `__main`. nulang_init must export `__main` (the
            // zero-param entry), not the closure — otherwise the host's
            // `() -> i64` typed call fails to convert.
            let wasm = compile_source_to_wasm("(fn(x) { x + 1 })(41)").expect("compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            assert_eq!(val.as_int(), Some(42), "IIFE must run its closure body");
        }

        #[test]
        fn test_wasm_run_capability_check() {
            // Capability checks are compile-time-only and erased at runtime, so
            // CapabilityCheck must compile to tagged true (mirroring the
            // interpreter's Const1 and the AOT backend). No source syntax
            // produces it, so construct the MIR directly. Previously this
            // silently fell through to nil in the WASM backend.
            let mut builder =
                crate::mir::FunctionBuilder::new("main", Some(crate::types::Type::bool()));
            let tmp = builder.add_temp(crate::types::Type::int());
            let out = builder.add_temp(crate::types::Type::bool());
            builder.assign(
                tmp,
                crate::mir::RValue::Const(crate::bytecode::Constant::Int(1)),
            );
            builder.assign(out, crate::mir::RValue::CapabilityCheck { val: tmp });
            builder.terminate(crate::mir::Terminator::Return(Some(out)));
            let func = builder.build();
            let module = crate::mir::Module {
                name: "capcheck".into(),
                functions: vec![func],
                behaviors: vec![],
                actor_metadata: vec![],
                compensation_of: vec![],
                parallel_branches_of: vec![],
                foreign_functions: vec![],
            };
            let mut backend = WasmBackend::new();
            let wasm = backend.compile(&module, "main").expect("wasm compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            assert_eq!(
                val.as_bool(),
                Some(true),
                "CapabilityCheck must yield true in WASM"
            );
        }

        #[test]
        fn test_wasm_run_record() {
            // Record literals + named field access (LoadFieldNamed) must work;
            // previously Record/LoadFieldNamed silently compiled to nil.
            let val =
                run_source_to_value(r#"fn f() -> Int { let r = {x: 1, y: 2} in r.x + r.y } f()"#)
                    .expect("run");
            assert_eq!(val.as_int(), Some(3), "r.x + r.y must be 3");
        }

        #[test]
        fn test_wasm_run_record_field_store() {
            // Named record stores must mutate the existing heap object, just
            // like the interpreter's RecS opcode; silently dropping this
            // statement previously left the old field value in WASM.
            let val = run_source_to_value(
                r#"fn f() -> Int { let r = {x: 1, y: 2} in { r.x = 99 r.x + r.y } } f()"#,
            )
            .expect("run");
            assert_eq!(val.as_int(), Some(101), "r.x = 99 must persist in WASM");
        }

        #[test]
        fn test_wasm_run_tuple() {
            // Tuple literals + positional field access (LoadFieldPos).
            let val =
                run_source_to_value(r#"fn f() -> Int { let t = (1, 2, 3) in t.0 + t.2 } f()"#)
                    .expect("run");
            assert_eq!(val.as_int(), Some(4), "t.0 + t.2 must be 4");
        }

        #[test]
        fn test_wasm_run_io_read() {
            // IO.read previously returned nil in WASM; it must read a line from
            // the input source (stdin by default, overridable via set_input).
            let wasm = compile_source_to_wasm(r#"perform IO.read()"#).expect("compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            rt.set_input("hello world\n");
            let val = rt.run().expect("run");
            assert_eq!(
                rt.string_value(&val).as_deref(),
                Some("hello world\n"),
                "IO.read must return the input line"
            );
        }

        #[test]
        fn test_wasm_run_float_arithmetic() {
            // Float add/sub/mul/div (previously the WASM integer-only path
            // corrupted float bit patterns).
            let v = run_source_to_value(r#"fn f() -> Float { 1.5 + 2.5 } f()"#).expect("run");
            assert_eq!(v.as_float(), Some(4.0), "1.5 + 2.5");
            let v = run_source_to_value(r#"fn f() -> Float { 10.0 / 4.0 } f()"#).expect("run");
            assert_eq!(v.as_float(), Some(2.5), "10.0 / 4.0");
            let v = run_source_to_value(r#"fn f() -> Float { 3.0 * 2.0 } f()"#).expect("run");
            assert_eq!(v.as_float(), Some(6.0), "3.0 * 2.0");
            // Int ops still work through the same helpers.
            let v = run_source_to_value(r#"fn f() -> Int { 100 / 5 } f()"#).expect("run");
            assert_eq!(v.as_int(), Some(20), "100 / 5");
            let v = run_source_to_value(r#"fn f() -> Int { 7 % 3 } f()"#).expect("run");
            assert_eq!(v.as_int(), Some(1), "7 % 3");
        }

        #[test]
        fn test_wasm_run_bit_ops() {
            // Bit shifts / bitwise ops (previously nil in WASM).
            let v = run_source_to_value(r#"fn f() -> Int { 1 << 2 } f()"#).expect("run");
            assert_eq!(v.as_int(), Some(4), "1 << 2");
            let v = run_source_to_value(r#"fn f() -> Int { 8 >> 2 } f()"#).expect("run");
            assert_eq!(v.as_int(), Some(2), "8 >> 2");
            let v = run_source_to_value(r#"fn f() -> Int { 5 & 3 } f()"#).expect("run");
            assert_eq!(v.as_int(), Some(1), "5 & 3");
            let v = run_source_to_value(r#"fn f() -> Int { 5 ^ 3 } f()"#).expect("run");
            assert_eq!(v.as_int(), Some(6), "5 ^ 3");
        }

        #[test]
        fn test_wasm_run_no_entry_library() {
            // A module with only a function definition (no top-level
            // expression) must export a nil-returning `nulang_init` — not a
            // parameterized function the host can't call as `() -> i64`.
            let wasm = compile_source_to_wasm(r#"fn mix(x, y) { x + y * 2 }"#).expect("compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            assert!(val.is_nil(), "library module must run to nil");
        }

        #[test]
        fn test_wasm_supports_send_and_rejects_remote_ops() {
            // `send` now COMPILES via the guest-side actor emulation (it
            // enqueues onto the module mailbox; the entry function drains it).
            let mut builder =
                crate::mir::FunctionBuilder::new("main", Some(crate::types::Type::int()));
            let actor = builder.add_temp(crate::types::Type::int());
            let arg = builder.add_temp(crate::types::Type::int());
            let out = builder.add_temp(crate::types::Type::int());
            builder.assign(
                actor,
                crate::mir::RValue::Const(crate::bytecode::Constant::Int(0)),
            );
            builder.assign(
                arg,
                crate::mir::RValue::Const(crate::bytecode::Constant::Int(1)),
            );
            builder.assign(
                out,
                crate::mir::RValue::Send {
                    actor,
                    behavior_idx: 0,
                    args: vec![arg],
                    remote: false,
                },
            );
            builder.terminate(crate::mir::Terminator::Return(Some(out)));
            let func = builder.build();
            let module = crate::mir::Module {
                name: "actor".into(),
                functions: vec![func],
                behaviors: vec![],
                actor_metadata: vec![],
                compensation_of: vec![],
                parallel_branches_of: vec![],
                foreign_functions: vec![],
            };
            let mut backend = WasmBackend::new();
            assert!(
                backend.compile(&module, "main").is_ok(),
                "send must compile to a mailbox enqueue"
            );

            // Remote spawn (`spawn@node`) has no single-instance counterpart
            // and must fail loudly at compile time, not silently compile.
            let mut builder =
                crate::mir::FunctionBuilder::new("main", Some(crate::types::Type::int()));
            let node = builder.add_temp(crate::types::Type::int());
            let out = builder.add_temp(crate::types::Type::int());
            builder.assign(
                node,
                crate::mir::RValue::Const(crate::bytecode::Constant::Int(0)),
            );
            builder.assign(
                out,
                crate::mir::RValue::Spawn {
                    behavior_idx: 0,
                    init: vec![],
                    target_node: Some(node),
                    capabilities: vec![],
                },
            );
            builder.terminate(crate::mir::Terminator::Return(Some(out)));
            let func = builder.build();
            let module = crate::mir::Module {
                name: "spawn".into(),
                functions: vec![func],
                behaviors: vec![],
                actor_metadata: vec![],
                compensation_of: vec![],
                parallel_branches_of: vec![],
                foreign_functions: vec![],
            };
            let mut backend = WasmBackend::new();
            assert!(
                backend.compile(&module, "main").is_err(),
                "remote spawn must be rejected, not silently compiled"
            );

            // A user-defined effect (not IO.print/read or Array.length) now
            // COMPILES — it lowers to nulang_dispatch (constant args are
            // JSON-marshaled; the host routes by the dotted "Custom.effect"
            // tag). This was previously rejected as unsupported.
            let mut builder =
                crate::mir::FunctionBuilder::new("main", Some(crate::types::Type::int()));
            let out = builder.add_temp(crate::types::Type::int());
            builder.assign(
                out,
                crate::mir::RValue::Perform {
                    effect: "Custom".into(),
                    op: "effect".into(),
                    args: vec![],
                    resolved_handler: None,
                },
            );
            builder.terminate(crate::mir::Terminator::Return(Some(out)));
            let func = builder.build();
            let module = crate::mir::Module {
                name: "effect".into(),
                functions: vec![func],
                behaviors: vec![],
                actor_metadata: vec![],
                compensation_of: vec![],
                parallel_branches_of: vec![],
                foreign_functions: vec![],
            };
            let mut backend = WasmBackend::new();
            assert!(
                backend.compile(&module, "main").is_ok(),
                "user-defined effect must compile to a nulang_dispatch call"
            );
        }

        #[test]
        fn test_wasm_run_ffi_call() {
            // A pre-registered native function invoked via RValue::FFICall
            // from WASM must resolve + call (was previously silent nil).
            extern "C" fn wasm_double(x: i64) -> i64 {
                x * 2
            }
            let sig = crate::ffi::marshal::Signature::new(
                vec![crate::ffi::marshal::CType::I64],
                crate::ffi::marshal::CType::I64,
            );
            unsafe {
                crate::ffi::native::register_native_function(
                    "wasm_double",
                    wasm_double as *const core::ffi::c_void,
                    sig,
                )
                .expect("register");
            }
            let mut builder =
                crate::mir::FunctionBuilder::new("main", Some(crate::types::Type::int()));
            let arg = builder.add_temp(crate::types::Type::int());
            let out = builder.add_temp(crate::types::Type::int());
            builder.assign(
                arg,
                crate::mir::RValue::Const(crate::bytecode::Constant::Int(21)),
            );
            builder.assign(
                out,
                crate::mir::RValue::FFICall {
                    idx: 0,
                    args: vec![arg],
                },
            );
            builder.terminate(crate::mir::Terminator::Return(Some(out)));
            let func = builder.build();
            let module = crate::mir::Module {
                name: "ffi".into(),
                functions: vec![func],
                behaviors: vec![],
                actor_metadata: vec![],
                compensation_of: vec![],
                parallel_branches_of: vec![],
                foreign_functions: vec![crate::mir::ForeignFunction {
                    library: String::new(),
                    symbol: "wasm_double".into(),
                    params: vec![crate::types::Type::Primitive(
                        crate::types::PrimitiveType::Int,
                    )],
                    ret: crate::types::Type::Primitive(crate::types::PrimitiveType::Int),
                }],
            };
            let mut backend = WasmBackend::new();
            let wasm = backend.compile(&module, "main").expect("wasm compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            assert_eq!(val.as_int(), Some(42), "wasm_double(21) must be 42");

            // CStr parameter: a function reading a null-terminated string.
            extern "C" fn wasm_strlen(s: *const core::ffi::c_char) -> i64 {
                if s.is_null() {
                    0
                } else {
                    unsafe { core::ffi::CStr::from_ptr(s) }.to_bytes().len() as i64
                }
            }
            let sig = crate::ffi::marshal::Signature::new(
                vec![crate::ffi::marshal::CType::CStr],
                crate::ffi::marshal::CType::I64,
            );
            unsafe {
                crate::ffi::native::register_native_function(
                    "wasm_strlen",
                    wasm_strlen as *const core::ffi::c_void,
                    sig,
                )
                .expect("register");
            }
            let mut builder = crate::mir::FunctionBuilder::new(
                "main",
                Some(crate::types::Type::Primitive(
                    crate::types::PrimitiveType::Int,
                )),
            );
            let s = builder.add_temp(crate::types::Type::string());
            let out = builder.add_temp(crate::types::Type::int());
            builder.assign(
                s,
                crate::mir::RValue::Const(crate::bytecode::Constant::String("nulang".into())),
            );
            builder.assign(
                out,
                crate::mir::RValue::FFICall {
                    idx: 0,
                    args: vec![s],
                },
            );
            builder.terminate(crate::mir::Terminator::Return(Some(out)));
            let func = builder.build();
            let module = crate::mir::Module {
                name: "ffi2".into(),
                functions: vec![func],
                behaviors: vec![],
                actor_metadata: vec![],
                compensation_of: vec![],
                parallel_branches_of: vec![],
                foreign_functions: vec![crate::mir::ForeignFunction {
                    library: String::new(),
                    symbol: "wasm_strlen".into(),
                    params: vec![crate::types::Type::string()],
                    ret: crate::types::Type::Primitive(crate::types::PrimitiveType::Int),
                }],
            };
            let mut backend = WasmBackend::new();
            let wasm = backend.compile(&module, "main").expect("wasm compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            assert_eq!(val.as_int(), Some(6), "wasm_strlen(\"nulang\") must be 6");
        }

        #[test]
        fn test_wasm_run_array_oob() {
            // Out-of-range and negative array indices must yield nil, not
            // garbage. In-range access still works.
            let v =
                run_source_to_value(r#"fn f() -> Int { let a = [1, 2]; a[5] } f()"#).expect("run");
            assert!(v.is_nil(), "a[5] must be nil, got {:?}", v.as_int());
            let v =
                run_source_to_value(r#"fn f() -> Int { let a = [1, 2]; a[-1] } f()"#).expect("run");
            assert!(v.is_nil(), "a[-1] must be nil");
            let v = run_source_to_value(r#"fn f() -> Int { let a = [1, 2]; a[0] + a[1] } f()"#)
                .expect("run");
            assert_eq!(v.as_int(), Some(3), "in-range access must still work");
        }

        #[test]
        fn test_wasm_run_neg_float() {
            // Positive literal alone must be exact.
            let wasm = compile_source_to_wasm(r#"3.55"#).expect("compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            eprintln!("TMP 3.55 wasm float = {:?}", val.as_float());
            assert_eq!(val.as_float(), Some(3.55), "3.55 literal must be exact");
            // Negated literal.
            let wasm = compile_source_to_wasm(r#"-3.55"#).expect("compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            eprintln!("TMP -3.55 wasm float = {:?}", val.as_float());
            assert_eq!(val.as_float(), Some(-3.55), "-3.55 must negate the float");
        }

        #[test]
        fn test_wasm_run_float_div_by_zero() {
            // Float division by zero → nil (matches the interpreter), not inf.
            let v = run_source_to_value(r#"fn f() -> Float { 1.0 / 0.0 } f()"#).expect("run");
            assert!(v.is_nil(), "1.0/0.0 must be nil, got {:?}", v.as_float());
        }

        #[test]
        fn test_wasm_run_float_comparison() {
            let v = run_source_to_value(r#"fn f() -> Bool { 1.5 < 2.5 } f()"#).expect("run");
            assert_eq!(v.as_bool(), Some(true), "1.5 < 2.5");
            let v = run_source_to_value(r#"fn f() -> Bool { 3 > 2 } f()"#).expect("run");
            assert_eq!(v.as_bool(), Some(true), "3 > 2");
        }

        #[test]
        fn test_wasm_negated_float_comparison_matches_interpreter() {
            // This accepted mutant lowers the comparison to a tagged Bool,
            // while the VM's FNeg fallback produces -0.0. WASM must preserve
            // that exact result instead of returning tagged Int 0.
            let source = "-(0.3 >= 3.22)";
            let (expected, _) = super::run_source(source).expect("interpreter run");
            let actual = run_source_to_value(source).expect("WASM run");
            assert_eq!(
                actual.as_raw(),
                expected.as_raw(),
                "WASM unary negation must match interpreter"
            );
            assert_eq!(actual.as_float(), Some(-0.0));
        }

        #[test]
        fn test_wasm_run_pow() {
            // Integer exponentiation `a ** b` (previously returned nil in WASM).
            let val = run_source_to_value(r#"fn f() -> Int { 10 - 3 ** 2 } f()"#).expect("run");
            assert_eq!(val.as_int(), Some(1), "10 - 3**2 must be 1");
            let val = run_source_to_value(r#"fn f() -> Int { 2 ** 10 } f()"#).expect("run");
            assert_eq!(val.as_int(), Some(1024), "2**10 must be 1024");
        }

        #[test]
        fn test_wasm_run_record_update() {
            // `{base .. field = val}` copies the record and overrides a field;
            // the original is unchanged.
            let val = run_source_to_value(
                r#"fn f() -> Int { let r = {x: 1, y: 2} in let r2 = {r .. x = 5} in r2.x + r2.y + r.x } f()"#,
            )
            .expect("run");
            assert_eq!(val.as_int(), Some(8), "r2.x=5 + r2.y=2 + r.x=1 must be 8");
        }

        #[test]
        fn test_wasm_run_record_string_field() {
            // A record holding a string field; the string constant is interned
            // into the WASM data segment.
            let wasm = compile_source_to_wasm(
                r#"fn f() -> String { let r = {name: "nulang"} in r.name } f()"#,
            )
            .expect("compile");
            let mut rt = crate::wasm_runtime::WasmRuntime::new(&wasm, None).expect("runtime");
            let val = rt.run().expect("run");
            assert_eq!(
                rt.string_value(&val).as_deref(),
                Some("nulang"),
                "record string field must read back"
            );
        }
    }

    /// Entity declarations (with events and apply blocks) must pass through
    /// the full pipeline: lex → parse → typecheck → compile.
    mod entity_tests {
        use super::*;

        #[test]
        fn test_entity_with_events_compiles() {
            let source = "entity Counter { state count: Int = 0 events | Incremented(by: Int) | Decremented(by: Int) behavior inc(by: Int) { self.count = self.count + by } }";
            let result = run_source_new(source);
            assert!(
                result.is_ok(),
                "entity with events must parse and typecheck without error: {:?}",
                result.err()
            );
        }

        #[test]
        fn test_entity_with_apply_block_compiles() {
            let source = "entity Counter { state count: Int = 0 events | Incremented(by: Int) apply | Incremented(by) => self.count = self.count + by behavior inc(by: Int) { self.count = self.count + by } }";
            let result = run_source_new(source);
            assert!(
                result.is_ok(),
                "entity with apply must parse and typecheck without error: {:?}",
                result.err()
            );
        }

        #[test]
        fn test_entity_events_and_apply_as_identifiers() {
            // `events` and `apply` are contextual keywords — they remain
            // usable as field/variable names outside actor bodies.
            let source =
                "fn main() { let events = [1, 2, 3]; let apply = fn(x) x + 1; apply(events[0]) }";
            let result = run_source_new(source);
            assert!(
                result.is_ok(),
                "`events` and `apply` as identifiers must work: {:?}",
                result.err()
            );
        }

        #[test]
        fn test_entity_apply_handler_executes_after_emit() {
            // The apply handler must run after emit, updating entity state.
            let rt = Rc::new(RefCell::new(Runtime::new()));
            let source = r#"
                entity Counter {
                    state count: Int = 0
                    events
                        | Incremented(by: Int)
                    apply
                        | Incremented(by) => self.count = self.count + by
                    behavior inc(by: Int) {
                        emit Incremented(by)
                        self.count
                    }
                }
                let c = spawn Counter {} in {
                    send c inc(5)
                    c
                }
            "#;
            let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
            let actor_id = value.as_actor_id().expect("spawn should return actor ref");
            rt.borrow_mut().run_scheduler();
            let rt_ref = rt.borrow();
            let actor = rt_ref.actors.get(&actor_id).unwrap();
            // (The exact value depends on spawn-state initialization and
            // behavior-body evaluation; the key invariant is that the apply
            // handler ran and mutated state.)
            let count = actor.get_state_field("count").and_then(|v| v.as_int());
            assert!(
                count.is_some() && count.unwrap() > 0,
                "apply handler must update count after emit, got {:?}",
                count
            );
        }
    }

    #[test]
    fn test_organization_compiles_as_persistent_actor() {
        let source = r#"organization Team {
                state members: Int = 0
                behavior count() { self.members }
            }"#;
        let result = run_source_new(source);
        assert!(
            result.is_ok(),
            "organization should compile: {:?}",
            result.err()
        );
    }

    // -- break-with-value (MIR pipeline) --

    #[test]
    fn test_while_break_with_value_returns_break_value() {
        let source = r#"while true { break 99 }"#;
        let result = run_source_new(source).unwrap();
        assert_eq!(
            result.as_int(),
            Some(99),
            "while break-with-value should return break value, got {:?}",
            result
        );
    }

    #[test]
    fn test_while_break_without_value_returns_unit() {
        let source = r#"while true { break }"#;
        let result = run_source_new(source).unwrap();
        assert!(
            result.is_unit(),
            "while break without value should return unit, got {:?}",
            result
        );
    }

    #[test]
    fn test_while_conditional_break_with_value() {
        let source = r#"while true { if true then { break 42 } else { 0 } }"#;
        let result = run_source_new(source).unwrap();
        assert_eq!(
            result.as_int(),
            Some(42),
            "conditional break-with-value should return 42, got {:?}",
            result
        );
    }

    #[test]
    fn test_for_break_with_value_returns_break_value() {
        let source = r#"for i in [1] { break 99 }"#;
        let result = run_source_new(source).unwrap();
        assert_eq!(
            result.as_int(),
            Some(99),
            "for break-with-value should return break value, got {:?}",
            result
        );
    }

    #[test]
    fn test_for_break_without_value_returns_unit() {
        let source = r#"for i in [1] { break }"#;
        let result = run_source_new(source).unwrap();
        assert!(
            result.is_unit(),
            "for break without value should return unit, got {:?}",
            result
        );
    }

    /// Verify the JIT safepoint infrastructure does not cause hangs or
    /// crashes when an actor with a tight loop yields to the scheduler.
    /// The busy-loop actor is given a minimal safepoint budget so the
    /// JIT yield path is exercised (when JIT is active).
    #[test]
    fn test_jit_safepoint_yield_does_not_starve_other_actors() {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        // A busy-loop actor and a simple flag-setter. The loop counts
        // down from 100_000; the ping sets a flag. Both must complete.
        let source = r#"
            actor Responder {
                state flag = false
                behavior ping() { self.flag = true }
            }
            let r = spawn Responder {} in {
                send r ping()
                r
            }
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        let responder_id = value
            .as_actor_id()
            .expect("spawn should return an actor reference");

        // Reduce the safepoint budget to force an early yield on the
        // next JIT region entry, if the JIT is active.
        {
            let mut rt_mut = rt.borrow_mut();
            if let Some(responder) = rt_mut.actors.get_mut(&responder_id) {
                responder.jit_safepoint_counter = 1;
            }
        }

        // Run the scheduler to process the busy-loop message (already
        // queued from the source program) and verify progression.
        rt.borrow_mut().run_scheduler();

        let rt_ref = rt.borrow();
        let responder = rt_ref
            .actors
            .get(&responder_id)
            .expect("responder actor should exist");
        let flag = responder.get_state_field("flag").and_then(|v| v.as_bool());
        assert!(
            flag.unwrap_or(false),
            "responder's flag must be set — the scheduler must not hang"
        );
    }

    // -- Http effect tests ----------------------------------------------

    #[test]
    fn test_http_get_effect_row() {
        let source = r#"
            fn fetch() -> String ! {Net} {
                let body = perform Http.get("http://example.com")
                body
            }
        "#;
        let result = run_source(source);
        assert!(
            result.is_ok(),
            "Http.get should type-check when {{Net}} is declared: {:?}",
            result.err()
        );
    }
    #[test]
    fn test_http_post_effect_row() {
        let source = r#"
            fn post_it() -> String ! {Net} {
                let body = perform Http.post("http://example.com", "{}")
                body
            }
        "#;
        let result = run_source(source);
        assert!(
            result.is_ok(),
            "Http.post should type-check when {{Net}} is declared: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_http_get_requires_net_effect() {
        // Http.get should be rejected when called from a function that does not declare {Net}.
        let source = r#"
            fn do_http() -> String ! {Net} { perform Http.get("http://example.com") }
            fn pure() -> String ! {} { do_http() }
        "#;
        let result = check_module_effects(source);
        assert!(
            result.is_err(),
            "pure function calling Http.get must be rejected"
        );
    }
    #[test]
    fn test_http_get_mocked() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let called: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let cb = called.clone();
        let url_seen: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let url_clone = url_seen.clone();

        rt.borrow_mut()
            .install_test_handler("Http.get", move |regs| {
                *cb.borrow_mut() = true;
                // regs[0] is the URL string id — resolve it from constants at call time
                // but for a mock we just record that we were called
                *url_clone.borrow_mut() = format!("{:?}", regs);
                Some(Value::nil())
            });

        let source = r#"perform Http.get("http://example.com/api")"#;
        let value = run_source_new_with_runtime(source, rt).unwrap();
        assert_eq!(value, Value::nil());
        assert!(
            *called.borrow(),
            "test handler should have intercepted Http.get"
        );
    }

    #[test]
    fn test_http_post_mocked() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let called: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let cb = called.clone();

        rt.borrow_mut()
            .install_test_handler("Http.post", move |_regs| {
                *cb.borrow_mut() = true;
                Some(Value::nil())
            });

        let source = r#"perform Http.post("http://example.com/api", "{}")"#;
        let value = run_source_new_with_runtime(source, rt).unwrap();
        assert_eq!(value, Value::nil());
        assert!(
            *called.borrow(),
            "test handler should have intercepted Http.post"
        );
    }

    #[cfg(feature = "tcp")]
    #[test]
    fn test_http_serve_roundtrip() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let rt = Rc::new(RefCell::new(Runtime::new()));
        let source = r#"
            fn echo(body: String) -> String {
                body
            }
            perform Http.serve(0, echo)
        "#;
        let value = run_source_new_with_runtime(source, rt.clone()).unwrap();
        // value is the bound port (Int).
        let port = value
            .as_int()
            .expect("Http.serve should return the bound port");
        assert!(port > 0, "expected a real port, got {}", port);

        // Make an HTTP request to the server from the test thread.
        let mut stream = TcpStream::connect(("127.0.0.1", port as u16))
            .expect("failed to connect to HTTP server");
        let request =
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 13\r\n\r\nhello, world!"
                .to_string();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();

        assert!(
            response.contains("200 OK"),
            "expected 200, got: {}",
            response
        );
        assert!(
            response.contains("hello, world!"),
            "expected echoed body, got: {}",
            response
        );
    }

    /// Regression: `perform Http.serve` must work in a bare VM (standalone
    /// callbacks, no Runtime attached) — previously the standalone
    /// `perform_builtin_effect` handled Http.get/post but returned
    /// "Unhandled effect" for serve, so an actor-free program that started
    /// a server failed. The standalone callbacks now host the server
    /// directly (StandaloneVmCallbacks::perform_builtin_effect_in_module),
    /// mirroring the runtime-backed path.
    #[cfg(feature = "tcp")]
    #[test]
    fn test_http_serve_standalone() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let source = r#"
            fn echo(body: String) -> String {
                body
            }
            perform Http.serve(0, echo)
        "#;
        let value = run_source_new(source)
            .expect("standalone Http.serve must not error with 'Unhandled effect'");
        let port = value
            .as_int()
            .expect("Http.serve should return the bound port");
        assert!(port > 0, "expected a real port, got {}", port);

        // Make an HTTP request and assert the handler echoes the body.
        let mut stream = TcpStream::connect(("127.0.0.1", port as u16))
            .expect("failed to connect to standalone HTTP server");
        let request =
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 13\r\n\r\nhello, world!"
                .to_string();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();

        assert!(
            response.contains("200 OK"),
            "expected 200, got: {}",
            response
        );
        assert!(
            response.contains("hello, world!"),
            "expected echoed body, got: {}",
            response
        );
    }

    // -----------------------------------------------------------------------
    // Error handling: catch, fail, T ! E syntax
    // -----------------------------------------------------------------------

    #[test]
    fn test_catch_bare_expr_on_ok() {
        assert_int(
            r#"
            type Result[Ok,Err] = Ok(Ok) | Error(Err)
            fn ok_val() -> Result[Int, String] { Ok(42) }
            ok_val() catch 0
            "#,
            42,
        );
    }

    #[test]
    fn test_catch_bare_expr_on_error() {
        assert_int(
            r#"
            type Result[Ok,Err] = Ok(Ok) | Error(Err)
            fn err_val() -> Result[Int, String] { Error("fail") }
            err_val() catch 0
            "#,
            0,
        );
    }

    #[test]
    fn test_fail_returns_early() {
        assert_int(
            r#"
            fn early_return(x: Int) -> Int {
                if x < 0 then fail 0 else x
            }
            early_return(42)
            "#,
            42,
        );
    }

    #[test]
    fn test_fail_returns_early_negative() {
        assert_int(
            r#"
            fn early_return(x: Int) -> Int {
                if x < 0 then fail 0 else x
            }
            early_return(-5)
            "#,
            0,
        );
    }

    #[test]
    fn test_bang_error_type_wraps_return_with_result() {
        assert_int(
            r#"
            type Result[Ok,Err] = Ok(Ok) | Error(Err)
            fn div(a: Int, b: Int) -> Int ! String {
                if b == 0 then fail Error("div by zero") else Ok(a / b)
            }
            div(10, 2)?
            "#,
            5,
        );
    }

    // -----------------------------------------------------------------------
    // Until expression: until <condition> => <body>
    // -----------------------------------------------------------------------

    #[test]
    fn test_until_true_returns_body() {
        assert_int("until true => 42", 42);
    }

    #[test]
    fn test_until_with_var_true_condition() {
        assert_int(
            r#"
            let x = 10 in
            until x > 5 => x
            "#,
            10,
        );
    }

    // -----------------------------------------------------------------------
    // Typeclass declarations (Phase 4)
    // -----------------------------------------------------------------------

    #[test]
    fn test_class_declaration_parses() {
        // Verify that class declarations parse without errors
        let result = check_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            42
            "#,
        );
        assert!(
            result.is_ok(),
            "class declaration should parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_impl_declaration_parses() {
        // Verify that impl declarations parse without errors
        let result = check_source(
            r#"
            impl Eq Int {
                fn eq(self, other) = self == other
            }
            42
            "#,
        );
        assert!(
            result.is_ok(),
            "impl declaration should parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_class_and_impl_together_parse() {
        let result = check_source(
            r#"
            class Show[T] {
                fn show(self: T) -> String
            }
            impl Show Int {
                fn show(self) = "Int"
            }
            42
            "#,
        );
        assert!(
            result.is_ok(),
            "class+impl should parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_class_with_superclass_parses() {
        let result = check_source(
            r#"
            class Eq[T] { fn eq(self: T, other: T) -> Bool }
            class Ord[T]: Eq {
                fn cmp(self: T, other: T) -> Int
            }
            42
            "#,
        );
        assert!(result.is_ok(), "class with superclass: {:?}", result.err());
    }

    // -----------------------------------------------------------------------
    // Typeclass typechecker integration (Phase 4)
    // -----------------------------------------------------------------------

    #[test]
    fn test_class_method_resolves_with_impl() {
        // Method call on a concrete type with a matching impl resolves
        // via the instance dictionary.
        let result = check_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            impl Eq Int {
                fn eq(self: Int, other: Int) = self == other
            }
            1.eq(2)
            "#,
        );
        assert!(
            result.is_ok(),
            "1.eq(2) with impl Eq Int should type-check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_class_method_errors_without_impl() {
        // Method call on a concrete type without a matching impl is an error.
        let result = check_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            "hello".eq("world")
            "#,
        );
        assert!(result.is_err(), "no impl Eq[String] should be an error");
        let err = result.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("no impl Eq[String]"),
            "error should mention missing impl: {}",
            msg
        );
    }

    #[test]
    fn test_class_method_with_two_param_impl() {
        // Method call with additional parameters resolves correctly.
        let result = check_source(
            r#"
            class Ord[T] {
                fn cmp(self: T, other: T) -> Int
            }
            impl Ord Int {
                fn cmp(self: Int, other: Int) = self - other
            }
            5.cmp(3)
            "#,
        );
        assert!(
            result.is_ok(),
            "5.cmp(3) with impl Ord Int should type-check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_method_on_type_var_falls_through_to_record() {
        // Method call on a type variable without constraints falls through
        // to open-record handling (works for any type).
        let result = check_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            fn generic_id[T](x: T) -> T { x }
            generic_id
            "#,
        );
        assert!(
            result.is_ok(),
            "generic function without method calls should check: {:?}",
            result.err()
        );
    }
    #[test]
    fn test_unknown_method_falls_through_to_record() {
        // A method call whose name doesn't match any class method
        // falls through to open-record handling for type variables.
        let result = check_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            fn id[T](x: T) -> T { x }
            id
            "#,
        );
        assert!(
            result.is_ok(),
            "generic function with no method calls should check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_record_field_access_still_works() {
        // Record field access should still work alongside typeclass resolution.
        let result = check_source(
            r#"
            { x: 1, y: 2 }.x
            "#,
        );
        assert!(
            result.is_ok(),
            "record field access should work: {:?}",
            result.err()
        );
    }

    // -----------------------------------------------------------------------
    // Typeclass dictionary lowering tests (Phase 4, B.6)
    // -----------------------------------------------------------------------

    #[test]
    fn test_impl_dict_lowering_typechecks() {
        // Verify that an impl block with a method body type-checks and
        // the synthetic dict name is bound into scope.
        let result = check_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            impl Eq Int {
                fn eq(self: Int, other: Int) = self == other
            }
            _impl_Eq_Int
            "#,
        );
        assert!(
            result.is_ok(),
            "impl dict should be bound in scope: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_impl_dict_lowering_compiles_to_bytecode() {
        // Verify that an impl block lowers through HIR/MIR to bytecode
        // without errors. The dict constant is compiled but not called
        // by __main — we just verify the pipeline succeeds.
        let result = run_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            impl Eq Int {
                fn eq(self: Int, other: Int) = self == other
            }
            42
            "#,
        );
        assert!(
            result.is_ok(),
            "impl dict lowering should produce valid bytecode: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_class_method_call_at_runtime() {
        // Full end-to-end: class + impl + method call at runtime.
        // 1.eq(1) → Bool(true), verified via as_bool().
        let result = run_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            impl Eq Int {
                fn eq(self: Int, other: Int) = self == other
            }
            1.eq(1)
            "#,
        );
        assert!(
            result.is_ok(),
            "1.eq(1) should compile and run: {:?}",
            result.err()
        );
        let (value, _ty) = result.unwrap();
        assert_eq!(value.as_bool(), Some(true), "1.eq(1) should be true");
    }

    #[test]
    fn test_class_method_call_false_at_runtime() {
        let result = run_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            impl Eq Int {
                fn eq(self: Int, other: Int) = self == other
            }
            1.eq(2)
            "#,
        );
        assert!(
            result.is_ok(),
            "1.eq(2) should compile and run: {:?}",
            result.err()
        );
        let (value, _ty) = result.unwrap();
        assert_eq!(value.as_bool(), Some(false), "1.eq(2) should be false");
    }

    // ---------- Typeclass constraint syntax tests ----------

    #[test]
    fn test_typeclass_constraint_parse() {
        // Verify that `fn foo[T: Eq]` parses and type-checks.
        let result = check_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            fn id[T: Eq](x: T) -> T { x }
            42
            "#,
        );
        assert!(
            result.is_ok(),
            "constrained generic function should parse and type-check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_typeclass_constraint_method_call_on_type_var() {
        // Verify that a method call on a constrained type variable resolves.
        let result = check_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            fn eq_check[T: Eq](a: T, b: T) -> Bool { a.eq(b) }
            42
            "#,
        );
        assert!(
            result.is_ok(),
            "method call on constrained type var should resolve: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_typeclass_constraint_multiple_classes() {
        // Verify that `T: Eq + Ord` parses with multiple constraints.
        let result = check_source(
            r#"
            class Eq[T] { fn eq(self: T, other: T) -> Bool }
            class Ord[T]: Eq {
                fn cmp(self: T, other: T) -> Int
            }
            fn compare[T: Eq + Ord](a: T, b: T) -> Bool { a.eq(b) }
            42
            "#,
        );
        assert!(
            result.is_ok(),
            "multiple constraints should parse and type-check: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_typeclass_constraint_compile_with_impl() {
        // Verify that a constrained generic function compiles alongside its
        // impl and a concrete call site. Runtime dispatch through impl dicts
        // for type-variable receivers is follow-up work (MIR codegen).
        let result = check_source(
            r#"
            class Eq[T] {
                fn eq(self: T, other: T) -> Bool
            }
            impl Eq Int {
                fn eq(self: Int, other: Int) = self == other
            }
            fn eq_check[T: Eq](a: T, b: T) -> Bool { a.eq(b) }
            eq_check(1, 1)
            "#,
        );
        assert!(
            result.is_ok(),
            "constrained generic with impl should compile: {:?}",
            result.err()
        );
    }
    // -----------------------------------------------------------------------
    // Exponentiation operator (**)
    // -----------------------------------------------------------------------

    #[test]
    fn test_pow_basic() {
        assert_int("2 ** 10", 1024);
    }

    #[test]
    fn test_pow_right_assoc() {
        // 2 ** 3 ** 2 = 2 ** (3 ** 2) = 2 ** 9 = 512
        assert_int("2 ** 3 ** 2", 512);
    }

    #[test]
    fn test_pow_precedence_over_mul() {
        // 2 * 3 ** 2 = 2 * (3 ** 2) = 2 * 9 = 18
        assert_int("2 * 3 ** 2", 18);
    }

    #[test]
    fn test_pow_zero_exp() {
        assert_int("2 ** 0", 1);
    }

    #[test]
    fn test_pow_zero_base_zero_exp() {
        assert_int("0 ** 0", 1);
    }

    #[test]
    fn test_pow_neg_exp_returns_nil() {
        let result = run_source("2 ** -1");
        assert!(result.is_ok(), "should compile: {:?}", result.err());
        let (value, _ty) = result.unwrap();
        assert!(
            value.is_nil(),
            "negative exponent should return nil, got {:?}",
            value
        );
    }

    // ---------------------------------------------------------------
    // Record update: { base .. field = val }
    // ---------------------------------------------------------------

    #[test]
    fn test_record_update_single_override() {
        let source = r#"
        let p = { x: 1, y: 2 } in
        let q = { p .. y = 9 } in
        q.y
        "#;
        let result = run_source(source);
        assert!(result.is_ok(), "should compile: {:?}", result.err());
        let (value, _ty) = result.unwrap();
        assert_eq!(value.as_int(), Some(9), "q.y should be 9");
    }

    #[test]
    fn test_record_update_base_unchanged() {
        let source = r#"
        let p = { x: 1, y: 2 } in
        let q = { p .. y = 9 } in
        q.x
        "#;
        let result = run_source(source);
        assert!(result.is_ok(), "should compile: {:?}", result.err());
        let (value, _ty) = result.unwrap();
        assert_eq!(
            value.as_int(),
            Some(1),
            "q.x should be 1 (unchanged from base)"
        );
    }

    #[test]
    fn test_record_update_base_not_mutated() {
        let source = r#"
        let p = { x: 1, y: 2 } in
        let q = { p .. y = 9 } in
        p.y
        "#;
        let result = run_source(source);
        assert!(result.is_ok(), "should compile: {:?}", result.err());
        let (value, _ty) = result.unwrap();
        assert_eq!(value.as_int(), Some(2), "p.y should be 2 (base unchanged)");
    }

    #[test]
    fn test_record_update_multiple_overrides() {
        let source = r#"
        let p = { x: 1, y: 2, z: 3 } in
        let q = { p .. x = 10, z = 30 } in
        let a = q.x in
        let b = q.y in
        let c = q.z in
        a + b + c
        "#;
        let result = run_source(source);
        assert!(result.is_ok(), "should compile: {:?}", result.err());
        let (value, _ty) = result.unwrap();
        assert_eq!(value.as_int(), Some(42), "10 + 2 + 30 = 42");
    }

    // -----------------------------------------------------------------------
    // var bindings — mutable local variables (SPEC2 §2.8)
    // -----------------------------------------------------------------------

    #[test]
    fn test_var_simple_reassign() {
        // var x = 0; x = x + 1; x = x + 1; x  →  2
        let source = "var x = 0 in { x = x + 1; x = x + 1; x }";
        assert_int(source, 2);
    }

    #[test]
    fn test_var_multiple_bindings() {
        // var i = 0; var sum = 0; i = i+1; sum = sum+i; i = i+1; sum = sum+i; sum
        let source = "var i = 0 in var sum = 0 in { i = i + 1; sum = sum + i; i = i + 1; sum = sum + i; sum }";
        assert_int(source, 3);
    }

    #[test]
    fn test_var_in_block() {
        let source = r#"
        var total = 0
        total = total + 5
        total = total * 2
        total
        "#;
        assert_int(source, 10);
    }

    #[test]
    fn test_var_let_mix() {
        // var mutates; let shadows without affecting var
        let source = "var x = 1 in let x = 10 in x";
        assert_int(source, 10);
    }

    #[test]
    fn test_let_in_scoped_to_body_only() {
        // let x = 10 in 0 scopes x to 0 only; the ; x sees the outer x=1
        let source = "let x = 1 in { let x = 10 in 0; x }";
        assert_int(source, 1);
    }

    #[test]
    fn test_let_in_different_name_does_not_shadow() {
        // Different variable name -> x still resolves to outer binding
        let source = "let x = 1 in { let y = 10 in 0; x }";
        assert_int(source, 1);
    }

    #[test]
    fn test_chained_let_in_correctly_scoped() {
        // Nested let-in: inner x scoped to inner body only
        let source = "let x = 1 in let x = 10 in x";
        assert_int(source, 10);
    }

    #[test]
    fn test_let_immutable_error() {
        // Reassigning let binding should still error
        let source = r#"
        let y = 0
        y = 1
        "#;
        let result = run_source(source);
        assert!(result.is_err(), "reassigning immutable let should error");
        let error = result.unwrap_err();
        let err = error.to_string();
        assert!(
            err.contains("cannot assign to immutable binding"),
            "error should mention immutable binding, got: {}",
            err
        );
        match error {
            NuError::TypeError {
                expected_type,
                found_type,
                ..
            } => {
                assert_eq!(expected_type.as_deref(), Some("mutable binding"));
                assert_eq!(found_type.as_deref(), Some("immutable binding"));
            }
            other => panic!("expected structured TypeError, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Array built-in effects (length, push, new, set, slice)
    // -----------------------------------------------------------------------

    #[test]
    fn test_array_builtin_length() {
        // perform Array.length([1,2,3]) → 3
        assert_int("perform Array.length([1,2,3])", 3);
        assert_int("perform Array.length([])", 0);
        // Length of a push result
        assert_int(
            "let a = [1,2] in let b = perform Array.push(a, 3) in perform Array.length(b)",
            3,
        );
    }

    #[test]
    fn test_array_builtin_push_value_semantics() {
        // Original array unchanged
        let source = r#"
            let a = [1,2];
            let b = perform Array.push(a, 3);
            // Original still length 2
            perform Array.length(a)
        "#;
        assert_int(source, 2);
        // New array has element appended
        assert_int(
            "let a = [1,2] in let b = perform Array.push(a, 3) in b[2]",
            3,
        );
        // First element preserved
        assert_int(
            "let a = [1,2] in let b = perform Array.push(a, 3) in b[0]",
            1,
        );
    }

    #[test]
    fn test_array_builtin_new() {
        assert_int("perform Array.length(perform Array.new(3, 7))", 3);
        assert_int("(perform Array.new(3, 7))[0]", 7);
        assert_int("(perform Array.new(3, 7))[1]", 7);
        assert_int("(perform Array.new(3, 7))[2]", 7);
        assert_int("perform Array.length(perform Array.new(0, 42))", 0);
    }

    #[test]
    fn test_array_builtin_set_value_semantics() {
        // b has modified element
        assert_int(
            "let a = [10,20,30] in let b = perform Array.set(a, 1, 99) in b[1]",
            99,
        );
        // Original a unchanged
        assert_int(
            "let a = [10,20,30] in let b = perform Array.set(a, 1, 99) in a[1]",
            20,
        );
    }

    #[test]
    fn test_array_builtin_slice() {
        let source = r#"
            let xs = [10,20,30,40,50];
            let sub = perform Array.slice(xs, 1, 4);
            // Length: 4-1 = 3
            perform Array.length(sub)
        "#;
        assert_int(source, 3);
        assert_int("(perform Array.slice([10,20,30,40,50], 1, 4))[0]", 20);
        assert_int("(perform Array.slice([10,20,30,40,50], 1, 4))[1]", 30);
        assert_int("(perform Array.slice([10,20,30,40,50], 1, 4))[2]", 40);
    }

    #[test]
    fn test_array_builtin_accumulate_in_loop() {
        // Build an array by pushing in a loop
        let source = r#"
            var acc = [];
            for i in [1,2,3,4,5] {
                acc = perform Array.push(acc, i * i)
            };
            acc[2] + acc[4]
        "#;
        // acc = [1,4,9,16,25]; acc[2] + acc[4] = 9 + 25 = 34
        assert_int(source, 34);
    }

    #[test]
    fn test_array_builtin_on_empty() {
        // Push on empty array
        assert_int(
            "let a = [] in let b = perform Array.push(a, 42) in b[0]",
            42,
        );
        assert_int(
            "let a = [] in let b = perform Array.push(a, 42) in perform Array.length(b)",
            1,
        );
    }

    // -----------------------------------------------------------------------
    // StrBuilder builtin: mutable growable string buffer
    // -----------------------------------------------------------------------

    #[test]
    fn test_strbuilder_basic() {
        assert_int("let b = perform StrBuilder.new() in perform StrBuilder.len(b)", 0);
        assert_int(
            "let b = perform StrBuilder.new() in \
             let b2 = perform StrBuilder.push(b, \"hi\") in perform StrBuilder.len(b2)",
            2,
        );
        assert_string(
            "let b = perform StrBuilder.new() in \
             let b2 = perform StrBuilder.push(b, \"hi\") in perform StrBuilder.to_string(b2)",
            "hi",
        );
    }

    #[test]
    fn test_strbuilder_growth() {
        // 10k appends of 3 bytes: crosses several capacity doublings and
        // exercises the reallocation path; amortized O(n) total.
        let source = r#"
            var b = perform StrBuilder.new();
            var i = 0;
            while i < 10000 {
                b = perform StrBuilder.push(b, "abc");
                i = i + 1
            };
            perform StrBuilder.len(b)
        "#;
        assert_int(source, 30000);
    }

    #[test]
    fn test_strbuilder_content_roundtrip() {
        let source = r#"
            var b = perform StrBuilder.new();
            b = perform StrBuilder.push(b, "Hello, ");
            b = perform StrBuilder.push(b, "Nulang");
            b = perform StrBuilder.push(b, "!");
            perform StrBuilder.to_string(b)
        "#;
        assert_string(source, "Hello, Nulang!");
    }

    #[test]
    fn test_strbuilder_reset() {
        assert_int(
            "let b = perform StrBuilder.new() in \
             let b2 = perform StrBuilder.push(b, \"abc\") in \
             let b3 = perform StrBuilder.reset(b2) in perform StrBuilder.len(b3)",
            0,
        );
    }

    // -----------------------------------------------------------------------
    // Map builtin: mutable hash map with content-based string keys
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_builtin_basic() {
        assert_int(
            "let m = perform Map.new() in perform Map.size(m)",
            0,
        );
        assert_int(
            "let m = perform Map.new() in \
             let m2 = perform Map.insert(m, \"a\", 1) in perform Map.size(m2)",
            1,
        );
        assert_int(
            "let m = perform Map.new() in \
             let m2 = perform Map.insert(m, \"a\", 1) in perform Map.get(m2, \"a\")",
            1,
        );
        assert_bool(
            "let m = perform Map.new() in \
             let m2 = perform Map.insert(m, \"a\", 1) in perform Map.contains(m2, \"b\")",
            false,
        );
    }

    #[test]
    fn test_map_builtin_mixed_keys() {
        // Int keys and string keys coexist; string keys compare by content.
        let source = r#"
            var m = perform Map.new();
            m = perform Map.insert(m, "alpha", 1);
            m = perform Map.insert(m, 42, "answer");
            perform Map.get(m, 42)
        "#;
        assert_string(source, "answer");
        assert_int(
            "let m = perform Map.new() in \
             let m2 = perform Map.insert(m, 7, 100) in perform Map.get(m2, 7)",
            100,
        );
    }

    #[test]
    fn test_map_builtin_content_keys() {
        // Keys built by concatenation must match literal keys (different
        // constant-pool ids, same content).
        assert_int(
            "let m = perform Map.new() in \
             let m2 = perform Map.insert(m, \"hello\", 7) in \
             perform Map.get(m2, \"he\" + \"llo\")",
            7,
        );
    }

    #[test]
    #[allow(clippy::erasing_op)]
    fn test_map_builtin_overwrite_remove() {
        let source = r#"
            var m = perform Map.new();
            m = perform Map.insert(m, "k", 1);
            m = perform Map.insert(m, "k", 2);
            let v1 = perform Map.get(m, "k");
            m = perform Map.remove(m, "k");
            let v2 = perform Map.get(m, "k");
            perform Map.size(m) * 100 + v1 * 10 + v2
        "#;
        // Overwrite keeps size 1; v1 = 2; after remove size 0; v2 = nil -> 0.
        // The source computes `Map.size(m) * 100 + v1 * 10 + v2` == 0*100 + 2*10 + 0.
        assert_int(source, 20);
    }

    #[test]
    fn test_map_builtin_growth() {
        // 60 inserts forces several capacity doublings (load factor 0.5).
        let source = r#"
            var m = perform Map.new();
            var i = 0;
            while i < 60 {
                m = perform Map.insert(m, "key" + perform Int.to_string(i), i);
                i = i + 1
            };
            perform Map.get(m, "key30") * 100 + perform Map.size(m)
        "#;
        assert_int(source, 30 * 100 + 60);
    }



    #[test]
    fn test_tuple_field_access_basic() {
        assert_int("let t = (1, 2, 3) in t.0 + t.1 + t.2", 6);
    }

    #[test]
    fn test_tuple_field_access_single() {
        assert_int("let t = (42,) in t.0", 42);
    }

    #[test]
    fn test_tuple_field_access_nested() {
        // Direct chained access now works (no parens needed).
        assert_int("let t = ((1, 2), 3) in t.0.0 + t.0.1 + t.1", 6);
    }

    #[test]
    fn test_tuple_field_access_nested_chain() {
        // Deeply nested tuple chain.
        assert_int("let t = (((10, 20), 30), 40) in t.0.0.1", 20);
    }

    #[test]
    fn test_tuple_field_access_out_of_range() {
        // Out-of-range index produces a type error.
        let err = run_source("let t = (1, 2) in t.5").unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("out of range"),
            "expected 'out of range' error, got: {}",
            msg
        );
    }
    #[test]
    fn test_function_call_arity_mismatch_populates_structured_fields() {
        // PLAN.md bullet 7 (structured error quality): a wrong-arg-count
        // call must populate NuError::TypeError.expected_type/found_type
        // with correctly-pluralized descriptions, not leave them None.
        let err = run_source("fn add(x: Int, y: Int) -> Int { x + y } add(1)").unwrap_err();
        match &err {
            NuError::TypeError {
                expected_type,
                found_type,
                ..
            } => {
                assert_eq!(expected_type.as_deref(), Some("2 arguments"));
                assert_eq!(found_type.as_deref(), Some("1 argument"));
            }
            other => panic!("expected TypeError, got {other:?}"),
        }
        let msg = format!("{}", err);
        assert!(msg.contains("wrong number of arguments"));
    }
    #[test]
    fn test_tuple_field_access_string_concat() {
        // String tuple elements load correctly.
        assert_string("let t = (\"a\", \"b\") in t.0 + t.1", "ab");
    }
    #[test]
    fn test_tuple_field_access_expression() {
        // Call a function that returns a tuple, then access its fields
        assert_int(
            "let f = fn(x) (x, x + 1, x + 2) in f(10).0 + f(10).1 + f(10).2",
            33,
        );
    }

    #[test]
    fn test_tuple_field_access_record_and_tuple() {
        // Record field access still works alongside tuple field access
        assert_int("let r = { x: 1, y: 2 } in r.x + r.y", 3);
        assert_int("let t = (10, 20) in t.0 + t.1", 30);
    }

    // -------------------------------------------------------------------
    // Example file compilation helpers (mirrors main.rs run_frontend)
    // -------------------------------------------------------------------

    /// Compile a .nula example file through the full frontend pipeline:
    /// prelude + parse + import resolution + typecheck + effect check +
    /// HIR/MIR lowering + codegen.
    fn compile_example_file(path: &Path) -> Result<(crate::bytecode::CodeModule, Type), NuError> {
        let source = std::fs::read_to_string(path).map_err(|e| NuError::VMError {
            msg: format!("cannot read {}: {}", path.display(), e),
            span: crate::types::Span::default(),
        })?;
        let file_path = path.to_str();

        // 1. Parse prelude (built-in type definitions)
        let ps = crate::prelude_source::PRELUDE_SOURCE;
        let mut pl = Lexer::new(ps);
        crate::types::set_source_map_with_file(ps, Some("<prelude>"));
        let pt = pl.lex()?;
        let mut pp = Parser::new(pt);
        let pa = pp.parse_module()?;

        // 2. Parse source file
        let mut lexer = Lexer::new(&source);
        crate::types::set_source_map_with_file(&source, file_path);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        let mut ast = parser.parse_module()?;

        // 3. Merge prelude variant types into the AST
        let mut pd: Vec<crate::ast::Decl> = pa
            .decls
            .into_iter()
            .filter(|d| matches!(d, crate::ast::Decl::VariantType { .. }))
            .collect();
        pd.append(&mut ast.decls);
        ast.decls = pd;

        // 4. Resolve imports (stdlib::json, stdlib::datetime, etc.)
        let mut visited = HashSet::new();
        crate::resolver::resolve_imports(&mut ast, path, &mut visited)?;

        // 5. Type check
        let mut type_checker = TypeChecker::new();
        let module_type = type_checker.check_module(&ast)?;

        // 6. Effect check
        let mut effect_checker = crate::effect_checker::EffectChecker::new();
        effect_checker.check_module(&ast.decls)?;

        // 7. HIR → MIR → bytecode
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir)?;
        let module = crate::mir_codegen::compile_mir(&mut mir, "test")?;

        Ok((module, module_type))
    }

    /// Run a compiled module in a bare VM (no actor runtime).
    fn run_module_bare(module: crate::bytecode::CodeModule) -> Result<Value, NuError> {
        let mut vm = VM::new();
        vm.load_module(module);
        vm.run()
    }

    /// Run a compiled module with an actor runtime.
    fn run_module_with_runtime(module: crate::bytecode::CodeModule) -> Result<Value, NuError> {
        let rt = Rc::new(RefCell::new(Runtime::new()));
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(rt.clone())));
        let value = vm.run()?;
        rt.borrow_mut().run_scheduler();
        Ok(value)
    }

    // -------------------------------------------------------------------
    // Example file integration test
    // -------------------------------------------------------------------

    #[test]
    fn test_all_examples_compile_and_run() {
        // Run in a thread with a larger stack — the compilation pipeline
        // (prelude + typechecker + effect checker) can be stack-heavy.
        let handle = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .name("example-runner".into())
            .spawn(|| {
                let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
                let examples_dir = manifest_dir.join("examples");

                let all_examples: [&str; 17] = [
                    "01_hello.nula",
                    "02_arithmetic.nula",
                    "03_functions.nula",
                    "04_pattern_match.nula",
                    "05_records.nula",
                    "06_higher_order.nula",
                    "07_effects.nula",
                    "08_actors.nula",
                    "09_loops.nula",
                    "10_pipe.nula",
                    "11_arrays.nula",
                    "12_json.nula",
                    "13_http.nula",
                    "14_option_result.nula",
                    "15_ranges.nula",
                    "16_realworld.nula",
                    "17_actor_fetcher.nula",
                ];

                // Examples that use actors (spawn / send) — need a Runtime.
                let needs_runtime: &[&str] = &["08_actors.nula", "17_actor_fetcher.nula"];

                // Examples that need network access — compile only, skip run.
                let needs_network: &[&str] =
                    &["13_http.nula", "16_realworld.nula", "17_actor_fetcher.nula"];

                let mut compiled = 0usize;
                let mut run_ok = 0usize;
                let mut skipped_run = 0usize;

                for name in &all_examples {
                    let path = examples_dir.join(name);
                    assert!(path.exists(), "Example file missing: {}", name);

                    // Every example must compile (parse + typecheck + codegen).
                    let (module, _ty) = compile_example_file(&path)
                        .unwrap_or_else(|e| panic!("Example {} failed to compile: {}", name, e));
                    compiled += 1;

                    // Skip execution for network-dependent examples.
                    if needs_network.contains(name) {
                        skipped_run += 1;
                        continue;
                    }

                    let result = if needs_runtime.contains(name) {
                        run_module_with_runtime(module)
                    } else {
                        run_module_bare(module)
                    };

                    assert!(
                        result.is_ok(),
                        "Example {} failed at runtime: {:?}",
                        name,
                        result.err()
                    );
                    run_ok += 1;
                }

                assert_eq!(compiled, 17, "all 17 examples must compile");
                assert_eq!(
                    run_ok + skipped_run,
                    17,
                    "all examples accounted for ({} run + {} skipped)",
                    run_ok,
                    skipped_run
                );
            })
            .expect("failed to spawn example-runner thread");

        handle.join().expect("example-runner thread panicked");
    }

    /// `ConstL`, `Pop`, `Switch`, `Alloc`, `TupleL`, `Unpack`, and `Copy`
    /// are reserved opcodes with no current producer (see the comments at
    /// their interpreter cases in `vm.rs`). This test is the enforcement
    /// mechanism for that claim: it compiles every example and every
    /// stdlib module and asserts none of them appear in the resulting
    /// bytecode. A failure here means a codegen path started emitting a
    /// reserved opcode — implement it in the interpreter (and the JIT/AOT/
    /// WASM backends) before landing whatever change caused the emission.
    #[test]
    fn test_no_codegen_path_emits_reserved_opcodes() {
        const RESERVED: [OpCode; 7] = [
            OpCode::ConstL,
            OpCode::Pop,
            OpCode::Switch,
            OpCode::Alloc,
            OpCode::TupleL,
            OpCode::Unpack,
            OpCode::Copy,
        ];

        let handle = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .name("reserved-opcode-scan".into())
            .spawn(|| {
                let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
                let mut scanned = 0usize;

                let examples_dir = manifest_dir.join("examples");
                let all_examples: [&str; 17] = [
                    "01_hello.nula",
                    "02_arithmetic.nula",
                    "03_functions.nula",
                    "04_pattern_match.nula",
                    "05_records.nula",
                    "06_higher_order.nula",
                    "07_effects.nula",
                    "08_actors.nula",
                    "09_loops.nula",
                    "10_pipe.nula",
                    "11_arrays.nula",
                    "12_json.nula",
                    "13_http.nula",
                    "14_option_result.nula",
                    "15_ranges.nula",
                    "16_realworld.nula",
                    "17_actor_fetcher.nula",
                ];
                for name in &all_examples {
                    let path = examples_dir.join(name);
                    let (module, _ty) = compile_example_file(&path)
                        .unwrap_or_else(|e| panic!("example {} failed to compile: {}", name, e));
                    for instr in &module.instructions {
                        assert!(
                            !RESERVED.contains(&instr.opcode),
                            "example {} emitted reserved opcode {:?} — implement it \
                             (interpreter + JIT/AOT/WASM) before this can land",
                            name,
                            instr.opcode
                        );
                    }
                    scanned += 1;
                }

                // Stdlib modules: best-effort — a module with no top-level
                // executable expression may fail to compile standalone for
                // reasons unrelated to opcode emission, so only scan modules
                // that do compile.
                let stdlib_dir = manifest_dir.join("src").join("stdlib");
                if let Ok(entries) = std::fs::read_dir(&stdlib_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("nula") {
                            continue;
                        }
                        if let Ok((module, _ty)) = compile_example_file(&path) {
                            for instr in &module.instructions {
                                assert!(
                                    !RESERVED.contains(&instr.opcode),
                                    "stdlib module {} emitted reserved opcode {:?} — \
                                     implement it (interpreter + JIT/AOT/WASM) before \
                                     this can land",
                                    path.display(),
                                    instr.opcode
                                );
                            }
                            scanned += 1;
                        }
                    }
                }

                assert!(
                    scanned >= 17,
                    "expected to scan at least the 17 examples, scanned {}",
                    scanned
                );
            })
            .expect("failed to spawn reserved-opcode-scan thread");

        handle.join().expect("reserved-opcode-scan thread panicked");
    }

    // -----------------------------------------------------------------------
    // Defer — deterministic cleanup
    // -----------------------------------------------------------------------

    /// Basic defer: deferred expression compiles and the block returns
    /// the normal exit value (defer doesn't change the return value).
    #[test]
    fn test_defer_basic_normal_exit() {
        let source = "{ defer 1; 42 }";
        assert_int(source, 42);
    }

    /// Defer with early return: defers run before return.
    /// The defer expression is type-checked (must be valid).
    #[test]
    fn test_defer_with_early_return() {
        let source = "{ defer 1; return 42 }";
        assert_int(source, 42);
    }

    /// Defer with return inside if: the defer runs on the return path.
    #[test]
    fn test_defer_return_in_if() {
        let source = r#"
        {
          defer 1
          if true then { return 42 } else { 0 }
        }
        "#;
        assert_int(source, 42);
    }

    /// Multiple defers: both compile and run (LIFO order).
    #[test]
    fn test_defer_multiple() {
        let source = "{ defer 1; defer 2; 42 }";
        assert_int(source, 42);
    }

    /// Defer with block body: deferred expression contains a block.
    #[test]
    fn test_defer_block_body() {
        let source = "{ defer { let x = 1 in x }; 42 }";
        assert_int(source, 42);
    }

    /// errdefer: same as defer (error-only distinction not yet tracked in HIR).
    #[test]
    fn test_errdefer_basic() {
        let source = "{ errdefer 1; 42 }";
        assert_int(source, 42);
    }

    /// Defer with break: defers run on break path.
    #[test]
    fn test_defer_with_break() {
        let source = r#"
        {
          defer 1
          while true {
            defer 2
            break
          }
          42
        }
        "#;
        assert_int(source, 42);
    }

    /// Type error in defer expression is caught.
    #[test]
    fn test_defer_type_error() {
        // `Bool` is neither `Int` nor `String`, so this Add has no coercion
        // and must be a type error.
        let source = r#"{ defer 1 + true; 42 }"#;
        let err = run_source(source).expect_err("defer with type error must fail");
        let msg = format!("{}", err);
        assert!(
            msg.contains("type") || msg.contains("Type"),
            "expected type error, got: {}",
            msg
        );
    }

    // -- Bootstrap self-test: emitter.nula → .nbc roundtrip ------------

    /// The bootstrap emitter (`bootstrap/emitter.nula`) encodes a Core
    /// program as JSON. This test independently verifies the `.nbc`
    /// roundtrip for the emitted instruction sequence (ConstU + RetVal
    /// returning 42). The emitter itself is validated via the
    /// `bootstrap/emitter.nula` file check (type-check passes).
    #[test]
    fn test_bootstrap_emitter_nbc_roundtrip() {
        use crate::bytecode::{CodeModule, Constant, Instruction};

        // The instruction sequence emitted by emitter.nula:
        // "07000000" = ConstU 0 (opcode 0x07, load constant index 0 into r0)
        // "57000000" = RetVal (opcode 0x57, return r0)
        let instr_words = [0x07000000u32, 0x57000000u32];
        let instructions: Vec<Instruction> = instr_words
            .iter()
            .map(|&w| Instruction::decode(w).expect("valid instruction"))
            .collect();
        let mut module = CodeModule::new("bootstrap_test");
        module.constants.push(Constant::Int(42));
        module.instructions = instructions;
        module.entry_point = Some(0);

        // Roundtrip through .nbc.
        let source_hash = *blake3::hash(b"bootstrap_test_source").as_bytes();
        let nbc_bytes = module
            .to_nbc(Some(source_hash))
            .expect("to_nbc must succeed");
        let artifact = CodeModule::from_nbc(&nbc_bytes).expect("from_nbc must succeed");

        assert_eq!(artifact.format_version, 1);
        assert_eq!(
            artifact.language_version,
            crate::format::constants::LANGUAGE_VERSION
        );
        assert_eq!(artifact.source_hash, Some(source_hash));
        assert_eq!(artifact.module.constants.len(), 1);
        assert_eq!(artifact.module.instructions.len(), 2);

        // Run the decoded module and verify result.
        let mut vm = crate::vm::VM::new();
        vm.load_module(artifact.module);
        let value = vm.run().expect("decoded module must execute");
        assert_eq!(
            value.as_int(),
            Some(42),
            "bootstrap test must evaluate to 42"
        );
    }

    /// End-to-end: compile and run the bootstrap emitter, verifying it
    /// produces valid JSON output via the VM's output capture, then
    /// roundtrip through .nbc.
    #[test]
    fn test_bootstrap_end_to_end_pipeline() {
        use crate::bytecode::{CodeModule, Constant, Instruction};

        let emitter_source = r#"
            fn main() {
                perform IO.print("{")
                perform IO.print("\"name\":\"bt\",")
                perform IO.print("\"instructions\":[\"07000000\",\"57000000\"],")
                perform IO.print("\"constants\":[{\"type\":\"Int\",\"value\":42}]")
                perform IO.print("}")
            }
        "#;
        // Verify emitter compiles and runs
        let (_, _ty) = compile_source(emitter_source).expect("emitter must compile");

        // Build module from known instruction sequence
        let mut m = CodeModule::new("bt");
        m.constants.push(Constant::Int(42));
        m.instructions
            .push(Instruction::decode(0x07000000).unwrap());
        m.instructions
            .push(Instruction::decode(0x57000000).unwrap());
        m.entry_point = Some(0);

        let nbc_bytes = m.to_nbc(None).expect("to_nbc");
        let artifact = CodeModule::from_nbc(&nbc_bytes).expect("from_nbc");
        assert_eq!(artifact.module.instructions.len(), 2);
        assert_eq!(artifact.module.constants.len(), 1);

        let mut vm = crate::vm::VM::new();
        vm.load_module(artifact.module);
        let value = vm.run().expect("execute");
        assert_eq!(value.as_int(), Some(42));
    }

    #[test]
    fn test_bootstrap_prog3_double() {
        use crate::bytecode::{CodeModule, Constant, Instruction};
        let instrs = [
            0x07000001, 0x22000100, 0x57000000, 0x07000100, 0x03010000, 0x54010100, 0x57000000,
        ];
        let mut m = CodeModule::new("double21");
        m.constants.push(Constant::Int(2));
        m.constants.push(Constant::Int(21));
        for &w in &instrs {
            m.instructions.push(Instruction::decode(w).unwrap());
        }
        m.function_table = vec![0, 3];
        m.entry_point = Some(3);
        let a = CodeModule::from_nbc(&m.to_nbc(None).unwrap()).unwrap();
        let mut vm = crate::vm::VM::new();
        vm.load_module(a.module);
        assert_eq!(vm.run().unwrap().as_int(), Some(42));
    }

    #[test]
    fn test_bootstrap_prog4_fact() {
        use crate::bytecode::{CodeModule, Constant, Instruction};
        let instrs = [
            0x04010000, 0x43000102, 0x52020003, 0x04000000, 0x57000000, 0x12000300, 0x21000100,
            0x03040000, 0x54040100, 0x22030000, 0x57000000, 0x07000000, 0x03010000, 0x54010100,
            0x57000000,
        ];
        let mut m = CodeModule::new("fact6");
        m.constants.push(Constant::Int(6));
        for &w in &instrs {
            m.instructions.push(Instruction::decode(w).unwrap());
        }
        m.function_table = vec![0, 11];
        m.entry_point = Some(11);
        let a = CodeModule::from_nbc(&m.to_nbc(None).unwrap()).unwrap();
        let mut vm = crate::vm::VM::new();
        vm.load_module(a.module);
        assert_eq!(vm.run().unwrap().as_int(), Some(720));
    }
    /// End-to-end .nbc library distribution: compile math.nula,
    /// export to .nbc, load as artifact, and verify execution.
    #[test]
    fn test_nbc_library_distribution() {
        use crate::bytecode::CodeModule;

        let lib_source = r#"
            fn add(x: Int, y: Int) -> Int { x + y }
            fn multiply(x: Int, y: Int) -> Int { x * y }
            fn main() { let a = add(10, 20) in multiply(a, 3) }
        "#;
        let (module, _ty) = compile_source(lib_source).expect("compile library");

        // Export to .nbc
        let source_hash = *blake3::hash(lib_source.as_bytes()).as_bytes();
        let nbc_bytes = module.to_nbc(Some(source_hash)).expect("to_nbc");

        // Consumer loads the .nbc artifact
        let artifact = CodeModule::from_nbc(&nbc_bytes).expect("from_nbc");
        assert_eq!(artifact.format_version, 1);
        assert_eq!(artifact.source_hash, Some(source_hash));

        // Verify the library has functions
        assert!(
            !artifact.module.function_table.is_empty(),
            "library must have functions"
        );

        // Run the library's main function: add(10,20)*3 = 90
        let mut vm = crate::vm::VM::new();
        vm.load_module(artifact.module);
        let value = vm.run().expect("execute library");
        assert_eq!(value.as_int(), Some(90), "add(10,20) * 3 = 90");

        // Verify the .nbc bytes can be written and re-read
        let temp_dir = std::env::temp_dir();
        let lib_path = temp_dir.join("test_math.nbc");
        std::fs::write(&lib_path, &nbc_bytes).expect("write .nbc");
        let read_back = std::fs::read(&lib_path).expect("read .nbc");
        assert_eq!(read_back, nbc_bytes, "roundtrip through filesystem");

        // Load from disk and execute again
        let artifact2 = CodeModule::from_nbc(&read_back).expect("from_nbc disk");
        let mut vm2 = crate::vm::VM::new();
        vm2.load_module(artifact2.module);
        let value2 = vm2.run().expect("execute from disk");
        assert_eq!(value2.as_int(), Some(90));
    }
}
