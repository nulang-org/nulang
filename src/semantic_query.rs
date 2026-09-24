//! Semantic source queries for coding agents.
//!
//! This layer builds on the parser/typechecker but remains independent of the
//! optional AI runtime and LSP transport. It intentionally returns stable,
//! compact JSON rather than editor-protocol objects.

use crate::ast::{Decl, Expr, Pattern, WorkflowItem};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::typechecker::TypeChecker;
use crate::types::{set_source_map_with_file, Capability, EffectRow, NuError, NuResult, Span, Type};
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

pub const SEMANTIC_QUERY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct SemanticQueryReport {
    pub schema_version: u32,
    pub command: String,
    pub file: String,
    pub ok: bool,
    pub symbols: Vec<SemanticSymbol>,
    pub references: Vec<SemanticReference>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticSymbol {
    pub name: String,
    pub qualified_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferred_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferred_effects: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferred_capability: Option<String>,
    pub span: SemanticSpan,
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticReference {
    pub target: String,
    pub owner: String,
    /// `call` for a statically direct call, otherwise `reference`.
    pub kind: String,
    pub span: SemanticSpan,
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticSpan {
    pub file: String,
    pub start_byte: u32,
    pub end_byte: u32,
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

#[derive(Debug)]
struct SemanticIndex {
    file: String,
    symbols: Vec<SemanticSymbol>,
    references: Vec<SemanticReference>,
}

fn query_err(msg: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: msg.into(),
        span: Span::default(),
    }
}

pub fn run(args: &[String]) -> NuResult<()> {
    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    if matches!(command, "help" | "-h" | "--help") {
        print_help();
        return Ok(());
    }
    if !matches!(command, "type" | "references" | "callers" | "callees") {
        return Err(query_err(format!(
            "unknown semantic query '{command}'; try: type, references, callers, callees"
        )));
    }

    let mut positional = Vec::new();
    let mut json = false;
    for arg in &args[1..] {
        if arg == "--json" {
            json = true;
        } else if arg.starts_with('-') {
            return Err(query_err(format!("unknown query option: {arg}")));
        } else {
            positional.push(arg.as_str());
        }
    }
    if positional.len() != 2 {
        return Err(query_err(format!(
            "usage: nulang query {command} <name> <file> [--json]"
        )));
    }

    let requested = positional[0];
    let index = analyze_file(Path::new(positional[1]))?;
    let (symbols, references) = match command {
        "type" => (
            index
                .symbols
                .into_iter()
                .filter(|s| name_matches(requested, &s.name, &s.qualified_name))
                .collect(),
            Vec::new(),
        ),
        "references" => (
            Vec::new(),
            index
                .references
                .into_iter()
                .filter(|r| target_matches(requested, &r.target))
                .collect(),
        ),
        "callers" => (
            Vec::new(),
            index
                .references
                .into_iter()
                .filter(|r| r.kind == "call" && target_matches(requested, &r.target))
                .collect(),
        ),
        "callees" => (
            Vec::new(),
            index
                .references
                .into_iter()
                .filter(|r| r.kind == "call" && owner_matches(requested, &r.owner))
                .collect(),
        ),
        _ => unreachable!(),
    };

    let report = SemanticQueryReport {
        schema_version: SEMANTIC_QUERY_SCHEMA_VERSION,
        command: command.to_string(),
        file: index.file,
        ok: true,
        symbols,
        references,
    };
    emit_report(&report, json)
}

fn print_help() {
    eprintln!(
        "nulang query semantic source inspection\n\n\
         Usage:\n\
           nulang query type <name> <file> [--json]\n\
           nulang query references <name> <file> [--json]\n\
           nulang query callers <name> <file> [--json]\n\
           nulang query callees <owner> <file> [--json]\n"
    );
}

fn emit_report(report: &SemanticQueryReport, json: bool) -> NuResult<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(report).map_err(|e| query_err(e.to_string()))?
        );
        return Ok(());
    }

    match report.command.as_str() {
        "type" => {
            if report.symbols.is_empty() {
                println!("No matching symbols.");
            }
            for symbol in &report.symbols {
                println!(
                    "{}  type={}  effects={}  capability={}  {}:{}:{}",
                    symbol.qualified_name,
                    symbol.inferred_type.as_deref().unwrap_or("<unknown>"),
                    symbol.inferred_effects.as_deref().unwrap_or("<unknown>"),
                    symbol
                        .inferred_capability
                        .as_deref()
                        .unwrap_or("<unknown>"),
                    symbol.span.file,
                    symbol.span.line,
                    symbol.span.col
                );
            }
        }
        _ => {
            if report.references.is_empty() {
                println!("No matching references.");
            }
            for reference in &report.references {
                println!(
                    "{}  {} -> {}  {}:{}:{}",
                    reference.kind,
                    reference.owner,
                    reference.target,
                    reference.span.file,
                    reference.span.line,
                    reference.span.col
                );
            }
        }
    }
    Ok(())
}

