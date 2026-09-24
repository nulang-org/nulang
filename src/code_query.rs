//! Machine-readable source queries for coding agents and editor tooling.
//!
//! This module intentionally lives in core and has no dependency on the optional
//! AI runtime. The CLI surface is `nulang query ...`; consumers can depend on
//! its stable JSON shape without enabling LLM/provider features.

use crate::ast::{Decl, Param};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::types::{NuError, NuResult, Span};
use serde::Serialize;
use std::path::Path;

pub const QUERY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct QueryReport {
    pub schema_version: u32,
    pub command: String,
    pub file: String,
    pub ok: bool,
    pub symbols: Vec<QuerySymbol>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuerySymbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effects: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    pub span: QuerySpan,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuerySpan {
    pub file: String,
    pub start_byte: u32,
    pub end_byte: u32,
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

fn query_err(msg: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: msg.into(),
        span: Span::default(),
    }
}

/// Dispatch `nulang query` subcommands.
///
/// Supported commands:
/// - `symbols <file> [--name <substring>] [--json]`
/// - `symbol <name> <file> [--json]`
pub fn run(args: &[String]) -> NuResult<()> {
    match args.first().map(String::as_str) {
        Some("symbols") => cmd_symbols(args.get(1..).unwrap_or(&[])),
        Some("symbol") => cmd_symbol(args.get(1..).unwrap_or(&[])),
        Some("help") | Some("-h") | Some("--help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(query_err(format!(
            "unknown query subcommand '{other}'; try: symbols, symbol"
        ))),
    }
}

fn print_help() {
    eprintln!(
        "nulang query — machine-readable source inspection\n\n\
         Usage:\n\
           nulang query symbols <file> [--name <substring>] [--json]\n\
           nulang query symbol <name> <file> [--json]\n"
    );
}

fn cmd_symbols(args: &[String]) -> NuResult<()> {
    let mut file: Option<&str> = None;
    let mut name_filter: Option<&str> = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                i += 1;
                name_filter = Some(
                    args.get(i)
                        .map(String::as_str)
                        .ok_or_else(|| query_err("missing value for --name"))?,
                );
            }
            "--json" => json = true,
            other if !other.starts_with('-') && file.is_none() => file = Some(other),
            other => return Err(query_err(format!("unknown query symbols option: {other}"))),
        }
        i += 1;
    }

    let file = file.ok_or_else(|| {
        query_err("usage: nulang query symbols <file> [--name <substring>] [--json]")
    })?;
    let mut report = analyze_file(Path::new(file), "symbols")?;
    if let Some(filter) = name_filter {
        let needle = filter.to_lowercase();
        report.symbols.retain(|s| {
            s.name.to_lowercase().contains(&needle)
                || s.qualified_name.to_lowercase().contains(&needle)
        });
    }
    emit_report(&report, json)
}

fn cmd_symbol(args: &[String]) -> NuResult<()> {
    let mut positional = Vec::new();
    let mut json = false;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else if arg.starts_with('-') {
            return Err(query_err(format!("unknown query symbol option: {arg}")));
        } else {
            positional.push(arg.as_str());
        }
    }

    if positional.len() != 2 {
        return Err(query_err(
            "usage: nulang query symbol <name> <file> [--json]",
        ));
    }
    let requested = positional[0];
    let mut report = analyze_file(Path::new(positional[1]), "symbol")?;
    report
        .symbols
        .retain(|s| s.name == requested || s.qualified_name == requested);
    emit_report(&report, json)
}

fn emit_report(report: &QueryReport, json: bool) -> NuResult<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(report).map_err(|e| query_err(e.to_string()))?
        );
        return Ok(());
    }

    if report.symbols.is_empty() {
        println!("No matching symbols.");
        return Ok(());
    }

    for symbol in &report.symbols {
        let signature = symbol
            .signature
            .as_deref()
            .map(|s| format!("  {s}"))
            .unwrap_or_default();
        println!(
            "{}  {}{}  {}:{}:{}",
            symbol.kind,
            symbol.qualified_name,
            signature,
            symbol.span.file,
            symbol.span.line,
            symbol.span.col
        );
    }
    Ok(())
}

pub fn analyze_file(path: &Path, command: &str) -> NuResult<QueryReport> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| query_err(format!("cannot read '{}': {e}", path.display())))?;

    let tokens = Lexer::new(&source).lex()?;
    let ast = Parser::new(tokens).parse_module()?;
    let file = path.display().to_string();
    let mut symbols = Vec::new();
    collect_decls(&ast.decls, "", &file, &source, &mut symbols);

    Ok(QueryReport {
        schema_version: QUERY_SCHEMA_VERSION,
        command: command.to_string(),
        file,
        ok: true,
        symbols,
    })
}

