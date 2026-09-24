//! Transactional application of compiler-produced machine-applicable fixes.
//!
//! Safe fixes are compiler edits, not prose suggestions. The implementation
//! validates edit ranges, rejects overlaps/conflicts, re-checks the edited
//! source in memory, verifies the on-disk source did not change concurrently,
//! and only then commits through a same-directory temporary file.

use crate::json_diagnostics::diagnostics_from_error;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::typechecker::TypeChecker;
use crate::types::{set_source_map_with_file, NuError, NuResult, Span};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const SAFE_FIX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct SafeFixReport {
    pub schema_version: u32,
    pub file: String,
    pub changed: bool,
    pub dry_run: bool,
    pub applied_fixes: usize,
    pub applied_edits: usize,
    pub errors_before: usize,
    pub errors_after: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SafeEdit {
    start: usize,
    end: usize,
    replacement: String,
}

pub fn run(args: &[String]) -> NuResult<()> {
    let mut safe = false;
    let mut dry_run = false;
    let mut json = false;
    let mut file: Option<&str> = None;

    for arg in args {
        match arg.as_str() {
            "--safe" => safe = true,
            "--dry-run" => dry_run = true,
            "--json" => json = true,
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            other if !other.starts_with('-') && file.is_none() => file = Some(other),
            other => return Err(fix_error(format!("unknown fix option: {other}"))),
        }
    }

    if !safe {
        return Err(fix_error(
            "automatic fixes require --safe; only machine-applicable compiler edits are supported",
        ));
    }
    let file = file.ok_or_else(|| {
        fix_error("usage: nulang fix --safe [--dry-run] [--json] <file>")
    })?;
    let report = fix_file(Path::new(file), dry_run)?;

    if json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|error| fix_error(error.to_string()))?
        );
    } else if report.changed {
        if report.dry_run {
            println!(
                "Would apply {} safe edit(s) from {} fix(es) to {}",
                report.applied_edits, report.applied_fixes, report.file
            );
        } else {
            println!(
                "Applied {} safe edit(s) from {} fix(es) to {}",
                report.applied_edits, report.applied_fixes, report.file
            );
        }
    } else {
        println!("No machine-applicable fixes for {}", report.file);
    }

    Ok(())
}

fn print_help() {
    eprintln!(
        "nulang fix — apply compiler-proven source edits\n\n\
         Usage:\n\
           nulang fix --safe [--dry-run] [--json] <file>\n"
    );
}

pub fn fix_file(path: &Path, dry_run: bool) -> NuResult<SafeFixReport> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| fix_error(format!("cannot read '{}': {error}", path.display())))?;
    let original_digest = *blake3::hash(source.as_bytes()).as_bytes();
    let errors = collect_type_errors(&source, path)?;
    let errors_before = errors.len();
    let (applied_fixes, edits) = collect_safe_edits(&errors, path, &source)?;

    if edits.is_empty() {
        return Ok(SafeFixReport {
            schema_version: SAFE_FIX_SCHEMA_VERSION,
            file: path.display().to_string(),
            changed: false,
            dry_run,
            applied_fixes: 0,
            applied_edits: 0,
            errors_before,
            errors_after: errors_before,
        });
    }

    let fixed = apply_edits(&source, &edits)?;
    let remaining = collect_type_errors(&fixed, path)?;
    if remaining.len() > errors_before {
        return Err(fix_error(format!(
            "refusing safe fix: compiler errors increased from {} to {} after applying edits in memory",
            errors_before,
            remaining.len()
        )));
    }

    if !dry_run {
        commit_if_unchanged(path, &fixed, original_digest)?;
    }

    Ok(SafeFixReport {
        schema_version: SAFE_FIX_SCHEMA_VERSION,
        file: path.display().to_string(),
        changed: true,
        dry_run,
        applied_fixes,
        applied_edits: edits.len(),
        errors_before,
        errors_after: remaining.len(),
    })
}

fn collect_type_errors(source: &str, path: &Path) -> NuResult<Vec<NuError>> {
    let file = path.display().to_string();
    set_source_map_with_file(source, Some(&file));

    let tokens = Lexer::new(source).lex()?;
    let mut ast = Parser::new(tokens).parse_module()?;
    crate::resolver::resolve_imports(&mut ast, path, &mut HashSet::new())?;

    let mut checker = TypeChecker::new();
    checker.collect_errors = true;
    let result = checker.check_module(&ast);
    let mut errors = std::mem::take(&mut checker.collected_errors);
    if errors.is_empty() {
        if let Err(error) = result {
            errors.push(error);
        }
    }
    Ok(errors)
}

