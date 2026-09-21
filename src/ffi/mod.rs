//! Foreign Function Interface (FFI) core for Nulang.
//!
//! This module provides a registry of dynamically loaded native functions and
//! libraries, plus marshalling between Nulang `Value`s and C ABI types.
//!
//! It intentionally does not touch the language frontend, bytecode compiler, or
//! VM execution engine. The registry is global and thread-safe.

pub mod c_api;
pub mod marshal;
pub mod native;

pub use marshal::{call_native, CType, Signature};
pub use native::{
    register_native_function, FfiRegistry, NativeFunction, NativeLibrary, FFI_REGISTRY,
};

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use crate::ffi::marshal::{call_native, CType, Signature};
    use crate::ffi::native::{register_native_function, NativeFunction};
    use crate::vm::{Value, VM};

    extern "C" fn add(a: i64, b: i64) -> i64 {
        a + b
    }

    #[test]
    fn test_register_and_call_native() {
        let signature = Signature::new(vec![CType::I64, CType::I64], CType::I64);
        // SAFETY: pointer matches the provided signature.
        unsafe {
            register_native_function("add", add as *const c_void, signature).unwrap();
        }

        let func = NativeFunction {
            ptr: add as *const c_void,
            signature: Signature::new(vec![CType::I64, CType::I64], CType::I64),
            library: None,
            symbol: "add".to_string(),
        };
        // SAFETY: pointer matches signature.
        let result = unsafe { call_native(&func, &[Value::int(10), Value::int(32)]).unwrap() };
        assert_eq!(result.as_int(), Some(42));
    }

    extern "C" fn double_int(x: i64) -> i64 {
        x * 2
    }

    fn run_source(source: &str) -> Result<Value, crate::types::NuError> {
        let tokens = crate::lexer::Lexer::new(source).lex()?;
        let ast = crate::parser::Parser::new(tokens).parse_module()?;
        let mut tc = crate::typechecker::TypeChecker::new();
        tc.check_module(&ast)?;
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir)?;
        let module =
            crate::compiler_identity::compile_typed_bytecode(&hir, &mut mir, [], "test")?;
        let mut vm = VM::new();
        vm.load_module(module);
        vm.run()
    }

    #[test]
    fn test_compile_and_run_registered_ffi() {
        let signature = Signature::new(vec![CType::I64], CType::I64);
        // SAFETY: pointer matches the provided signature.
        unsafe {
            register_native_function("double_int", double_int as *const c_void, signature).unwrap();
        }

        let source = r#"
            extern "__nulang_registered__" {
              fn double_int(x: Int) -> Int
            }
            double_int(21)
        "#;
        let result = run_source(source).expect("should compile and call registered FFI");
        assert_eq!(result.as_int(), Some(42));
    }
}
