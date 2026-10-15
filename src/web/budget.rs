//! Performance budget enforcement for Nulang Web builds.
//!
//! Parses optional `[budgets]` from `Nulang.toml` and checks emitted assets
//! against declared limits. `nula build --web` fails when a budget is exceeded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A single budget violation found during the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetViolation {
    pub file: String,
    pub size: usize,
    pub budget: usize,
}

/// Check the initial JavaScript transfer budget for one complete output tree.
pub fn check_initial_js_budget(
    output_dir: &Path,
    max_bytes: Option<usize>,
) -> Result<(), Vec<BudgetViolation>> {
    check_initial_js_budget_with_retained_output(output_dir, output_dir, max_bytes)
}

/// Check the JavaScript budget for the output that would result from publishing
/// `staged_output` over `retained_output`.
///
/// Files in the stage win on path collision; retained files absent from the
/// stage remain part of the candidate output and cannot bypass its budget.
pub fn check_initial_js_budget_with_retained_output(
    staged_output: &Path,
    retained_output: &Path,
    max_bytes: Option<usize>,
) -> Result<(), Vec<BudgetViolation>> {
    let Some(max) = max_bytes else {
        return Ok(());
    };
    let mut violations = Vec::new();
    let mut assets = BTreeMap::new();
    collect_initial_js_assets(&retained_output.join("assets"), &mut assets);
    collect_initial_js_assets(&staged_output.join("assets"), &mut assets);
    for path in assets.into_values() {
        collect_budget_violation(&path, max, &mut violations);
    }

    let staged_client = staged_output.join("app.client.js");
    let retained_client = retained_output.join("app.client.js");
    let client_js = if staged_client.is_file() {
        &staged_client
    } else {
        &retained_client
    };
    collect_budget_violation(client_js, max, &mut violations);

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn collect_initial_js_assets(dir: &Path, assets: &mut BTreeMap<std::ffi::OsString, PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("js") {
            assets.insert(entry.file_name(), path);
        }
    }
}

fn collect_budget_violation(path: &Path, max: usize, violations: &mut Vec<BudgetViolation>) {
    if let Ok(metadata) = std::fs::metadata(path) {
        let size = metadata.len() as usize;
        if size > max {
            violations.push(BudgetViolation {
                file: path.display().to_string(),
                size,
                budget: max,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_budget_passes() {
        let dir = std::env::temp_dir().join("nulang_budget_none");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.client.js"), "x".repeat(1000)).unwrap();
        assert!(check_initial_js_budget(&dir, None).is_ok());
    }

    #[test]
    fn test_under_budget_passes() {
        let dir = std::env::temp_dir().join("nulang_budget_ok");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.client.js"), "x".repeat(100)).unwrap();
        assert!(check_initial_js_budget(&dir, Some(200)).is_ok());
    }

    #[test]
    fn test_over_budget_fails() {
        let dir = std::env::temp_dir().join("nulang_budget_fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.client.js"), "x".repeat(300)).unwrap();
        let err = check_initial_js_budget(&dir, Some(200)).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].size, 300);
        assert_eq!(err[0].budget, 200);
    }

    #[test]
    fn test_staged_budget_counts_retained_assets_unless_replaced() {
        let root =
            std::env::temp_dir().join(format!("nulang_budget_retained_{}", std::process::id()));
        let stage = root.join("stage");
        let retained = root.join("dist");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(stage.join("assets")).unwrap();
        std::fs::create_dir_all(retained.join("assets")).unwrap();
        std::fs::write(retained.join("assets/app.js"), "x".repeat(300)).unwrap();

        let err = check_initial_js_budget_with_retained_output(&stage, &retained, Some(200))
            .expect_err("retained JavaScript must be budgeted");
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].size, 300);

        std::fs::write(stage.join("assets/app.js"), "x".repeat(100)).unwrap();
        assert!(
            check_initial_js_budget_with_retained_output(&stage, &retained, Some(200)).is_ok(),
            "staged files must replace retained files at the same output path"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
