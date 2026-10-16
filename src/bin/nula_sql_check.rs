//! Experimental SQL contract preparation for literal Nulang DB effect calls.
//! This is a dev/build-time tool. It never changes runtime query execution.

use nulang::lexer::{Lexer, Token, TokenKind};
use postgres::{Client, NoTls};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgumentType {
    Int,
    Float,
    Bool,
    Text,
    Null,
    Dynamic,
}

#[derive(Debug)]
struct QueryCall {
    sql: String,
    line: usize,
    column: usize,
    args: Vec<ArgumentType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ResultColumn {
    name: String,
    postgres_type_oid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PreparedQuery {
    sql_hash: String,
    parameter_type_oids: Vec<u32>,
    columns: Vec<ResultColumn>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct QueryCache {
    version: u32,
    queries: Vec<PreparedQuery>,
}

fn is_db_query_operation(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident(name)
            if matches!(name.as_str(), "query" | "query_checked" | "query_checked_async")
    )
}

fn infer_argument(tokens: &[Token]) -> ArgumentType {
    match tokens {
        [Token { kind: TokenKind::IntLit(_), .. }]
        | [Token { kind: TokenKind::Minus, .. }, Token { kind: TokenKind::IntLit(_), .. }] => {
            ArgumentType::Int
        }
        [Token { kind: TokenKind::FloatLit(_), .. }]
        | [Token { kind: TokenKind::Minus, .. }, Token { kind: TokenKind::FloatLit(_), .. }] => {
            ArgumentType::Float
        }
        [Token { kind: TokenKind::BoolLit(_), .. }]
        | [Token { kind: TokenKind::True, .. }]
        | [Token { kind: TokenKind::False, .. }] => ArgumentType::Bool,
        [Token { kind: TokenKind::StringLit(_), .. }] => ArgumentType::Text,
        [Token { kind: TokenKind::NilLit, .. }] => ArgumentType::Null,
        _ => ArgumentType::Dynamic,
    }
}

fn parse_arguments(tokens: &[Token], start: usize) -> Result<Vec<ArgumentType>, String> {
    let mut args = Vec::new();
    let mut nested = Vec::new();
    let mut arg_start = start;

    for i in start..tokens.len() {
        match &tokens[i].kind {
            TokenKind::LParen => nested.push(')'),
            TokenKind::LBracket => nested.push(']'),
            TokenKind::LBrace => nested.push('}'),
            TokenKind::RParen if nested.is_empty() => {
                if i == arg_start {
                    return Err("empty SQL bind parameter after comma".into());
                }
                args.push(infer_argument(&tokens[arg_start..i]));
                return Ok(args);
            }
            TokenKind::RParen => {
                if nested.pop() != Some(')') {
                    return Err("mismatched delimiters in SQL arguments".into());
                }
            }
            TokenKind::RBracket => {
                if nested.pop() != Some(']') {
                    return Err("mismatched delimiters in SQL arguments".into());
                }
            }
            TokenKind::RBrace => {
                if nested.pop() != Some('}') {
                    return Err("mismatched delimiters in SQL arguments".into());
                }
            }
            TokenKind::Comma if nested.is_empty() => {
                if i == arg_start {
                    return Err("empty SQL bind parameter".into());
                }
                args.push(infer_argument(&tokens[arg_start..i]));
                arg_start = i + 1;
            }
            _ => {}
        }
    }
    Err("unterminated SQL query call".into())
}

/// Only explicitly written DB effects with a literal first argument are checked.
/// The Nulang lexer excludes comments and treats string content as one token.
/// Dynamic SQL fails closed, instead of being mislabeled as checked SQL.
fn source_position(source: &str, offset: usize) -> (usize, usize) {
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
    let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, column)
}

fn extract_queries(source: &str) -> Result<Vec<QueryCall>, String> {
    let tokens = Lexer::new(source)
        .lex()
        .map_err(|e| format!("Nulang lexer failed: {e}"))?;
    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|t| !matches!(&t.kind, TokenKind::Newline | TokenKind::DocComment(_)))
        .collect();
    let mut queries = Vec::new();

    for i in 0..tokens.len().saturating_sub(4) {
        if !matches!(&tokens[i].kind, TokenKind::Perform)
            || !matches!(&tokens[i + 1].kind, TokenKind::UpperIdent(name) if name == "DB")
            || !matches!(&tokens[i + 2].kind, TokenKind::Dot)
            || !is_db_query_operation(&tokens[i + 3].kind)
            || !matches!(&tokens[i + 4].kind, TokenKind::LParen)
        {
            continue;
        }
        let offset = tokens[i].span.start as usize;
        let (line, column) = source_position(source, offset);
        let sql = match tokens.get(i + 5).map(|t| &t.kind) {
            Some(TokenKind::StringLit(value)) => value.clone(),
            _ => {
                return Err(format!(
                    "line {line}, column {column}: DB query SQL must be a literal string to be checked"
                ));
            }
        };
        let args = match tokens.get(i + 6).map(|t| &t.kind) {
            Some(TokenKind::RParen) => Vec::new(),
            Some(TokenKind::Comma) => parse_arguments(&tokens, i + 7)
                .map_err(|e| format!("line {line}, column {column}: {e}"))?,
            _ => return Err(format!("line {line}, column {column}: malformed DB query call")),
        };
        queries.push(QueryCall { sql, line, column, args });
    }
    Ok(queries)
}