fn analyze_file(path: &Path) -> NuResult<SemanticIndex> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| query_err(format!("cannot read '{}': {e}", path.display())))?;
    let file = path.display().to_string();

    set_source_map_with_file(&source, Some(&file));
    let tokens = Lexer::new(&source).lex()?;
    let ast = Parser::new(tokens).parse_module()?;

    // Resolve imports on a clone solely for type inference. Reference spans
    // must remain anchored to the original file AST.
    let mut resolved = ast.clone();
    let mut seen = HashSet::new();
    let typed_ast = if crate::resolver::resolve_imports(&mut resolved, path, &mut seen).is_ok() {
        resolved
    } else {
        ast.clone()
    };

    let mut checker = TypeChecker::new();
    checker.collect_errors = true;
    let _ = checker.check_module(&typed_ast);

    let mut symbols = Vec::new();
    collect_semantic_symbols(
        &ast.decls,
        "",
        &checker,
        &file,
        &source,
        &mut symbols,
    );

    let mut declared = HashSet::new();
    collect_value_names(&ast.decls, &mut declared);

    let mut references = Vec::new();
    collect_decl_references(
        &ast.decls,
        "",
        &declared,
        &file,
        &source,
        &mut references,
    );

    Ok(SemanticIndex {
        file,
        symbols,
        references,
    })
}

fn collect_semantic_symbols(
    decls: &[Decl],
    prefix: &str,
    checker: &TypeChecker,
    file: &str,
    source: &str,
    out: &mut Vec<SemanticSymbol>,
) {
    for decl in decls {
        match decl {
            Decl::Function { name, span, .. } => {
                out.push(semantic_symbol(
                    name,
                    prefix,
                    checker.inferred_decl_types.get(name),
                    *span,
                    file,
                    source,
                ));
            }
            Decl::LetBinding { name, span, .. } | Decl::Signal { name, span, .. } => {
                out.push(semantic_symbol(
                    name,
                    prefix,
                    checker.inferred_decl_types.get(name),
                    *span,
                    file,
                    source,
                ));
            }
            Decl::Given {
                name, ty, span, ..
            } => {
                out.push(semantic_symbol(
                    name,
                    prefix,
                    ty.as_ref(),
                    *span,
                    file,
                    source,
                ));
            }
            Decl::Extern { funcs, .. } => {
                for func in funcs {
                    let param_types = func
                        .params
                        .iter()
                        .map(|(_, ty)| ty.clone())
                        .collect::<Vec<_>>();
                    let param = if param_types.len() == 1 {
                        param_types[0].clone()
                    } else {
                        Type::Tuple(param_types)
                    };
                    let ty = Type::Function {
                        param: Box::new(param),
                        ret: Box::new(func.ret.clone()),
                        effect: EffectRow::singleton(crate::types::Effect::FFI),
                        cap: Capability::Ref,
                    };
                    out.push(semantic_symbol(
                        &func.name,
                        prefix,
                        Some(&ty),
                        func.span,
                        file,
                        source,
                    ));
                }
            }
            Decl::TypeAlias {
                name, body, span, ..
            } => out.push(semantic_symbol(
                name,
                prefix,
                Some(body),
                *span,
                file,
                source,
            )),
            Decl::RecordType {
                name, fields, span, ..
            } => {
                let ty = Type::Record(fields.clone());
                out.push(semantic_symbol(
                    name,
                    prefix,
                    Some(&ty),
                    *span,
                    file,
                    source,
                ));
            }
            Decl::VariantType {
                name,
                variants,
                span,
                ..
            } => {
                let ty = Type::Variant(variants.clone());
                out.push(semantic_symbol(
                    name,
                    prefix,
                    Some(&ty),
                    *span,
                    file,
                    source,
                ));
            }
            Decl::Module {
                name, decls, ..
            } => {
                let nested = qualify(prefix, name);
                collect_semantic_symbols(decls, &nested, checker, file, source, out);
            }
            _ => {}
        }
    }
}