fn collect_decls(
    decls: &[Decl],
    prefix: &str,
    file: &str,
    source: &str,
    out: &mut Vec<QuerySymbol>,
) {
    for decl in decls {
        match decl {
            Decl::Function {
                name,
                type_params,
                params,
                using_params,
                ret_type,
                error_type,
                effect,
                cap,
                public,
                span,
                ..
            } => {
                let generic = if type_params.is_empty() {
                    String::new()
                } else {
                    format!("[{}]", type_params.join(", "))
                };
                let mut all_params: Vec<String> = params.iter().map(format_param).collect();
                all_params.extend(
                    using_params
                        .iter()
                        .map(|p| format!("using {}", format_param(p))),
                );
                let ret = ret_type
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "_".to_string());
                let mut signature =
                    format!("fn {name}{generic}({}) -> {ret}", all_params.join(", "));
                if let Some(err) = error_type {
                    signature.push_str(&format!(" ! {err}"));
                }
                out.push(symbol(
                    name,
                    prefix,
                    "function",
                    Some(*public),
                    Some(signature),
                    effect.as_ref().map(ToString::to_string),
                    cap.as_ref().map(ToString::to_string),
                    *span,
                    file,
                    source,
                ));
            }
            Decl::Actor {
                name,
                type_params,
                persistent,
                span,
                ..
            } => {
                let generic = if type_params.is_empty() {
                    String::new()
                } else {
                    format!("[{}]", type_params.join(", "))
                };
                let head = if *persistent {
                    "persistent actor"
                } else {
                    "actor"
                };
                out.push(symbol(
                    name,
                    prefix,
                    "actor",
                    None,
                    Some(format!("{head} {name}{generic}")),
                    None,
                    None,
                    *span,
                    file,
                    source,
                ));
            }
            Decl::StateMachine { name, span, .. } => out.push(symbol(
                name,
                prefix,
                "state_machine",
                None,
                Some(format!("state_machine {name}")),
                None,
                None,
                *span,
                file,
                source,
            )),
            Decl::TypeAlias {
                name,
                type_params,
                body,
                opaque,
                public,
                span,
            } => {
                let generic = if type_params.is_empty() {
                    String::new()
                } else {
                    format!("[{}]", type_params.join(", "))
                };
                let kw = if *opaque { "opaque type" } else { "type" };
                out.push(symbol(
                    name,
                    prefix,
                    "type_alias",
                    Some(*public),
                    Some(format!("{kw} {name}{generic} = {body}")),
                    None,
                    None,
                    *span,
                    file,
                    source,
                ));
            }
            Decl::RecordType {
                name,
                type_params,
                fields,
                public,
                span,
                ..
            } => {
                let generic = if type_params.is_empty() {
                    String::new()
                } else {
                    format!("[{}]", type_params.join(", "))
                };
                let body = fields
                    .iter()
                    .map(|(n, t)| format!("{n}: {t}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push(symbol(
                    name,
                    prefix,
                    "record_type",
                    Some(*public),
                    Some(format!("type {name}{generic} = {{ {body} }}")),
                    None,
                    None,
                    *span,
                    file,
                    source,
                ));
            }
            Decl::VariantType {
                name,
                type_params,
                variants,
                public,
                span,
            } => {
                let generic = if type_params.is_empty() {
                    String::new()
                } else {
                    format!("[{}]", type_params.join(", "))
                };
                let body = variants
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}({t})"),
                        None => n.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");
                out.push(symbol(
                    name,
                    prefix,
                    "variant_type",
                    Some(*public),
                    Some(format!("type {name}{generic} = {body}")),
                    None,
                    None,
                    *span,
                    file,
                    source,
                ));
            }
            Decl::EffectDecl { name, span, .. } => out.push(symbol(
                name,
                prefix,
                "effect",
                None,
                Some(format!("effect {name}")),
                None,
                None,
                *span,
                file,
                source,
            )),
            Decl::Module {
                name, decls, span, ..
            } => {
                out.push(symbol(
                    name,
                    prefix,
                    "module",
                    None,
                    Some(format!("module {name}")),
                    None,
                    None,
                    *span,
                    file,
                    source,
                ));
                let nested_prefix = qualify(prefix, name);
                collect_decls(decls, &nested_prefix, file, source, out);
            }
            Decl::Workflow { name, span, .. } => out.push(symbol(
                name,
                prefix,
                "workflow",
                None,
                Some(format!("workflow {name}")),
                None,
                None,
                *span,
                file,
                source,
            )),
            Decl::Agent {
                name, model, span, ..
            } => out.push(symbol(
                name,
                prefix,
                "agent",
                None,
                Some(format!("agent {name} model={model}")),
                None,
                None,
                *span,
                file,
                source,
            )),
            Decl::Database { name, span, .. } => out.push(symbol(
                name,
                prefix,
                "database",
                None,
                Some(format!("database {name}")),
                None,
                None,
                *span,
                file,
                source,
            )),
            Decl::CrdtDecl { name, span, .. } => out.push(symbol(
                name,
                prefix,
                "crdt",
                None,
                Some(format!("crdt {name}")),
                None,
                None,
                *span,
                file,
                source,
            )),
            Decl::NamedHandler { name, span, .. } => out.push(symbol(
                name,
                prefix,
                "handler",
                None,
                Some(format!("handler {name}")),
                None,
                None,
                *span,
                file,
                source,
            )),
            Decl::Class {
                name,
                type_params,
                span,
                ..
            } => {
                let generic = if type_params.is_empty() {
                    String::new()
                } else {
                    format!("[{}]", type_params.join(", "))
                };
                out.push(symbol(
                    name,
                    prefix,
                    "class",
                    None,
                    Some(format!("class {name}{generic}")),
                    None,
                    None,
                    *span,
                    file,
                    source,
                ));
            }
            Decl::LetBinding {
                name,
                type_ann,
                mutable,
                span,
                ..
            } => {
                let kw = if *mutable { "var" } else { "let" };
                out.push(symbol(
                    name,
                    prefix,
                    "binding",
                    None,
                    Some(match type_ann {
                        Some(t) => format!("{kw} {name}: {t}"),
                        None => format!("{kw} {name}"),
                    }),
                    None,
                    None,
                    *span,
                    file,
                    source,
                ));
            }
            Decl::Signal { name, ty, span, .. } => out.push(symbol(
                name,
                prefix,
                "signal",
                None,
                Some(format!("signal {name}: {ty}")),
                None,
                None,
                *span,
                file,
                source,
            )),
            Decl::Given { name, ty, span, .. } => out.push(symbol(
                name,
                prefix,
                "given",
                None,
                Some(match ty {
                    Some(t) => format!("given {name}: {t}"),
                    None => format!("given {name}"),
                }),
                None,
                None,
                *span,
                file,
                source,
            )),
            Decl::Extern { funcs, .. } => {
                for f in funcs {
                    let params = f
                        .params
                        .iter()
                        .map(|(n, t)| format!("{n}: {t}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push(symbol(
                        &f.name,
                        prefix,
                        "extern_function",
                        None,
                        Some(format!("extern fn {}({params}) -> {}", f.name, f.ret)),
                        None,
                        None,
                        f.span,
                        file,
                        source,
                    ));
                }
            }
            Decl::Import { .. } | Decl::Impl { .. } => {}
        }
    }
}