fn fingerprint(sql: &str) -> String {
    blake3::hash(sql.as_bytes()).to_hex().to_string()
}

fn literal_compatible(argument: ArgumentType, oid: u32) -> bool {
    match argument {
        // Conservative checks: the actual runtime binder must still accept
        // the values. Do not claim all PostgreSQL implicit casts are safe.
        ArgumentType::Int => oid == 20, // int8
        ArgumentType::Float => oid == 701, // float8
        ArgumentType::Bool => oid == 16, // bool
        ArgumentType::Text => matches!(oid, 25 | 1043), // text, varchar
        ArgumentType::Null | ArgumentType::Dynamic => true,
    }
}

fn verify_queries(calls: &[QueryCall], cache: &QueryCache) -> Result<usize, String> {
    if cache.version != FORMAT_VERSION {
        return Err(format!("unsupported SQL cache version {}", cache.version));
    }
    let mut entries = BTreeMap::new();
    for entry in &cache.queries {
        if entries.insert(entry.sql_hash.as_str(), entry).is_some() {
            return Err("duplicate SQL cache fingerprint".into());
        }
    }
    let mut used = BTreeSet::new();
    for call in calls {
        let hash = fingerprint(&call.sql);
        let entry = entries
            .get(hash.as_str())
            .ok_or_else(|| format!("line {}, column {}: SQL not present in cache; run prepare", call.line, call.column))?;
        if entry.parameter_type_oids.len() != call.args.len() {
            return Err(format!(
                "line {}, column {}: SQL expects {} bind parameters, but call has {}",
                call.line,
                call.column,
                entry.parameter_type_oids.len(),
                call.args.len()
            ));
        }
        for (i, (argument, oid)) in call
            .args
            .iter()
            .zip(&entry.parameter_type_oids)
            .enumerate()
        {
            if !literal_compatible(*argument, *oid) {
                return Err(format!(
                    "line {}, column {}: parameter {} has incompatible literal type for PostgreSQL OID {}",
                    call.line,
                    call.column,
                    i + 1,
                    oid
                ));
            }
        }
        used.insert(hash);
    }
    if used.len() != entries.len() {
        return Err("SQL cache contains queries absent from source; run prepare".into());
    }
    Ok(calls.len())
}

fn prepare_queries(calls: &[QueryCall], client: &mut Client) -> Result<QueryCache, String> {
    let mut queries = BTreeMap::new();
    for call in calls {
        let hash = fingerprint(&call.sql);
        if queries.contains_key(&hash) {
            continue;
        }
        // PostgreSQL's prepare/describe protocol parses and analyzes SQL but
        // does not execute the statement, including writes. No user arguments
        // are sent to the database during contract preparation.
        let statement = client.prepare(&call.sql).map_err(|e| {
            format!(
                "line {}, column {}: PostgreSQL rejected SQL ({:?})",
                call.line,
                call.column,
                e.code()
            )
        })?;
        queries.insert(
            hash.clone(),
            PreparedQuery {
                sql_hash: hash,
                parameter_type_oids: statement.params().iter().map(|ty| ty.oid()).collect(),
                columns: statement
                    .columns()
                    .iter()
                    .map(|col| ResultColumn {
                        name: col.name().to_owned(),
                        postgres_type_oid: col.type_().oid(),
                    })
                    .collect(),
            },
        );
    }
    let cache = QueryCache {
        version: FORMAT_VERSION,
        queries: queries.into_values().collect(),
    };
    verify_queries(calls, &cache)?;
    Ok(cache)
}

