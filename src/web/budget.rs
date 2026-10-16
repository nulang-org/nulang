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

/// Apply the legacy per-file JavaScript size budget to `.js` files in
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

/// Enforce an optional budget on *all emitted*, uncompressed JavaScript,
/// including lazily loaded chunks. Unlike `initial_js` this measures the
/// output footprint, not the bytes required for the first page load.
///
/// Every .js/.mjs file anywhere in the published output tree counts,
/// including the generated client runtime, root-level static scripts, and lazy
/// chunks. Symlinked paths and unreadable entries fail closed so
/// missing bytes cannot make a configured budget appear to pass.
pub fn check_total_js_budget(
    output_dir: &Path,
    max_bytes: Option<usize>,
) -> std::io::Result<Option<BudgetViolation>> {
    let Some(budget) = max_bytes else {
        return Ok(None);
    };

    let mut total = 0usize;
    count_asset_js(output_dir, &mut total)?;

    Ok((total > budget).then(|| BudgetViolation {
        file: format!("{} (total emitted JavaScript)", output_dir.display()),
        size: total,
        budget,
    }))
}

fn invalid_js_asset(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "invalid or symlinked JavaScript asset path: {}",
            path.display()
        ),
    )
}

fn add_js_file_size(path: &Path, total: &mut usize) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid_js_asset(path));
    }
    let bytes = usize::try_from(metadata.len()).map_err(|_| invalid_js_asset(path))?;
    *total = total.checked_add(bytes).ok_or_else(|| invalid_js_asset(path))?;
    Ok(())
}

fn count_asset_js(directory: &Path, total: &mut usize) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid_js_asset(directory));
    }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(invalid_js_asset(&path));
        }
        if file_type.is_dir() {
            count_asset_js(&path, total)?;
        } else if file_type.is_file()
            && matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("js" | "mjs")
            )
        {
            add_js_file_size(&path, total)?;
        }
    }
    Ok(())
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
    fn total_js_budget_counts_root_level_published_scripts() {
        let dir = std::env::temp_dir().join("nulang_budget_total_root_scripts");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.client.js"), "x".repeat(20)).unwrap();
        std::fs::write(dir.join("analytics.js"), "x".repeat(30)).unwrap();

        assert_eq!(
            check_total_js_budget(&dir, Some(40))
                .unwrap()
                .expect("root-level published script must count")
                .size,
            50
        );
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

    #[cfg(unix)]
    #[test]
    fn total_js_budget_rejects_symlinked_output_entries() {
        let dir = std::env::temp_dir().join("nulang_budget_total_symlink");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("outside-root", dir.join("assets")).unwrap();

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