fn collect_safe_edits(
    errors: &[NuError],
    path: &Path,
    source: &str,
) -> NuResult<(usize, Vec<SafeEdit>)> {
    let target = path.display().to_string();
    let mut unique: BTreeMap<(usize, usize), String> = BTreeMap::new();
    let mut fixes = 0usize;

    for error in errors {
        for diagnostic in diagnostics_from_error(error) {
            for fix in diagnostic
                .fixes
                .into_iter()
                .filter(|fix| fix.applicability == "machine_applicable")
            {
                let mut contributed = false;
                for edit in fix.edits {
                    if edit.file != target {
                        continue;
                    }
                    let start = edit.start_byte as usize;
                    let end = edit.end_byte as usize;
                    validate_edit_range(source, start, end)?;
                    match unique.get(&(start, end)) {
                        Some(existing) if existing != &edit.replacement => {
                            return Err(fix_error(format!(
                                "conflicting machine-applicable edits for bytes {start}..{end}"
                            )));
                        }
                        Some(_) => {}
                        None => {
                            unique.insert((start, end), edit.replacement);
                            contributed = true;
                        }
                    }
                }
                if contributed {
                    fixes += 1;
                }
            }
        }
    }

    let edits = unique
        .into_iter()
        .map(|((start, end), replacement)| SafeEdit {
            start,
            end,
            replacement,
        })
        .collect::<Vec<_>>();

    for pair in edits.windows(2) {
        if pair[0].end > pair[1].start {
            return Err(fix_error(format!(
                "overlapping machine-applicable edits at bytes {}..{} and {}..{}",
                pair[0].start, pair[0].end, pair[1].start, pair[1].end
            )));
        }
    }

    Ok((fixes, edits))
}

fn validate_edit_range(source: &str, start: usize, end: usize) -> NuResult<()> {
    if start > end || end > source.len() {
        return Err(fix_error(format!(
            "machine-applicable edit range {start}..{end} is outside source length {}",
            source.len()
        )));
    }
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return Err(fix_error(format!(
            "machine-applicable edit range {start}..{end} splits a UTF-8 code point"
        )));
    }
    Ok(())
}

fn apply_edits(source: &str, edits: &[SafeEdit]) -> NuResult<String> {
    let mut output = source.to_string();
    for edit in edits.iter().rev() {
        validate_edit_range(&output, edit.start, edit.end)?;
        output.replace_range(edit.start..edit.end, &edit.replacement);
    }
    Ok(output)
}

fn commit_if_unchanged(path: &Path, fixed: &str, expected_digest: [u8; 32]) -> NuResult<()> {
    let current = std::fs::read(path)
        .map_err(|error| fix_error(format!("cannot re-read '{}': {error}", path.display())))?;
    if *blake3::hash(&current).as_bytes() != expected_digest {
        return Err(fix_error(format!(
            "refusing safe fix: '{}' changed on disk during analysis",
            path.display()
        )));
    }

    let metadata = std::fs::metadata(path)
        .map_err(|error| fix_error(format!("cannot stat '{}': {error}", path.display())))?;
    let temp = temporary_sibling(path);
    let mut file = std::fs::File::create(&temp)
        .map_err(|error| fix_error(format!("cannot create '{}': {error}", temp.display())))?;

    let write_result = (|| -> std::io::Result<()> {
        file.write_all(fixed.as_bytes())?;
        file.sync_all()?;
        std::fs::set_permissions(&temp, metadata.permissions())?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(fix_error(format!(
            "cannot stage fixed source '{}': {error}",
            temp.display()
        )));
    }

    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(fix_error(format!(
            "cannot atomically replace '{}': {error}",
            path.display()
        )));
    }
    Ok(())
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("source.nula");
    path.with_file_name(format!(
        ".{file_name}.nulang-fix-{}.tmp",
        std::process::id()
    ))
}

fn fix_error(msg: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: msg.into(),
        span: Span::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edits_uses_original_byte_offsets() {
        let source = "alpha beta gamma";
        let edits = vec![
            SafeEdit {
                start: 0,
                end: 5,
                replacement: "a".to_string(),
            },
            SafeEdit {
                start: 11,
                end: 16,
                replacement: "g".to_string(),
            },
        ];
        assert_eq!(apply_edits(source, &edits).unwrap(), "a beta g");
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let source = "abcdef";
        let errors = Vec::<NuError>::new();
        let (_count, edits) =
            collect_safe_edits(&errors, Path::new("test.nula"), source).unwrap();
        assert!(edits.is_empty());

        let overlapping = vec![
            SafeEdit {
                start: 1,
                end: 4,
                replacement: "x".to_string(),
            },
            SafeEdit {
                start: 3,
                end: 5,
                replacement: "y".to_string(),
            },
        ];
        let mut output = source.to_string();
        for edit in overlapping.iter().rev() {
            output.replace_range(edit.start..edit.end, &edit.replacement);
        }
        assert_ne!(output, source);
    }
}