fn semantic_symbol(
    name: &str,
    prefix: &str,
    ty: Option<&Type>,
    span: Span,
    file: &str,
    source: &str,
) -> SemanticSymbol {
    let (inferred_type, inferred_effects, inferred_capability) = match ty {
        Some(ty) => {
            let (effects, cap) = match ty {
                Type::Function { effect, cap, .. } => {
                    (Some(effect.to_string()), Some(cap.to_string()))
                }
                _ => (None, None),
            };
            (Some(ty.to_string()), effects, cap)
        }
        None => (None, None, None),
    };

    SemanticSymbol {
        name: name.to_string(),
        qualified_name: qualify(prefix, name),
        inferred_type,
        inferred_effects,
        inferred_capability,
        span: semantic_span(span, file, source),
    }
}

fn collect_value_names(decls: &[Decl], out: &mut HashSet<String>) {
    for decl in decls {
        match decl {
            Decl::Function { name, .. }
            | Decl::Actor { name, .. }
            | Decl::StateMachine { name, .. }
            | Decl::Workflow { name, .. }
            | Decl::Agent { name, .. }
            | Decl::NamedHandler { name, .. }
            | Decl::LetBinding { name, .. }
            | Decl::Signal { name, .. }
            | Decl::Given { name, .. } => {
                out.insert(name.clone());
            }
            Decl::VariantType { variants, .. } => {
                for (name, _) in variants {
                    out.insert(name.clone());
                }
            }
            Decl::Extern { funcs, .. } => {
                for func in funcs {
                    out.insert(func.name.clone());
                }
            }
            Decl::Module { decls, .. } => collect_value_names(decls, out),
            _ => {}
        }
    }
}

