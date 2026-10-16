//! Performance budget enforcement for Nulang Web builds.
//!
//! Parses optional `[budgets]` from `Nulang.toml` and checks emitted assets
//! against declared limits. `nula build --web` fails when a budget is exceeded.

use std::path::Path;

/// A single budget violation found during the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetViolation {
    pub file: String,
    pub size: usize,
    pub budget: usize,
}

/// Check the initial JavaScript transfer budget against all `.js` files in
/// `output_dir/assets/` and the generated `app.client.js` in `output_dir`.
///
/// Returns `Ok(())` when there is no budget, no JS files, or all files fit.
pub fn check_initial_js_budget(
    output_dir: &Path,
    max_bytes: Option<usize>,
) -> Result<(), Vec<BudgetViolation>> {
    let Some(max) = max_bytes else {
        return Ok(());
    };
    let mut violations = Vec::new();

    let assets_dir = output_dir.join("assets");
    if assets_dir.is_dir() {
        let entries = match std::fs::read_dir(&assets_dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("js") {
                if let Ok(meta) = std::fs::metadata(&p) {
                    let size = meta.len() as usize;
                    if size > max {
                        violations.push(BudgetViolation {
                            file: p.display().to_string(),
                            size,
                            budget: max,
                        });
                    }
                }
            }
        }
    }

    let client_js = output_dir.join("app.client.js");
    if let Ok(meta) = std::fs::metadata(&client_js) {
        let size = meta.len() as usize;
        if size > max {
            violations.push(BudgetViolation {
                file: client_js.display().to_string(),
                size,
                budget: max,
            });
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
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
    fn total_js_budget_rejects_combined_split_bundles() {
        let dir = std::env::temp_dir().join("nulang_budget_total_split");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("app.client.js"), "x".repeat(40)).unwrap();
        std::fs::write(dir.join("assets/first.js"), "x".repeat(90)).unwrap();
        std::fs::write(dir.join("assets/second.js"), "x".repeat(90)).unwrap();

        // The legacy per-file budget deliberately keeps its existing semantics.
        assert!(check_initial_js_budget(&dir, Some(200)).is_ok());
        let violation = check_total_js_budget(&dir, Some(200))
            .unwrap()
            .expect("combined JavaScript must exceed 200 bytes");
        assert_eq!(violation.size, 220);
        assert_eq!(violation.budget, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn total_js_budget_counts_nested_modules_but_not_stylesheets() {
        let dir = std::env::temp_dir().join("nulang_budget_total_nested");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets/chunks")).unwrap();
        std::fs::write(dir.join("app.client.js"), "x".repeat(10)).unwrap();
        std::fs::write(dir.join("assets/main.js"), "x".repeat(50)).unwrap();
        std::fs::write(dir.join("assets/chunks/lazy.mjs"), "x".repeat(90)).unwrap();
        std::fs::write(dir.join("assets/chunks/styles.css"), "x".repeat(999)).unwrap();

        assert!(check_total_js_budget(&dir, Some(150)).unwrap().is_none());
        assert_eq!(
            check_total_js_budget(&dir, Some(149))
                .unwrap()
                .expect("must count nested .mjs")
                .size,
            150
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn total_js_budget_does_not_silently_ignore_unreadable_asset_roots() {
        let dir = std::env::temp_dir().join("nulang_budget_total_bad_assets");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("assets"), "not a directory").unwrap();

        let err = check_total_js_budget(&dir, Some(100)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn total_js_budget_opt_out_does_not_access_missing_output() {
        let dir = std::env::temp_dir().join("nulang_budget_total_unconfigured");
        assert!(check_total_js_budget(&dir, None).unwrap().is_none());
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
}
