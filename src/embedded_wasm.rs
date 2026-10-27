//! Minimal in-process compiler for a deliberately restricted Nulang surface.
//!
//! This builds core Wasm bytes from memory without resolving user imports,
//! spawning a CLI, opening source files, or executing the resulting program.
//! It is not a general Nulang compiler: effects, actors, FFI and other
//! privileged constructs remain intentionally unsupported. Production guest
//! integration requires separate Firecracker/seccomp and host admission proof.

use crate::ast::Decl;
use crate::backends::{DefaultWasmBackend, WasmBackend};
use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use crate::lexer::{Lexer, TokenKind};
use crate::parser::Parser;
use crate::typechecker::TypeChecker;
use crate::types::{NuError, NuResult, Span};

/// Maximum UTF-8 source size for this first, restricted API (4 MiB).
pub const MAX_EMBEDDED_SOURCE_BYTES: usize = 4 * 1024 * 1024;

fn rejected(reason: &str) -> NuError {
    NuError::parse_error(reason.to_owned(), Span::default())
}

/// Compile a single pure Nulang module into in-memory core WebAssembly.
///
/// Only top-level functions and ordinary pure expressions are admitted.
/// This API never invokes import resolution or a toolchain subprocess.
/// Executing or trusting the result is exclusively the host's responsibility.
/// In particular, a Wasm magic header is not full module validation.
pub fn compile_pure_source(source: &str) -> NuResult<Vec<u8>> {
    if source.trim().is_empty() || source.len() > MAX_EMBEDDED_SOURCE_BYTES {
        return Err(rejected("embedded compiler requires nonempty, bounded source"));
    }

    let tokens = Lexer::new(source).lex()?;
    for token in &tokens {
        // Token-level denial handles nested expressions (including lambdas)
        // before semantic passes, without filesystem-backed import resolution.
        // String/comment contents are lexed as their own token kinds.
        if matches!(
            token.kind,
            TokenKind::Import
                | TokenKind::Extern
                | TokenKind::Perform
                | TokenKind::Effect
                | TokenKind::Handle
                | TokenKind::Resume
                | TokenKind::Actor
                | TokenKind::Entity
                | TokenKind::Organization
                | TokenKind::StateMachine
                | TokenKind::Spawn
                | TokenKind::Send
                | TokenKind::Ask
                | TokenKind::Receive
                | TokenKind::Remote
                | TokenKind::Workflow
                | TokenKind::Agent
                | TokenKind::Database
                | TokenKind::Migrate
                | TokenKind::Module
                | TokenKind::Await
                | TokenKind::Monitor
                | TokenKind::Link
                | TokenKind::Exit
                | TokenKind::Given
                | TokenKind::Using
        ) {
            return Err(rejected(
                "embedded compiler rejects imports, native FFI, effects and distributed constructs",
            ));
        }
    }

    let mut parser = Parser::new(tokens);
    let mut ast = parser.parse_module()?;
    if !parser.take_warnings().is_empty() {
        return Err(rejected("embedded compiler rejects parser warnings"));
    }
    if ast.decls.is_empty()
        || !ast.decls.iter().all(|decl| matches!(decl, Decl::Function { .. }))
    {
        return Err(rejected("embedded compiler accepts only top-level functions"));
    }
    if !ast.decls.iter().any(|decl| {
        matches!(decl, Decl::Function { name, params, .. } if name == "main" && params.is_empty())
    }) {
        return Err(rejected("embedded compiler requires zero-argument main"));
    }

    // Match CLI prelude declaration injection without invoking its resolver,
    // which can read files and is unsuitable inside an empty-root guest.
    let prelude = crate::prelude_source::PRELUDE_SOURCE;
    let prelude_ast = Parser::new(Lexer::new(prelude).lex()?).parse_module()?;
    let mut variant_types: Vec<Decl> = prelude_ast
        .decls
        .into_iter()
        .filter(|decl| matches!(decl, Decl::VariantType { .. }))
        .collect();
    variant_types.append(&mut ast.decls);
    ast.decls = variant_types;

    let mut checker = TypeChecker::new();
    checker.check_module(&ast)?;
    let mut effect_checker = EffectChecker::new();
    effect_checker.set_resource_grants(&[]);
    effect_checker.check_module(&ast.decls)?;
    if !effect_checker.diagnostics.is_empty() {
        return Err(rejected("embedded compiler rejects effect diagnostics"));
    }

    // Mirror the CLI's per-function capability analysis; do not permit
    // capability-incompatible programs to bypass it via an embedding API.
    let mut capability_analyzer = CapabilityAnalyzer::new();
    for decl in &ast.decls {
        if let Decl::Function { params, body, .. } = decl {
            let mut context = CapContext::new();
            for param in params {
                if let Some(capability) = param.cap {
                    context = context.with_binding(&param.name, capability);
                }
            }
            capability_analyzer.infer_cap(&context, body)?;
        }
    }

    let hir = crate::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mir = crate::mir_lower::lower_module(&hir)?;
    let mut backend = DefaultWasmBackend;
    backend.compile(&mir, "main")
}