fn symbol(
    name: &str,
    prefix: &str,
    kind: &str,
    public: Option<bool>,
    signature: Option<String>,
    effects: Option<String>,
    capability: Option<String>,
    span: Span,
    file: &str,
    source: &str,
) -> QuerySymbol {
    QuerySymbol {
        name: name.to_string(),
        qualified_name: qualify(prefix, name),
        kind: kind.to_string(),
        public,
        signature,
        effects,
        capability,
        span: query_span(span, file, source),
    }
}

fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

fn format_param(param: &Param) -> String {
    let ty = param
        .ty
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "_".to_string());
    match param.cap {
        Some(cap) => format!("{}: {} {ty}", param.name, cap),
        None => format!("{}: {ty}", param.name),
    }
}

fn query_span(span: Span, file: &str, source: &str) -> QuerySpan {
    let len = source.len() as u32;
    let start = span.start.min(len);
    let end = span.end.min(len).max(start);
    let (line, col) = offset_line_col(source, start);
    let (end_line, end_col) = offset_line_col(source, end);
    QuerySpan {
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
    for (i, &b) in source.as_bytes().iter().enumerate() {
        if i as u32 >= offset {
            break;
        }
        if b == b'\n' {
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
    fn query_span_uses_byte_offsets_and_one_based_positions() {
        let span = Span::new(3, 6);
        let s = "ab\nxyz\n";
        let q = query_span(span, "test.nula", s);
        assert_eq!(q.start_byte, 3);
        assert_eq!(q.end_byte, 6);
        assert_eq!((q.line, q.col), (2, 1));
        assert_eq!((q.end_line, q.end_col), (2, 4));
    }

    #[test]
    fn qualify_nested_symbols() {
        assert_eq!(qualify("", "Foo"), "Foo");
        assert_eq!(qualify("Outer", "Foo"), "Outer::Foo");
    }
}
