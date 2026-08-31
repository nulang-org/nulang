//! Nulang source formatter — `nulang fmt <file>`.
//!
//! Parses a `.nula` file and pretty-prints it with canonical formatting.

use crate::ast::{BinOp, Decl, Expr, Literal, Pattern};
use crate::types::Type;
use std::path::Path;

use crate::types::{NuError, NuResult, Span};

/// Format a Nulang source string and return the formatted output.
/// Returns an error if any construct is not yet supported by the formatter
/// (rather than silently dropping or corrupting it).
pub fn format_source(source: &str) -> Result<String, String> {
    let mut lexer = crate::lexer::Lexer::new(source);
    let tokens = lexer.lex().map_err(|e| e.to_string())?;
    let mut parser = crate::parser::Parser::new(tokens);
    let ast = parser.parse_module().map_err(|e| e.to_string())?;

    let mut out = String::new();
    let mut first = true;
    let mut had_unhandled = false;
    for decl in &ast.decls {
        if !first {
            out.push('\n');
        }
        first = false;
        fmt_decl(&mut out, decl, 0, &mut had_unhandled);
    }
    if had_unhandled {
        return Err("file contains constructs not yet supported by the formatter (e.g. workflow, agent, let-binding, class, impl). The file was not modified.".to_string());
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Recursively format all `.nula` files under `dir`.
///
/// When `check_only` is true files are never modified; an error is returned
/// on the first file that *would* be reformatted instead.
pub fn format_directory(dir: &Path, check_only: bool) -> NuResult<()> {
    walk_format(dir, check_only)
}

fn walk_format(dir: &Path, check_only: bool) -> NuResult<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        NuError::vm_error(
            format!("Cannot read directory '{}': {}", dir.display(), e),
            Span::default(),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            NuError::vm_error(
                format!("Cannot read entry in '{}': {}", dir.display(), e),
                Span::default(),
            )
        })?;
        let path = entry.path();
        if path.is_dir() {
            walk_format(&path, check_only)?;
        } else if path.extension().map_or(false, |ext| ext == "nula") {
            let source = std::fs::read_to_string(&path).map_err(|e| {
                NuError::vm_error(
                    format!("Cannot read '{}': {}", path.display(), e),
                    Span::default(),
                )
            })?;
            match format_source(&source) {
                Ok(formatted) => {
                    if formatted != source {
                        if check_only {
                            return Err(NuError::parse_error(
                                format!("Would reformat {}", path.display()),
                                Span::default(),
                            ));
                        }
                        std::fs::write(&path, formatted.as_bytes()).map_err(|e| {
                            NuError::vm_error(
                                format!("Cannot write '{}': {}", path.display(), e),
                                Span::default(),
                            )
                        })?;
                        println!("Formatted {}", path.display());
                    }
                }
                Err(e) => {
                    return Err(NuError::parse_error(
                        format!("{}: {}", path.display(), e),
                        Span::default(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn fmt_decl(out: &mut String, decl: &Decl, indent: usize, had_unhandled: &mut bool) {
    let sp = " ".repeat(indent);
    match decl {
        Decl::Function {
            name,
            params,
            ret_type,
            body,
            effect,
            ..
        } => {
            out.push_str(&format!("{}fn {}(", sp, name));
            for (i, p) in params.iter().enumerate() {
                let pn = &p.name;
                let pty = &p.ty;
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(pn);
                if let Some(t) = pty {
                    out.push_str(&format!(": {}", fmt_type(t)));
                }
            }
            out.push(')');
            if let Some(r) = ret_type {
                out.push_str(&format!(" -> {}", fmt_type(r)));
            }
            if let Some(e) = effect {
                out.push_str(&format!(" ! {}", e));
            }
            out.push_str(" {\n");
            fmt_block_body(out, body, indent + 4, had_unhandled);
            out.push_str(&format!("\n{}}}\n", sp));
        }
        Decl::VariantType {
            name,
            type_params,
            variants,
            ..
        } => {
            out.push_str(&format!("{}type {}", sp, name));
            if !type_params.is_empty() {
                out.push_str(&format!("[{}]", type_params.join(", ")));
            }
            out.push_str(" = ");
            for (i, (vn, vp)) in variants.iter().enumerate() {
                if i > 0 {
                    out.push_str(" | ");
                }
                out.push_str(vn);
                if let Some(p) = vp {
                    out.push_str(&format!("({})", fmt_type(p)));
                }
            }
            out.push('\n');
        }
        Decl::TypeAlias {
            name,
            type_params,
            body,
            ..
        } => {
            out.push_str(&format!("{}type {}", sp, name));
            if !type_params.is_empty() {
                out.push_str(&format!("[{}]", type_params.join(", ")));
            }
            out.push_str(&format!(" = {}\n", fmt_type(body)));
        }
        Decl::Actor {
            name,
            behaviors,
            state_fields,
            ..
        } => {
            out.push_str(&format!("{}actor {} {{\n", sp, name));
            for (fnm, _, fty, fdef) in state_fields {
                out.push_str(&format!("{}    state {}: {}", sp, fnm, fmt_type(fty)));
                out.push_str(" = ");
                fmt_expr(out, fdef, indent + 4, had_unhandled);
                out.push('\n');
            }
            if !state_fields.is_empty() && !behaviors.is_empty() {
                out.push('\n');
            }
            for (i, b) in behaviors.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(&format!("{}    behavior {}(", sp, b.name));
                for (j, p) in b.params.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&p.name);
                    if let Some(t) = &p.ty {
                        out.push_str(&format!(": {}", fmt_type(t)));
                    }
                }
                out.push_str(") {\n");
                fmt_block_body(out, &b.body, indent + 8, had_unhandled);
                out.push_str(&format!("\n{}    }}\n", sp));
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::EffectDecl { name, ops, .. } => {
            out.push_str(&format!("{}effect {} {{\n", sp, name));
            for (op, arg_tys, ret_ty) in ops.iter() {
                out.push_str(&format!("{}    {}: ", sp, op));
                if arg_tys.is_empty() {
                    out.push_str("-> ");
                } else if arg_tys.len() == 1 {
                    out.push_str(&format!("{} -> ", fmt_type(&arg_tys[0])));
                } else {
                    out.push_str("(");
                    for (j, t) in arg_tys.iter().enumerate() {
                        if j > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(&fmt_type(t));
                    }
                    out.push_str(") -> ");
                }
                out.push_str(&fmt_type(ret_ty));
                out.push('\n');
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::Module {
            name,
            exports,
            decls,
            ..
        } => {
            out.push_str(&format!("{}module {} {{\n", sp, name));
            if !exports.is_empty() {
                out.push_str(&format!("{}    export {}\n", sp, exports.join(", ")));
            }
            for d in decls {
                fmt_decl(out, d, indent + 4, had_unhandled);
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::Import { path, items, .. } => {
            if items.is_empty() {
                out.push_str(&format!("{}import {}\n", sp, path));
            } else {
                out.push_str(&format!("{}import {}.{{{}}}\n", sp, path, items.join(", ")));
            }
        }
        Decl::Extern { library, funcs, .. } => {
            out.push_str(&format!("{}extern \"{}\" {{\n", sp, library));
            for f in funcs {
                out.push_str(&format!("{}    fn {}(", sp, f.name));
                for (j, (pn, pt)) in f.params.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&format!("{}: {}", pn, fmt_type(pt)));
                }
                out.push_str(&format!(") -> {}\n", fmt_type(&f.ret)));
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::Workflow {
            name,
            input,
            items,
            compensate,
            ..
        } => {
            out.push_str(&format!("{}workflow {}", sp, name));
            if let Some((in_name, in_ty)) = input {
                out.push_str(&format!("({}: {})", in_name, fmt_type(in_ty)));
            }
            out.push_str(" {\n");
            for item in items {
                match item {
                    crate::ast::WorkflowItem::Step(step) => {
                        out.push_str(&format!("{}    step {} {{\n", sp, step.name));
                        fmt_block_body(out, &step.body, indent + 8, had_unhandled);
                        if let Some(c) = &step.compensate {
                            out.push_str(&format!("\n{}    compensate ", sp));
                            fmt_expr(out, c, indent + 8, had_unhandled);
                        }
                        out.push_str(&format!("\n{}    }}\n", sp));
                    }
                    crate::ast::WorkflowItem::Parallel(steps) => {
                        out.push_str(&format!("{}    parallel {{\n", sp));
                        for step in steps {
                            out.push_str(&format!("{}        step {} {{\n", sp, step.name));
                            fmt_block_body(out, &step.body, indent + 12, had_unhandled);
                            if let Some(c) = &step.compensate {
                                out.push_str(&format!("\n{}        compensate ", sp));
                                fmt_expr(out, c, indent + 12, had_unhandled);
                            }
                            out.push_str(&format!("\n{}        }}\n", sp));
                        }
                        out.push_str(&format!("{}    }}\n", sp));
                    }
                }
            }
            if let Some(c) = compensate {
                out.push_str(&format!("{}    compensate ", sp));
                fmt_expr(out, c, indent + 4, had_unhandled);
                out.push('\n');
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::Agent {
            name,
            model,
            system_prompt,
            tools,
            memory,
            semantic_memory,
            procedural_memory,
            pricing,
            fallback,
            retry,
            ..
        } => {
            out.push_str(&format!("{}agent {} = {{\n", sp, name));
            out.push_str(&format!("{}    model: \"{}\",\n", sp, model));
            if let Some(sp_) = system_prompt {
                out.push_str(&format!("{}    system_prompt: \"{}\",\n", sp, sp_));
            }
            if !tools.is_empty() {
                out.push_str(&format!("{}    tools: [{}],\n", sp, tools.join(", ")));
            }
            if let Some(m) = memory {
                out.push_str(&format!(
                    "{}    memory: {{ max_turns: {} }},\n",
                    sp, m.max_turns
                ));
            }
            if let Some(sm) = semantic_memory {
                out.push_str(&format!(
                    "{}    semantic_memory: {{ dimensions: {} }},\n",
                    sp, sm.dimensions
                ));
            }
            if let Some(pm) = procedural_memory {
                out.push_str(&format!(
                    "{}    procedural_memory: {{ namespace: \"{}\" }},\n",
                    sp, pm.namespace
                ));
            }
            if let Some(p) = pricing {
                out.push_str(&format!(
                    "{}    pricing: {{ input: {}, output: {} }},\n",
                    sp, p.input, p.output
                ));
            }
            if !fallback.is_empty() {
                out.push_str(&format!("{}    fallback: [", sp));
                for (i, f) in fallback.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&format!(
                        "{{ model: \"{}\", on: [{}]",
                        f.model,
                        f.on.join(", ")
                    ));
                    if let Some(mt) = f.max_tokens {
                        out.push_str(&format!(", max_tokens: {}", mt));
                    }
                    out.push_str(" }");
                }
                out.push_str("],\n");
            }
            if let Some(r) = retry {
                out.push_str(&format!(
                    "{}    retry: {{ max_attempts: {}, backoff: ",
                    sp, r.max_attempts
                ));
                match &r.backoff {
                    crate::ast::AgentBackoff::Exponential {
                        initial_ms,
                        factor,
                        max_ms,
                    } => {
                        out.push_str(&format!(
                            "Exponential {{ initial_ms: {}, factor: {}, max_ms: {} }}",
                            initial_ms, factor, max_ms
                        ));
                    }
                    crate::ast::AgentBackoff::Fixed { delay_ms } => {
                        out.push_str(&format!("Fixed {{ delay_ms: {} }}", delay_ms));
                    }
                }
                out.push_str(" },\n");
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::Database { name, tables, .. } => {
            out.push_str(&format!("{}database {} {{\n", sp, name));
            for table in tables {
                out.push_str(&format!("{}    {} {{\n", sp, table.name));
                for col in &table.columns {
                    out.push_str(&format!(
                        "{}        {}: {}",
                        sp,
                        col.name,
                        fmt_type(&col.col_type)
                    ));
                    if !col.modifiers.is_empty() {
                        out.push_str(&format!(" {}", col.modifiers.join(" ")));
                    }
                    out.push_str("\n");
                }
                out.push_str(&format!("{}    }}\n", sp));
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::CrdtDecl { name, fields, .. } => {
            out.push_str(&format!("{}crdt {} {{\n", sp, name));
            for (fname, cty, fty, default) in fields {
                out.push_str(&format!(
                    "{}    {} {}: {} = ",
                    sp,
                    cty.keyword(),
                    fname,
                    fmt_type(fty)
                ));
                fmt_expr(out, default, indent + 4, had_unhandled);
                out.push('\n');
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::NamedHandler { name, handlers, .. } => {
            out.push_str(&format!("{}handler {} = {{\n", sp, name));
            for h in handlers {
                out.push_str(&format!("{}    | {}.{}(", sp, h.effect_name, h.op_name));
                for (j, p) in h.params.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(p);
                }
                out.push(')');
                if h.resume {
                    out.push_str(" resume");
                }
                out.push_str(" => ");
                fmt_expr(out, &h.body, indent + 4, had_unhandled);
                out.push('\n');
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::Class {
            name,
            type_params,
            super_classes,
            methods,
            ..
        } => {
            out.push_str(&format!("{}class {}", sp, name));
            if !type_params.is_empty() {
                out.push_str(&format!("[{}]", type_params.join(", ")));
            }
            if !super_classes.is_empty() {
                out.push_str(&format!(" : {}", super_classes.join(", ")));
            }
            out.push_str(" {\n");
            for m in methods {
                out.push_str(&format!("{}    fn {}(", sp, m.name));
                for (j, (pn, pt)) in m.params.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(pn);
                    // Untyped params are stored as Unit (the parser's sentinel);
                    // only emit an annotation for an explicitly-written type.
                    if !is_omittable_type(pt) {
                        out.push_str(&format!(": {}", fmt_type(pt)));
                    }
                }
                out.push_str(")");
                // Untyped methods default to a Unit return; only emit -> when explicit.
                if !is_omittable_type(&m.return_type) {
                    out.push_str(&format!(" -> {}", fmt_type(&m.return_type)));
                }
                if let Some(db) = &m.default_body {
                    out.push_str(" = ");
                    fmt_expr(out, db, indent + 8, had_unhandled);
                }
                out.push('\n');
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::Impl {
            class_name,
            type_params,
            for_type,
            methods,
            ..
        } => {
            out.push_str(&format!("{}impl {}", sp, class_name));
            if !type_params.is_empty() {
                out.push_str(&format!("[{}]", type_params.join(", ")));
            }
            out.push_str(&format!(" {} {{\n", fmt_type(for_type)));
            for m in methods {
                out.push_str(&format!("{}    fn {}(", sp, m.name));
                for (j, (pn, pt)) in m.params.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(pn);
                    if !is_omittable_type(pt) {
                        out.push_str(&format!(": {}", fmt_type(pt)));
                    }
                }
                out.push_str(")");
                if !is_omittable_type(&m.return_type) {
                    out.push_str(&format!(" -> {}", fmt_type(&m.return_type)));
                }
                out.push_str(" = ");
                fmt_expr(out, &m.body, indent + 4, had_unhandled);
                out.push('\n');
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::LetBinding {
            name,
            type_ann,
            value,
            mutable,
            ..
        } => {
            out.push_str(&format!("{}let ", sp));
            if *mutable {
                out.push_str("mut ");
            }
            out.push_str(name);
            if let Some(t) = type_ann {
                out.push_str(&format!(": {}", fmt_type(t)));
            }
            out.push_str(" = ");
            fmt_expr(out, value, indent, had_unhandled);
            out.push('\n');
        }
        Decl::Signal { name, ty, init, .. } => {
            out.push_str(&format!("{}signal {}", sp, name));
            out.push_str(&format!(": {}", fmt_type(ty)));
            out.push_str(" = ");
            fmt_expr(out, init, indent, had_unhandled);
            out.push('\n');
        }
        Decl::Given {
            name, ty, value, ..
        } => {
            out.push_str(&format!("{}given {}", sp, name));
            if let Some(t) = ty {
                out.push_str(&format!(": {}", fmt_type(t)));
            }
            out.push_str(" = ");
            fmt_expr(out, value, indent, had_unhandled);
            out.push('\n');
        }
        Decl::StateMachine {
            name,
            states,
            events,
            entry_hooks,
            exit_hooks,
            ..
        } => {
            out.push_str(&format!("{}state_machine {} {{\n", sp, name));
            for st in states {
                out.push_str(&format!("{}    state {}\n", sp, st));
            }
            if !events.is_empty() {
                out.push('\n');
            }
            for ev in events {
                out.push_str(&format!("{}    event {}(", sp, ev.name));
                for (j, p) in ev.params.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&p.name);
                }
                out.push_str(&format!("): {}\n", ev.target));
            }
            if !entry_hooks.is_empty() || !exit_hooks.is_empty() {
                out.push('\n');
            }
            for (state, body) in entry_hooks {
                out.push_str(&format!("{}    on_entry {} {{\n", sp, state));
                fmt_block_body(out, body, indent + 8, had_unhandled);
                out.push_str(&format!("\n{}    }}\n", sp));
            }
            for (state, body) in exit_hooks {
                out.push_str(&format!("{}    on_exit {} {{\n", sp, state));
                fmt_block_body(out, body, indent + 8, had_unhandled);
                out.push_str(&format!("\n{}    }}\n", sp));
            }
            out.push_str(&format!("{}}}\n", sp));
        }
        Decl::RecordType {
            name,
            type_params,
            fields,
            public,
            ..
        } => {
            out.push_str(&format!("{}type {}", sp, name));
            if !type_params.is_empty() {
                out.push_str(&format!("[{}]", type_params.join(", ")));
            }
            out.push_str(" = {\n");
            for (fname, fty) in fields {
                out.push_str(&format!("{}    {}: {},\n", sp, fname, fmt_type(fty)));
            }
            out.push_str(&format!("{}}}", sp));
            if *public {
                out.push_str(" // public");
            }
            out.push('\n');
        }
    }
}

fn fmt_expr(out: &mut String, expr: &Expr, indent: usize, had_unhandled: &mut bool) {
    let sp = " ".repeat(indent);
    match expr {
        Expr::FString(parts, _) => {
            out.push_str("f\"");
            for part in parts {
                match part {
                    Expr::Literal(Literal::String(s), _) => out.push_str(s),
                    e => {
                        out.push_str("{");
                        fmt_expr(out, e, indent, had_unhandled);
                        out.push('}');
                    }
                }
            }
            out.push('"');
        }
        Expr::Literal(lit, _) => match lit {
            Literal::Int(n) => out.push_str(&n.to_string()),
            Literal::Float(f) => out.push_str(&f.to_string()),
            Literal::String(s) => out.push_str(&format!("\"{}\"", s)),
            Literal::Bool(b) => out.push_str(&b.to_string()),
            Literal::Nil => out.push_str("nil"),
            Literal::Unit => out.push_str("unit"),
        },
        Expr::Var(name, _) => out.push_str(name),
        Expr::SelfRef(_) => out.push_str("self"),
        Expr::Let {
            name, value, body, ..
        } => {
            out.push_str(&format!("let {} = ", name));
            fmt_expr(out, value, indent, had_unhandled);
            out.push_str(" in\n");
            fmt_expr(out, body, indent, had_unhandled);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            out.push_str("if ");
            fmt_expr(out, cond, indent, had_unhandled);
            out.push_str(" then ");
            fmt_expr(out, then_branch, indent, had_unhandled);
            if let Some(e) = else_branch {
                out.push_str(" else ");
                fmt_expr(out, e, indent, had_unhandled);
            }
        }
        Expr::App { func, args, .. } => {
            fmt_expr(out, func, indent, had_unhandled);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt_expr(out, a, indent, had_unhandled);
            }
            out.push(')');
        }
        Expr::Lambda { params, body, .. } => {
            out.push_str("fn(");
            for (i, p) in params.iter().enumerate() {
                let pn = &p.name;
                let _ = &p.ty;
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(pn);
            }
            out.push_str(") { ");
            fmt_expr(out, body, indent, had_unhandled);
            out.push_str(" }");
        }
        Expr::Binary {
            op, left, right, ..
        } => {
            fmt_expr(out, left, indent, had_unhandled);
            out.push_str(&format!(" {} ", op_sym(*op)));
            fmt_expr(out, right, indent, had_unhandled);
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            out.push_str("match ");
            fmt_expr(out, scrutinee, indent, had_unhandled);
            out.push_str(" {\n");
            for (pat, guard, body) in arms {
                out.push_str(&format!("{}    | ", sp));
                fmt_pat(out, pat);
                if let Some(g) = guard {
                    out.push_str(" if ");
                    fmt_expr(out, g, indent + 4, had_unhandled);
                }
                out.push_str(" => ");
                fmt_expr(out, body, indent + 4, had_unhandled);
                out.push('\n');
            }
            out.push_str(&format!("{}}}", sp));
        }
        Expr::Resume { value, .. } => {
            out.push_str("resume(");
            fmt_expr(out, value, indent, had_unhandled);
            out.push(')');
        }
        Expr::Block { exprs, .. } => {
            out.push_str("{\n");
            for e in exprs {
                out.push_str(&format!("{}    ", sp));
                fmt_expr(out, e, indent + 4, had_unhandled);
                out.push('\n');
            }
            out.push_str(&format!("{}}}", sp));
        }
        Expr::Par { exprs, .. } => {
            out.push_str("par {\n");
            for e in exprs {
                out.push_str(&format!("{}    ", sp));
                fmt_expr(out, e, indent + 4, had_unhandled);
                out.push('\n');
            }
            out.push_str(&format!("{}}}", sp));
        }
        Expr::Perform {
            effect, op, args, ..
        } => {
            out.push_str(&format!("perform {}.{}(", effect, op));
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt_expr(out, a, indent, had_unhandled);
            }
            out.push(')');
        }
        Expr::GrainRef {
            grain_type, key, ..
        } => {
            out.push_str(&format!("Grain(\"{}\", ", grain_type));
            fmt_expr(out, key, indent, had_unhandled);
            out.push(')');
        }
        Expr::Pipe { left, right, .. } => {
            fmt_expr(out, left, indent, had_unhandled);
            out.push_str(" |> ");
            fmt_expr(out, right, indent, had_unhandled);
        }
        Expr::FieldAccess { expr, field, .. } => {
            fmt_expr(out, expr, indent, had_unhandled);
            out.push_str(&format!(".{}", field));
        }
        Expr::Tuple(elems, _) => {
            out.push('(');
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt_expr(out, e, indent, had_unhandled);
            }
            out.push(')');
        }
        Expr::Record(fields, _) => {
            out.push_str("{ ");
            for (i, (nm, val)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("{}: ", nm));
                fmt_expr(out, val, indent, had_unhandled);
            }
            out.push_str(" }");
        }
        Expr::RecordUpdate { base, fields, .. } => {
            out.push_str("{ ");
            fmt_expr(out, base, indent, had_unhandled);
            out.push_str(" .. ");
            for (i, (nm, val)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("{} = ", nm));
                fmt_expr(out, val, indent, had_unhandled);
            }
            out.push_str(" }");
        }
        Expr::Consume { expr, .. } => {
            out.push_str("consume ");
            fmt_expr(out, expr, indent, had_unhandled);
        }
        Expr::Recover { body, .. } => {
            out.push_str("recover ");
            fmt_expr(out, body, indent, had_unhandled);
        }
        Expr::Return(value, _) => {
            out.push_str("return");
            if let Some(v) = value {
                out.push(' ');
                fmt_expr(out, v, indent, had_unhandled);
            }
        }
        Expr::Break(_, _) => out.push_str("break"),
        Expr::Array(elems, _) => {
            out.push('[');
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt_expr(out, e, indent, had_unhandled);
            }
            out.push(']');
        }
        Expr::Index { arr, idx, .. } => {
            fmt_expr(out, arr, indent, had_unhandled);
            out.push('[');
            fmt_expr(out, idx, indent, had_unhandled);
            out.push(']');
        }
        Expr::Unary { op, expr, .. } => {
            match op {
                crate::ast::UnOp::Ref(cap) => {
                    // `&iso x` — the capability keyword must be separated
                    // from the operand, or `&isox` re-parses as ref-cap `&` of
                    // the identifier `isox`.
                    out.push_str(&format!("&{} ", cap));
                }
                other => out.push_str(&op_sym_unary(*other)),
            }
            fmt_expr(out, expr, indent, had_unhandled);
        }
        Expr::Assign { target, value, .. } => {
            fmt_expr(out, target, indent, had_unhandled);
            out.push_str(" = ");
            fmt_expr(out, value, indent, had_unhandled);
        }
        Expr::While { cond, body, .. } => {
            out.push_str("while ");
            fmt_expr(out, cond, indent, had_unhandled);
            out.push_str(" {\n");
            fmt_block_body(out, body, indent + 4, had_unhandled);
            out.push_str(&format!("\n{}}}", sp));
        }
        Expr::For {
            var,
            iterable,
            body,
            ..
        } => {
            out.push_str(&format!("for {} in ", var));
            fmt_expr(out, iterable, indent, had_unhandled);
            out.push_str(" {\n");
            fmt_block_body(out, body, indent + 4, had_unhandled);
            out.push_str(&format!("\n{}}}", sp));
        }
        Expr::LetRec {
            name,
            params,
            value,
            body,
            ..
        } => {
            out.push_str(&format!("let rec {}(", name));
            for (i, p) in params.iter().enumerate() {
                let pn = &p.name;
                let _ = &p.ty;
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(pn);
            }
            out.push_str(") = ");
            fmt_expr(out, value, indent, had_unhandled);
            out.push_str(" in\n");
            fmt_expr(out, body, indent, had_unhandled);
        }
        Expr::Send {
            actor,
            behavior,
            args,
            ..
        } => {
            out.push_str("send ");
            fmt_expr(out, actor, indent, had_unhandled);
            out.push_str(&format!(" {}(", behavior));
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt_expr(out, a, indent, had_unhandled);
            }
            out.push(')');
        }
        Expr::Ask {
            actor,
            behavior,
            args,
            ..
        } => {
            out.push_str("ask ");
            fmt_expr(out, actor, indent, had_unhandled);
            out.push_str(&format!(" {}(", behavior));
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt_expr(out, a, indent, had_unhandled);
            }
            out.push(')');
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            register_as,
            target_node,
            ..
        } => {
            out.push_str("spawn");
            if let Some(node) = target_node {
                out.push('@');
                fmt_expr(out, node, indent, had_unhandled);
            }
            out.push(' ');
            fmt_expr(out, actor_type, indent, had_unhandled);
            out.push('(');
            let mut first_arg = true;
            if let Some(pos) = positional_args {
                for a in pos {
                    if !first_arg {
                        out.push_str(", ");
                    }
                    fmt_expr(out, a, indent, had_unhandled);
                    first_arg = false;
                }
            }
            for (nm, val) in init {
                if !first_arg {
                    out.push_str(", ");
                }
                out.push_str(&format!("{} = ", nm));
                fmt_expr(out, val, indent, had_unhandled);
                first_arg = false;
            }
            out.push(')');
            if let Some(reg) = register_as {
                out.push_str(&format!(" as \"{}\"", reg));
            }
        }
        Expr::Handle { body, handlers, .. } => {
            out.push_str("handle ");
            fmt_expr(out, body, indent, had_unhandled);
            out.push_str(" with {\n");
            for h in handlers {
                out.push_str(&format!("{}    | {}.{}(", sp, h.effect_name, h.op_name));
                for (j, p) in h.params.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(p);
                }
                out.push(')');
                if h.resume {
                    out.push_str(" resume");
                }
                out.push_str(" => ");
                fmt_expr(out, &h.body, indent + 4, had_unhandled);
                out.push('\n');
            }
            out.push_str(&format!("{}}}", sp));
        }
        Expr::Receive { arms, after, .. } => {
            out.push_str("receive {\n");
            for (bname, pats, guard, body) in arms {
                out.push_str(&format!("{}    | {}(", sp, bname));
                for (j, p) in pats.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    fmt_pat(out, p);
                }
                out.push(')');
                if let Some(g) = guard {
                    out.push_str(" if ");
                    fmt_expr(out, g, indent + 4, had_unhandled);
                }
                out.push_str(" => ");
                fmt_expr(out, body, indent + 4, had_unhandled);
                out.push('\n');
            }
            if let Some((ms, tb)) = after {
                out.push_str(&format!("{}    after ", sp));
                fmt_expr(out, ms, indent + 4, had_unhandled);
                out.push_str(" => ");
                fmt_expr(out, tb, indent + 4, had_unhandled);
                out.push('\n');
            }
            out.push_str(&format!("{}}}", sp));
        }
        Expr::Emit { event, args, .. } => {
            out.push_str(&format!("emit {}(", event));
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt_expr(out, a, indent, had_unhandled);
            }
            out.push(')');
        }
        Expr::Migrate { actor, node, .. } => {
            out.push_str("migrate ");
            fmt_expr(out, actor, indent, had_unhandled);
            out.push_str(" to ");
            fmt_expr(out, node, indent, had_unhandled);
        }
        Expr::CapAnnotate { expr, cap, .. } => {
            out.push_str("(");
            fmt_expr(out, expr, indent, had_unhandled);
            out.push_str(&format!(" :cap {:?})", cap));
        }
        Expr::TypeAnnotate { expr, ty, .. } => {
            out.push_str("(");
            fmt_expr(out, expr, indent, had_unhandled);
            out.push_str(&format!(": {})", fmt_type(ty)));
        }
        Expr::Defer {
            expr, error_only, ..
        } => {
            if *error_only {
                out.push_str("errdefer ");
            } else {
                out.push_str("defer ");
            }
            fmt_expr(out, expr, indent, had_unhandled);
        }
        Expr::Hide { names, body, .. } => {
            out.push_str("hide ");
            out.push_str(&names.join(", "));
            out.push(' ');
            fmt_expr(out, body, indent, had_unhandled);
        }
        Expr::Seal { names, body, .. } => {
            out.push_str("seal except ");
            out.push_str(&names.join(", "));
            out.push(' ');
            fmt_expr(out, body, indent, had_unhandled);
        }
        Expr::Panic(msg, _) => {
            out.push_str("panic(\"");
            out.push_str(msg);
            out.push_str("\")");
        }
    }
}

fn fmt_pat(out: &mut String, pat: &Pattern) {
    match pat {
        Pattern::Wild => out.push('_'),
        Pattern::Var(name) => out.push_str(name),
        Pattern::Lit(lit) => match lit {
            Literal::Int(n) => out.push_str(&n.to_string()),
            Literal::String(s) => out.push_str(&format!("\"{}\"", s)),
            Literal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            _ => out.push_str("_"),
        },
        Pattern::Variant(name, Some(inner)) => {
            out.push_str(&format!("{}(", name));
            fmt_pat(out, inner);
            out.push(')');
        }
        Pattern::Variant(name, None) => out.push_str(name),
        Pattern::Tuple(elems) => {
            out.push('(');
            for (i, p) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt_pat(out, p);
            }
            out.push(')');
        }
        Pattern::Record(fields) => {
            out.push('{');
            for (i, (name, pat)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(name);
                out.push_str(": ");
                fmt_pat(out, pat);
            }
            out.push('}');
        }
        Pattern::Alias(name, inner) => {
            out.push_str(name);
            out.push_str(" @ ");
            fmt_pat(out, inner);
        }
    }
}

/// Format a function/behavior body, unwrapping blocks to avoid double braces.
fn fmt_block_body(out: &mut String, body: &Expr, indent: usize, had_unhandled: &mut bool) {
    if let Expr::Block { exprs, .. } = body {
        let sp = " ".repeat(indent);
        for (i, e) in exprs.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&sp);
            fmt_expr(out, e, indent, had_unhandled);
        }
    } else {
        fmt_expr(out, body, indent, had_unhandled);
    }
}

fn op_sym(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Pow => "**",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::Le => "<=",
        BinOp::Ge => ">=",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        BinOp::Assign => "=",
        BinOp::Range => "..",
        BinOp::Pipe => "|>",
    }
}

fn fmt_type(ty: &Type) -> String {
    format!("{}", ty)
}

/// Whether a type is a sentinel that must not be re-emitted as an explicit
/// annotation: the parser stores omitted class/impl method param and return
/// types as `Unit`, and a bare `Type::Var` may be a class type parameter
/// (`self: T`) whose name is unrecoverable from the AST (TypeVar::Display is
/// `'_`). Re-emitting either would corrupt or fail to re-parse the output.
fn is_omittable_type(ty: &Type) -> bool {
    match ty {
        Type::Primitive(crate::types::PrimitiveType::Unit) => true,
        Type::Var(_) => true,
        _ => false,
    }
}

fn op_sym_unary(op: crate::ast::UnOp) -> String {
    match op {
        crate::ast::UnOp::Neg => "-".to_string(),
        crate::ast::UnOp::Not => "!".to_string(),
        crate::ast::UnOp::Deref => "*".to_string(),
        crate::ast::UnOp::Ref(cap) => format!("&{}", cap),
    }
}

#[cfg(test)]
mod tests {
    use super::format_source;

    /// Formatting must be idempotent: formatting the formatted output is a
    /// no-op (returns the same text), so repeated `fmt` runs converge.
    fn assert_idempotent(src: &str) {
        let once = format_source(src).unwrap_or_else(|e| panic!("first format: {e}"));
        let twice = format_source(&once).unwrap_or_else(|e| panic!("reformat: {e}"));
        assert_eq!(
            once, twice,
            "formatting must be idempotent\n--- once ---\n{once}\n--- twice ---\n{twice}"
        );
    }

    #[test]
    fn test_fmt_effect_decl() {
        let src = r#"effect MyEffect {
    op: Int -> Bool
    op2: (Int, String) -> Bool
}"#;
        let out = format_source(src).expect("effect decl formats");
        assert!(out.contains("effect MyEffect {"), "got: {out}");
        assert!(out.contains("op: Int -> Bool"), "got: {out}");
        assert!(out.contains("op2: (Int, String) -> Bool"), "got: {out}");
        assert_idempotent(src);
    }

    #[test]
    fn test_fmt_import() {
        let out = format_source("import Foo::Bar").expect("import formats");
        assert!(out.contains("import Foo::Bar"), "got: {out}");
    }

    #[test]
    fn test_fmt_cap_ref_constructors() {
        // `&cap` must format with the capability keyword AND a space before
        // the operand: the old `op_sym_unary` printed "ref" for every Ref op
        // (destroying the capability), and a missing space made `&iso x`
        // re-parse as ref-cap `&` of the identifier `isox`.
        let src = r#"fn main() {
    let x = 5
    let r = &iso x
    let s = &val x
    *r + *s
}
"#;
        let out = format_source(src).expect("cap refs format");
        assert!(out.contains("&iso x"), "iso constructor lost: {out}");
        assert!(out.contains("&val x"), "val constructor lost: {out}");
        assert!(
            !out.contains("ref x"),
            "old 'ref' emission still present: {out}"
        );
        assert_idempotent(src);
    }

    #[test]
    fn test_fmt_class_and_impl() {
        let src = r#"class Eq[T] {
    fn eq(self: T, other: T) -> Bool
}
impl Eq Int {
    fn eq(self, other) = self == other
}"#;
        let out = format_source(src).expect("class/impl formats");
        assert!(out.contains("class Eq["), "got: {out}");
        assert!(out.contains("impl Eq Int"), "got: {out}");
        assert!(
            out.contains("fn eq(self, other) = self == other"),
            "got: {out}"
        );
        assert_idempotent(src);
    }

    #[test]
    fn test_fmt_agent() {
        let src = r#"agent MyAgent = {
    model: "gpt-4o",
    system_prompt: "You are helpful.",
    tools: [add, subtract],
    memory: { max_turns: 100 }
}"#;
        let out = format_source(src).expect("agent formats");
        assert!(out.contains("agent MyAgent = {"), "got: {out}");
        assert!(out.contains("model: \"gpt-4o\""), "got: {out}");
        assert!(out.contains("tools: [add, subtract]"), "got: {out}");
        assert!(out.contains("memory: { max_turns: 100 }"), "got: {out}");
        assert_idempotent(src);
    }

    #[test]
    fn test_fmt_workflow() {
        let src = "workflow PurchaseOrder { step validate { 1 } }";
        let out = format_source(src).expect("workflow formats");
        assert!(out.contains("workflow PurchaseOrder {"), "got: {out}");
        assert!(out.contains("step validate"), "got: {out}");
        assert_idempotent(src);
    }

    #[test]
    fn test_fmt_state_machine() {
        let src = r#"state_machine Tcp {
    state Closed
    state Connecting
    event connect(address): Connecting
    on_entry Connecting { 1 }
}"#;
        let out = format_source(src).expect("state_machine formats");
        assert!(out.contains("state_machine Tcp {"), "got: {out}");
        assert!(out.contains("state Closed"), "got: {out}");
        assert!(
            out.contains("event connect(address): Connecting"),
            "got: {out}"
        );
        assert!(out.contains("on_entry Connecting {"), "got: {out}");
        assert_idempotent(src);
    }

    #[test]
    fn test_fmt_extern() {
        let src = r#"extern "libm.so.6" {
    fn sqrt(x: Float) -> Float
}"#;
        let out = format_source(src).expect("extern formats");
        assert!(out.contains("extern \"libm.so.6\" {"), "got: {out}");
        assert!(out.contains("fn sqrt(x: Float) -> Float"), "got: {out}");
        assert_idempotent(src);
    }

    #[test]
    fn test_fmt_crdt() {
        let src = "crdt C { gcounter hits: Int = 0 }";
        let out = format_source(src).expect("crdt formats");
        assert!(out.contains("crdt C {"), "got: {out}");
        assert!(out.contains("gcounter hits: Int = 0"), "got: {out}");
        assert_idempotent(src);
    }

    #[test]
    fn test_fmt_spawn_handle_receive() {
        let src = r#"
fn main() {
    let a = spawn Greeter();
    receive { | Msg(x) if x > 0 => x }
    emit Event(1)
}"#;
        let out = format_source(src).expect("spawn/receive/emit formats");
        assert!(out.contains("spawn Greeter()"), "got: {out}");
        assert!(out.contains("receive {"), "got: {out}");
        assert!(out.contains("emit Event(1)"), "got: {out}");
        assert_idempotent(src);
    }
}