fn collect_decl_references(
    decls: &[Decl],
    prefix: &str,
    declared: &HashSet<String>,
    file: &str,
    source: &str,
    out: &mut Vec<SemanticReference>,
) {
    for decl in decls {
        match decl {
            Decl::Function {
                name,
                params,
                default_values,
                using_params,
                requires,
                ensures,
                body,
                ..
            } => {
                let owner = qualify(prefix, name);
                let mut bound = HashSet::new();
                for param in params.iter().chain(using_params.iter()) {
                    bound.insert(param.name.clone());
                }
                for value in default_values.iter().flatten() {
                    walk_expr(value, &bound, declared, &owner, file, source, out);
                }
                for expr in requires {
                    walk_expr(expr, &bound, declared, &owner, file, source, out);
                }
                let mut ensures_bound = bound.clone();
                ensures_bound.insert("result".to_string());
                for expr in ensures {
                    walk_expr(
                        expr,
                        &ensures_bound,
                        declared,
                        &owner,
                        file,
                        source,
                        out,
                    );
                }
                walk_expr(body, &bound, declared, &owner, file, source, out);
            }
            Decl::Actor {
                name,
                state_fields,
                behaviors,
                init,
                initializer,
                apply_handlers,
                migrations,
                ..
            } => {
                let actor = qualify(prefix, name);
                for (field, _, _, value) in state_fields {
                    walk_expr(
                        value,
                        &HashSet::new(),
                        declared,
                        &format!("{actor}::<state:{field}>"),
                        file,
                        source,
                        out,
                    );
                }
                for (field, value) in init {
                    walk_expr(
                        value,
                        &HashSet::new(),
                        declared,
                        &format!("{actor}::<init:{field}>"),
                        file,
                        source,
                        out,
                    );
                }
                if let Some((init_name, params, body)) = initializer {
                    let bound = params.iter().map(|p| p.name.clone()).collect();
                    walk_expr(
                        body,
                        &bound,
                        declared,
                        &format!("{actor}::{init_name}"),
                        file,
                        source,
                        out,
                    );
                }
                for behavior in behaviors {
                    let bound = behavior
                        .params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect::<HashSet<_>>();
                    walk_expr(
                        &behavior.body,
                        &bound,
                        declared,
                        &format!("{actor}::{}", behavior.name),
                        file,
                        source,
                        out,
                    );
                }
                for handler in apply_handlers {
                    let bound = handler.params.iter().cloned().collect();
                    walk_expr(
                        &handler.body,
                        &bound,
                        declared,
                        &format!("{actor}::apply::{}", handler.event),
                        file,
                        source,
                        out,
                    );
                }
                for migration in migrations {
                    if let Some(body) = &migration.state_body {
                        walk_expr(
                            body,
                            &HashSet::new(),
                            declared,
                            &format!(
                                "{actor}::migration::{}->{}::state",
                                migration.from_version, migration.to_version
                            ),
                            file,
                            source,
                            out,
                        );
                    }
                    for (event, params, body) in &migration.event_migrations {
                        let bound = params.iter().cloned().collect();
                        walk_expr(
                            body,
                            &bound,
                            declared,
                            &format!(
                                "{actor}::migration::{}->{}::{event}",
                                migration.from_version, migration.to_version
                            ),
                            file,
                            source,
                            out,
                        );
                    }
                }
            }
            Decl::StateMachine {
                name,
                entry_hooks,
                exit_hooks,
                ..
            } => {
                let owner = qualify(prefix, name);
                for (state, body) in entry_hooks {
                    walk_expr(
                        body,
                        &HashSet::new(),
                        declared,
                        &format!("{owner}::on_entry::{state}"),
                        file,
                        source,
                        out,
                    );
                }
                for (state, body) in exit_hooks {
                    walk_expr(
                        body,
                        &HashSet::new(),
                        declared,
                        &format!("{owner}::on_exit::{state}"),
                        file,
                        source,
                        out,
                    );
                }
            }
            Decl::Module {
                name, decls, ..
            } => {
                let nested = qualify(prefix, name);
                collect_decl_references(decls, &nested, declared, file, source, out);
            }
            Decl::Workflow {
                name,
                input,
                items,
                compensate,
                ..
            } => {
                let owner = qualify(prefix, name);
                let mut bound = HashSet::new();
                if let Some((input_name, _)) = input {
                    bound.insert(input_name.clone());
                }
                for item in items {
                    match item {
                        WorkflowItem::Step(step) => {
                            walk_expr(
                                &step.body,
                                &bound,
                                declared,
                                &format!("{owner}::{}", step.name),
                                file,
                                source,
                                out,
                            );
                            if let Some(comp) = &step.compensate {
                                walk_expr(
                                    comp,
                                    &bound,
                                    declared,
                                    &format!("{owner}::{}::compensate", step.name),
                                    file,
                                    source,
                                    out,
                                );
                            }
                        }
                        WorkflowItem::Parallel(steps) => {
                            for step in steps {
                                walk_expr(
                                    &step.body,
                                    &bound,
                                    declared,
                                    &format!("{owner}::{}", step.name),
                                    file,
                                    source,
                                    out,
                                );
                                if let Some(comp) = &step.compensate {
                                    walk_expr(
                                        comp,
                                        &bound,
                                        declared,
                                        &format!("{owner}::{}::compensate", step.name),
                                        file,
                                        source,
                                        out,
                                    );
                                }
                            }
                        }
                    }
                }
                if let Some(comp) = compensate {
                    walk_expr(
                        comp,
                        &bound,
                        declared,
                        &format!("{owner}::compensate"),
                        file,
                        source,
                        out,
                    );
                }
            }
            Decl::CrdtDecl { name, fields, .. } => {
                let owner = qualify(prefix, name);
                for (field, _, _, value) in fields {
                    walk_expr(
                        value,
                        &HashSet::new(),
                        declared,
                        &format!("{owner}::<field:{field}>"),
                        file,
                        source,
                        out,
                    );
                }
            }
            Decl::NamedHandler {
                name, handlers, ..
            } => {
                let owner = qualify(prefix, name);
                for handler in handlers {
                    let bound = handler.params.iter().cloned().collect();
                    walk_expr(
                        &handler.body,
                        &bound,
                        declared,
                        &format!("{owner}::{}.{}", handler.effect_name, handler.op_name),
                        file,
                        source,
                        out,
                    );
                }
            }
            Decl::Class { name, methods, .. } => {
                let owner = qualify(prefix, name);
                for method in methods {
                    if let Some(body) = &method.default_body {
                        let bound = method
                            .params
                            .iter()
                            .map(|(name, _)| name.clone())
                            .collect();
                        walk_expr(
                            body,
                            &bound,
                            declared,
                            &format!("{owner}::{}", method.name),
                            file,
                            source,
                            out,
                        );
                    }
                }
            }
            Decl::Impl {
                class_name,
                methods,
                ..
            } => {
                for method in methods {
                    let bound = method
                        .params
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect();
                    walk_expr(
                        &method.body,
                        &bound,
                        declared,
                        &format!("{class_name}::impl::{}", method.name),
                        file,
                        source,
                        out,
                    );
                }
            }
            Decl::LetBinding {
                name, value, ..
            } => walk_expr(
                value,
                &HashSet::new(),
                declared,
                &qualify(prefix, name),
                file,
                source,
                out,
            ),
            Decl::Signal { name, init, .. } => walk_expr(
                init,
                &HashSet::new(),
                declared,
                &qualify(prefix, name),
                file,
                source,
                out,
            ),
            Decl::Given {
                name, value, ..
            } => walk_expr(
                value,
                &HashSet::new(),
                declared,
                &qualify(prefix, name),
                file,
                source,
                out,
            ),
            Decl::TypeAlias { .. }
            | Decl::RecordType { .. }
            | Decl::VariantType { .. }
            | Decl::EffectDecl { .. }
            | Decl::Import { .. }
            | Decl::Extern { .. }
            | Decl::Agent { .. }
            | Decl::Database { .. } => {}
        }
    }
}

