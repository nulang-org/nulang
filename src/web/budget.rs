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
