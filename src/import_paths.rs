//! Shared named-import lookup for the parser and declaration resolver.
//! Relative-path and unconfigured-package fallbacks remain caller-specific.

use std::path::PathBuf;

pub(crate) fn resolve_named(import: &str) -> Option<PathBuf> {
    if let Some(module) = import.strip_prefix("stdlib::") {
        let module_path = module.replace("::", std::path::MAIN_SEPARATOR_STR);
        let filename = format!("{module_path}.nula");
        if let Ok(dir) = std::env::var("NULANG_STDLIB") {
            return Some(PathBuf::from(dir).join(&filename));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let candidate = exe_dir.join("stdlib").join(&filename);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            let candidate = cwd.join("src").join("stdlib").join(&filename);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        return Some(PathBuf::from("src/stdlib").join(filename));
    }
    let module = import.strip_prefix("@nulang/")?;
    configured_module(
        module,
        &std::env::var("NULANG_MODULE_PATH").unwrap_or_default(),
    )
}

fn configured_module(module: &str, entries: &str) -> Option<PathBuf> {
    for entry in entries.split(';') {
        let Some((name, dir)) = entry.trim().split_once('=') else {
            continue;
        };
        let (name, dir) = (name.trim(), dir.trim());
        if name.is_empty() || dir.is_empty() {
            continue;
        }
        let name = name.strip_prefix("@nulang/").unwrap_or(name);
        if module == name || module.starts_with(&format!("{name}/")) {
            let rest = module
                .strip_prefix(name)
                .unwrap_or(module)
                .trim_start_matches('/');
            let file = if rest.is_empty() {
                "lib.nula".to_owned()
            } else {
                format!("{}.nula", rest.replace('/', std::path::MAIN_SEPARATOR_STR))
            };
            return Some(PathBuf::from(dir).join(file));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_module_preserves_first_match_and_prefix_boundaries() {
        let entries = "bad;auth=first;@nulang/auth=second;authentication=third";
        assert_eq!(
            configured_module("auth", entries),
            Some(PathBuf::from("first").join("lib.nula"))
        );
        assert_eq!(
            configured_module("auth/session", entries),
            Some(PathBuf::from("first").join("session.nula"))
        );
        assert_eq!(
            configured_module("authentication", entries),
            Some(PathBuf::from("third").join("lib.nula"))
        );
        assert_eq!(configured_module("authentic", entries), None);
    }
}