fn walk_expr(
    expr: &Expr,
    bound: &HashSet<String>,
    declared: &HashSet<String>,
    owner: &str,
    file: &str,
    source: &str,
    out: &mut Vec<SemanticReference>,
) {
    match expr {
        Expr::Literal(..) | Expr::SelfRef(_) | Expr::Panic(..) => {}
        Expr::Var(name, span) => {
            record_reference(name, *span, "reference", bound, declared, owner, file, source, out);
        }
        Expr::FString(items, _) | Expr::Tuple(items, _) | Expr::Array(items, _) => {
            for item in items {
                walk_expr(item, bound, declared, owner, file, source, out);
            }
        }
        Expr::Lambda { params, body, .. } => {
            let mut nested = bound.clone();
            nested.extend(params.iter().map(|p| p.name.clone()));
            walk_expr(body, &nested, declared, owner, file, source, out);
        }
        Expr::App { func, args, .. } => {
            if let Expr::Var(name, span) = func.as_ref() {
                record_reference(name, *span, "call", bound, declared, owner, file, source, out);
            } else {
                walk_expr(func, bound, declared, owner, file, source, out);
            }
            for arg in args {
                walk_expr(arg, bound, declared, owner, file, source, out);
            }
        }
        Expr::Let {
            name, value, body, ..
        } => {
            walk_expr(value, bound, declared, owner, file, source, out);
            let mut nested = bound.clone();
            nested.insert(name.clone());
            walk_expr(body, &nested, declared, owner, file, source, out);
        }
        Expr::LetRec {
            name,
            params,
            value,
            body,
            ..
        } => {
            let mut nested = bound.clone();
            nested.insert(name.clone());
            nested.extend(params.iter().map(|p| p.name.clone()));
            walk_expr(value, &nested, declared, owner, file, source, out);
            let mut body_bound = bound.clone();
            body_bound.insert(name.clone());
            walk_expr(body, &body_bound, declared, owner, file, source, out);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            walk_expr(cond, bound, declared, owner, file, source, out);
            walk_expr(then_branch, bound, declared, owner, file, source, out);
            if let Some(other) = else_branch {
                walk_expr(other, bound, declared, owner, file, source, out);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            walk_expr(scrutinee, bound, declared, owner, file, source, out);
            for (pattern, guard, body) in arms {
                let mut nested = bound.clone();
                pattern_bindings(pattern, &mut nested);
                if let Some(guard) = guard {
                    walk_expr(guard, &nested, declared, owner, file, source, out);
                }
                walk_expr(body, &nested, declared, owner, file, source, out);
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for item in exprs {
                walk_expr(item, bound, declared, owner, file, source, out);
            }
        }
        Expr::Record(fields, _) => {
            for (_, value) in fields {
                walk_expr(value, bound, declared, owner, file, source, out);
            }
        }
        Expr::FieldAccess { expr, .. }
        | Expr::Unary { expr, .. }
        | Expr::CapAnnotate { expr, .. }
        | Expr::TypeAnnotate { expr, .. }
        | Expr::Consume { expr, .. }
        | Expr::Defer { expr, .. } => {
            walk_expr(expr, bound, declared, owner, file, source, out);
        }
        Expr::RecordUpdate { base, fields, .. } => {
            walk_expr(base, bound, declared, owner, file, source, out);
            for (_, value) in fields {
                walk_expr(value, bound, declared, owner, file, source, out);
            }
        }
        Expr::Index { arr, idx, .. }
        | Expr::Binary {
            left: arr,
            right: idx,
            ..
        } => {
            walk_expr(arr, bound, declared, owner, file, source, out);
            walk_expr(idx, bound, declared, owner, file, source, out);
        }
        Expr::Assign { target, value, .. } => {
            walk_expr(target, bound, declared, owner, file, source, out);
            walk_expr(value, bound, declared, owner, file, source, out);
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            target_node,
            ..
        } => {
            walk_expr(actor_type, bound, declared, owner, file, source, out);
            for (_, value) in init {
                walk_expr(value, bound, declared, owner, file, source, out);
            }
            if let Some(args) = positional_args {
                for arg in args {
                    walk_expr(arg, bound, declared, owner, file, source, out);
                }
            }
            if let Some(node) = target_node {
                walk_expr(node, bound, declared, owner, file, source, out);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            walk_expr(actor, bound, declared, owner, file, source, out);
            for arg in args {
                walk_expr(arg, bound, declared, owner, file, source, out);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, patterns, guard, body) in arms {
                let mut nested = bound.clone();
                for pattern in patterns {
                    pattern_bindings(pattern, &mut nested);
                }
                if let Some(guard) = guard {
                    walk_expr(guard, &nested, declared, owner, file, source, out);
                }
                walk_expr(body, &nested, declared, owner, file, source, out);
            }
            if let Some((timeout, body)) = after {
                walk_expr(timeout, bound, declared, owner, file, source, out);
                walk_expr(body, bound, declared, owner, file, source, out);
            }
        }
        Expr::Emit { args, .. } | Expr::Perform { args, .. } => {
            for arg in args {
                walk_expr(arg, bound, declared, owner, file, source, out);
            }
        }
        Expr::GrainRef { key, .. } => {
            walk_expr(key, bound, declared, owner, file, source, out);
        }
        Expr::Resume { value, .. } => {
            walk_expr(value, bound, declared, owner, file, source, out);
        }
        Expr::Handle { body, handlers, .. } => {
            walk_expr(body, bound, declared, owner, file, source, out);
            for handler in handlers {
                let mut nested = bound.clone();
                nested.extend(handler.params.iter().cloned());
                walk_expr(
                    &handler.body,
                    &nested,
                    declared,
                    owner,
                    file,
                    source,
                    out,
                );
            }
        }
        Expr::Migrate { actor, node, .. } => {
            walk_expr(actor, bound, declared, owner, file, source, out);
            walk_expr(node, bound, declared, owner, file, source, out);
        }
        Expr::Pipe { left, right, .. } => {
            walk_expr(left, bound, declared, owner, file, source, out);
            if let Expr::Var(name, span) = right.as_ref() {
                record_reference(name, *span, "call", bound, declared, owner, file, source, out);
            } else {
                walk_expr(right, bound, declared, owner, file, source, out);
            }
        }
        Expr::For {
            var,
            iterable,
            body,
            ..
        } => {
            walk_expr(iterable, bound, declared, owner, file, source, out);
            let mut nested = bound.clone();
            nested.insert(var.clone());
            walk_expr(body, &nested, declared, owner, file, source, out);
        }
        Expr::While { cond, body, .. } => {
            walk_expr(cond, bound, declared, owner, file, source, out);
            walk_expr(body, bound, declared, owner, file, source, out);
        }
        Expr::Return(value, _) | Expr::Break(value, _) => {
            if let Some(value) = value {
                walk_expr(value, bound, declared, owner, file, source, out);
            }
        }
        Expr::Recover { body, .. } => {
            walk_expr(body, bound, declared, owner, file, source, out);
        }
        Expr::Hide { names, body, .. } => {
            let mut nested = bound.clone();
            nested.extend(names.iter().cloned());
            walk_expr(body, &nested, declared, owner, file, source, out);
        }
        Expr::Seal { names, body, .. } => {
            let allowed = names.iter().collect::<HashSet<_>>();
            let mut nested = bound.clone();
            for name in declared {
                if !allowed.contains(name) {
                    nested.insert(name.clone());
                }
            }
            walk_expr(body, &nested, declared, owner, file, source, out);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record_reference(
    name: &str,
    span: Span,
    kind: &str,
    bound: &HashSet<String>,
    declared: &HashSet<String>,
    owner: &str,
    file: &str,
    source: &str,
    out: &mut Vec<SemanticReference>,
) {
    if declared.contains(name) && !bound.contains(name) {
        out.push(SemanticReference {
            target: name.to_string(),
            owner: owner.to_string(),
            kind: kind.to_string(),
            span: semantic_span(span, file, source),
        });
    }
}

fn pattern_bindings(pattern: &Pattern, bound: &mut HashSet<String>) {
    match pattern {
        Pattern::Wild | Pattern::Lit(_) => {}
        Pattern::Var(name) => {
            bound.insert(name.clone());
        }
        Pattern::Alias(name, inner) => {
            bound.insert(name.clone());
            pattern_bindings(inner, bound);
        }
        Pattern::Tuple(items) => {
            for item in items {
                pattern_bindings(item, bound);
            }
        }
        Pattern::Record(fields) => {
            for (_, item) in fields {
                pattern_bindings(item, bound);
            }
        }
        Pattern::Variant(_, Some(inner)) => pattern_bindings(inner, bound),
        Pattern::Variant(_, None) => {}
    }
}

fn name_matches(requested: &str, name: &str, qualified: &str) -> bool {
    requested == name || requested == qualified
}

fn target_matches(requested: &str, target: &str) -> bool {
    requested == target
}

fn owner_matches(requested: &str, owner: &str) -> bool {
    requested == owner || owner.rsplit("::").next() == Some(requested)
}

fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

fn semantic_span(span: Span, file: &str, source: &str) -> SemanticSpan {
    let len = source.len() as u32;
    let start = span.start.min(len);
    let end = span.end.min(len).max(start);
    let (line, col) = offset_line_col(source, start);
    let (end_line, end_col) = offset_line_col(source, end);
    SemanticSpan {
        file: file.to_string(),
        start_byte: start,
        end_byte: end,
        line,
        col,
        end_line,
        end_col,
    }
}

fn offset_line_col(source: &str, offset: u32) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, &byte) in source.as_bytes().iter().enumerate() {
        if i as u32 >= offset {
            break;
        }
        if byte == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_matching_supports_qualified_and_leaf_names() {
        assert!(owner_matches("main", "main"));
        assert!(owner_matches("inc", "Counter::inc"));
        assert!(owner_matches("Counter::inc", "Counter::inc"));
        assert!(!owner_matches("Counter", "Counter::inc"));
    }

    #[test]
    fn pattern_bindings_collect_nested_names() {
        let pattern = Pattern::Tuple(vec![
            Pattern::Var("x".to_string()),
            Pattern::Alias(
                "whole".to_string(),
                Box::new(Pattern::Variant(
                    "Some".to_string(),
                    Some(Box::new(Pattern::Var("y".to_string()))),
                )),
            ),
        ]);
        let mut bound = HashSet::new();
        pattern_bindings(&pattern, &mut bound);
        assert!(bound.contains("x"));
        assert!(bound.contains("whole"));
        assert!(bound.contains("y"));
    }
}