fn write_cache(path: &Path, cache: &QueryCache) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| format!("cache directory: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(cache).map_err(|e| e.to_string())?;
    // Store only hashes and PostgreSQL type metadata, never credentials or SQL.
    let tmp = path.with_extension("sql-cache.tmp");
    fs::write(&tmp, bytes).map_err(|e| format!("cache write: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("cache rename: {e}"))
}

fn main_impl() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let mode = args.next().ok_or_else(|| "expected prepare or check".to_owned())?;
    if mode != "prepare" && mode != "check" {
        return Err("expected prepare or check".into());
    }
    let source_path = args.next().ok_or_else(|| "expected Nulang source path".to_owned())?;
    let mut cache_path = PathBuf::from(".nula/sql-cache.json");
    let mut allow_plaintext = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cache" => {
                cache_path = PathBuf::from(
                    args.next().ok_or_else(|| "--cache requires a path".to_owned())?,
                );
            }
            "--allow-plaintext" => allow_plaintext = true,
            _ => return Err(format!("unknown option: {arg}")),
        }
    }

    let source = fs::read_to_string(&source_path)
        .map_err(|e| format!("source read failed: {e}"))?;
    let calls = extract_queries(&source)?;
    if mode == "prepare" {
        // This initial spike deliberately uses the existing postgres crate's
        // NoTls adapter. Requiring an explicit opt-in prevents silent use over
        // a remote unencrypted connection. Use a local dev DB or SSH tunnel.
        if !allow_plaintext {
            return Err(
                "prepare uses NoTls; pass --allow-plaintext only for a local database or secure tunnel"
                    .into(),
            );
        }
        let url = env::var("NULANG_SQL_DATABASE_URL")
            .map_err(|_| "set NULANG_SQL_DATABASE_URL for prepare".to_owned())?;
        let mut client = Client::connect(&url, NoTls)
            .map_err(|_| "PostgreSQL connection failed (credentials and URI hidden)".to_owned())?;
        let cache = prepare_queries(&calls, &mut client)?;
        write_cache(&cache_path, &cache)?;
        println!("prepared {} checked SQL calls", calls.len());
    } else {
        if allow_plaintext {
            return Err("--allow-plaintext is only valid with prepare".into());
        }
        let bytes = fs::read(&cache_path).map_err(|_| "SQL cache missing; run prepare".to_owned())?;
        let cache: QueryCache =
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid SQL cache: {e}"))?;
        let count = verify_queries(&calls, &cache)?;
        println!("verified {count} SQL calls against cached PostgreSQL metadata");
    }
    Ok(())
}

fn main() {
    if let Err(error) = main_impl() {
        eprintln!("nula-sql-check: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_for(sql: &str, parameters: Vec<u32>) -> QueryCache {
        QueryCache {
            version: FORMAT_VERSION,
            queries: vec![PreparedQuery {
                sql_hash: fingerprint(sql),
                parameter_type_oids: parameters,
                columns: vec![ResultColumn {
                    name: "id".into(),
                    postgres_type_oid: 20,
                }],
            }],
        }
    }

    #[test]
    fn extracts_literal_effect_not_comments_or_strings() {
        let src = r#"
            // perform DB.query("SELECT invalid")
            fn demo() {
                let ignored = "perform DB.query(\"SELECT nope\")"
                perform DB.query("SELECT $1::int8", 7)
            }
        "#;
        let calls = extract_queries(src).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].sql, "SELECT $1::int8");
        assert!(calls[0].line > 1);
        assert!(calls[0].column > 1);
        assert_eq!(calls[0].args, vec![ArgumentType::Int]);
    }

    #[test]
    fn counts_nested_arguments_without_splitting_internal_commas() {
        let src = r#"fn f() { perform DB.query("SELECT $1::int8, $2::int8", add(1, 2), 3) }"#;
        let calls = extract_queries(src).unwrap();
        assert_eq!(calls[0].args, vec![ArgumentType::Dynamic, ArgumentType::Int]);
    }

    #[test]
    fn rejects_dynamic_sql_instead_of_claiming_to_check_it() {
        let src = "fn f(sql: String) { perform DB.query(sql, 1) }";
        assert!(extract_queries(src).unwrap_err().contains("literal string"));
    }

    #[test]
    fn offline_cache_rejects_changed_sql_and_parameter_count() {
        let calls = extract_queries(r#"fn f() { perform DB.query("SELECT $1::int8", 5) }"#).unwrap();
        assert!(verify_queries(&calls, &cache_for("SELECT $2::int8", vec![20])).is_err());
        assert!(verify_queries(&calls, &cache_for("SELECT $1::int8", vec![20, 20]))
            .unwrap_err()
            .contains("expects 2"));
    }

    #[test]
    fn offline_cache_rejects_incompatible_literal_type() {
        let calls = extract_queries(
            r#"fn f() { perform DB.query_checked("SELECT $1::int8", "wrong") }"#,
        )
        .unwrap();
        assert!(verify_queries(&calls, &cache_for("SELECT $1::int8", vec![20]))
            .unwrap_err()
            .contains("incompatible"));
    }

    #[test]
    fn offline_cache_accepts_dynamic_argument_but_does_not_type_check_it() {
        let calls = extract_queries(
            r#"fn f(id: Int) { perform DB.query_checked_async("SELECT $1::int8", id) }"#,
        )
        .unwrap();
        assert_eq!(
            verify_queries(&calls, &cache_for("SELECT $1::int8", vec![20])).unwrap(),
            1
        );
    }

    #[test]
    fn rejects_old_and_unreferenced_cache_entries() {
        let calls = extract_queries(r#"fn f() { perform DB.query("SELECT 1") }"#).unwrap();
        let mut cache = cache_for("SELECT 1", vec![]);
        cache.version = 99;
        assert!(verify_queries(&calls, &cache).is_err());
        cache.version = FORMAT_VERSION;
        cache.queries.push(PreparedQuery {
            sql_hash: fingerprint("SELECT 2"),
            parameter_type_oids: vec![],
            columns: vec![],
        });
        assert!(verify_queries(&calls, &cache)
            .unwrap_err()
            .contains("absent from source"));
    }

    #[test]
    fn rejects_trailing_empty_bind_argument() {
        assert!(extract_queries(r#"fn f() { perform DB.query("SELECT 1",) }"#).is_err());
    }
}
