//! Compile-only backend compatibility inspection.
//!
//! This is deliberately a *compiler acceptance* report, not a claim of
//! execution, semantic parity, successful durable recovery, or JIT tier-up.
//! Each answer comes from the real backend's compiler, not a duplicate
//! source-level feature classifier. The schema is versioned for CI consumers.

use crate::mir;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionStatus {
    Compiles,
    Rejected,
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct InspectionDiagnostic {
    /// Stable machine identifier, independent of backend error wording.
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct InspectionSummary {
    pub functions: usize,
    pub behaviors: usize,
    pub foreign_functions: usize,
}

#[derive(Debug, Serialize)]
pub struct BackendInspection {
    pub schema_version: u32,
    pub backend: String,
    pub status: InspectionStatus,
    /// "compile_only": no generated program is executed.
    pub verification: &'static str,
    pub summary: InspectionSummary,
    pub diagnostics: Vec<InspectionDiagnostic>,
}

impl BackendInspection {
    fn new(backend: &str, status: InspectionStatus, summary: InspectionSummary) -> Self {
        Self {
            schema_version: 1,
            backend: backend.to_owned(),
            status,
            verification: "compile_only",
            summary,
            diagnostics: Vec::new(),
        }
    }

    pub fn succeeded(&self) -> bool {
        self.status == InspectionStatus::Compiles
    }

    fn failure(
        backend: &str,
        status: InspectionStatus,
        summary: InspectionSummary,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        let mut report = Self::new(backend, status, summary);
        report.diagnostics.push(InspectionDiagnostic {
            code,
            message: message.into(),
        });
        report
    }

    /// Fail closed when lexing, type checking, effect checking or MIR lowering
    /// fails. Never describe an uncompiled program as backend-compatible.
    pub fn frontend_rejected(backend: &str, message: impl Into<String>) -> Self {
        Self::failure(
            backend,
            InspectionStatus::Rejected,
            InspectionSummary {
                functions: 0,
                behaviors: 0,
                foreign_functions: 0,
            },
            "frontend.rejected",
            message,
        )
    }
}

/// Compile a checked MIR module with the *actual* backend implementation.
///
/// Compilation can have implementation-side allocations (e.g. Cranelift JIT
/// code pages) but never calls the generated program or writes an artifact.
/// Success confirms compiler acceptance only; bytecode/JIT/native/WASM
/// semantics and deployment requirements must be validated separately.
pub fn inspect_mir(module: &mir::Module, backend: &str, native_target: &str) -> BackendInspection {
    let summary = InspectionSummary {
        functions: module.functions.len(),
        behaviors: module.behaviors.len(),
        foreign_functions: module.foreign_functions.len(),
    };

    let result = match backend {
        "bytecode" => {
            let mut cloned = module.clone();
            crate::mir_codegen::compile_mir(&mut cloned, "main").map(|_| ())
        }
        "native" => {
            #[cfg(feature = "native-codegen")]
            {
                crate::aot::AotModule::compile_for_target(module, native_target).map(|_| ())
            }
            #[cfg(not(feature = "native-codegen"))]
            {
                return BackendInspection::failure(
                    backend,
                    InspectionStatus::Unavailable,
                    summary,
                    "backend.disabled",
                    "native compilation requires the native-codegen Cargo feature",
                );
            }
        }
        "wasm" => {
            #[cfg(feature = "wasm-backend")]
            {
                use crate::backends::WasmBackend;
                let mut wasm = crate::backends::DefaultWasmBackend;
                wasm.compile(module, "main").map(|_| ())
            }
            #[cfg(not(feature = "wasm-backend"))]
            {
                return BackendInspection::failure(
                    backend,
                    InspectionStatus::Unavailable,
                    summary,
                    "backend.disabled",
                    "WASM compilation requires the wasm-backend Cargo feature",
                );
            }
        }
        _ => {
            return BackendInspection::failure(
                backend,
                InspectionStatus::Unavailable,
                summary,
                "backend.unknown",
                format!("unknown inspection backend '{backend}' (expected bytecode | native | wasm)"),
            );
        }
    };

    match result {
        Ok(()) => BackendInspection::new(backend, InspectionStatus::Compiles, summary),
        Err(error) => BackendInspection::failure(
            backend,
            InspectionStatus::Rejected,
            summary,
            "backend.rejected",
            error.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hir_lower, lexer::Lexer, mir_lower, parser::Parser, typechecker::TypeChecker};

    fn lower(source: &str) -> mir::Module {
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut checker = TypeChecker::new();
        checker.check_module(&ast).unwrap();
        let hir = hir_lower::lower_module(&ast, &checker.inferred_decl_types);
        mir_lower::lower_module(&hir).unwrap()
    }

    #[test]
    fn bytecode_acceptance_is_a_compile_only_result() {
        let report = inspect_mir(&lower("1 + 2"), "bytecode", "native");
        assert!(report.succeeded());
        assert_eq!(report.verification, "compile_only");
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn unknown_target_is_unavailable_not_compiles() {
        let report = inspect_mir(&lower("1 + 2"), "imaginary", "native");
        assert_eq!(report.status, InspectionStatus::Unavailable);
        assert_eq!(report.diagnostics[0].code, "backend.unknown");
    }

    #[test]
    fn bytecode_codegen_rejection_is_not_reported_as_success() {
        let mut module = lower("1 + 2");
        module.functions[0].blocks[0].terminator = mir::Terminator::Unterminated;
        let report = inspect_mir(&module, "bytecode", "native");
        assert_eq!(report.status, InspectionStatus::Rejected);
        assert_eq!(report.diagnostics[0].code, "backend.rejected");
    }
}
