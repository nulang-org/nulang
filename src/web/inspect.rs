//! Read-only inspection of the compiler-owned Web deployment IR.
//!
//! Do not rediscover routes by parsing source or executing application code.
//! The emitted IR is the same validated artifact consumed by deployment adapters.

use crate::web::ir::DeploymentIr;
use std::collections::HashSet;
use std::path::PathBuf;

const USAGE: &str =
    "Usage: nulang web routes [--ir FILE] | nulang web inspect METHOD PATH [--ir FILE]";

/// Render a deterministic route list or inspect one exact method/path claim.
///
/// This function does not access the filesystem or execute route handlers.
pub fn inspect_ir_json(source: &str, query: Option<(&str, &str)>) -> Result<String, String> {
    let ir: DeploymentIr =
        serde_json::from_str(source).map_err(|error| format!("invalid deployment IR: {error}"))?;
    if ir.version != 1 {
        return Err(format!(
            "unsupported deployment IR version {}; expected version 1",
            ir.version
        ));
    }

    let mut seen = HashSet::new();
    for route in &ir.routes {
        let displayed = [
            route.method.as_str(),
            route.path.as_str(),
            route.placement.as_str(),
            route.handler.as_deref().unwrap_or("-"),
            route.response_type.as_deref().unwrap_or("-"),
            route.error_type.as_deref().unwrap_or("-"),
            route.artifact.as_deref().unwrap_or("-"),
        ];
        if displayed
            .iter()
            .any(|value| value.chars().any(char::is_control))
            || route
                .effects
                .iter()
                .any(|effect| effect.chars().any(char::is_control))
        {
            return Err(format!(
                "deployment IR route contains a control character in a displayed field: {} {}",
                route.method, route.path
            ));
        }
        if !seen.insert((route.method.as_str(), route.path.as_str())) {
            return Err(format!(
                "duplicate route claim in deployment IR: {} {}",
                route.method, route.path
            ));
        }
    }

    let mut routes: Vec<_> = ir.routes.iter().collect();
    routes.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.method.cmp(&right.method))
    });

    if let Some((method, path)) = query {
        let route = routes
            .into_iter()
            .find(|route| route.method == method && route.path == path)
            .ok_or_else(|| format!("route {method} {path} not found in deployment IR"))?;

        let effects = if route.effects.is_empty() {
            "-".to_string()
        } else {
            route.effects.join(", ")
        };
        return Ok(format!(
            "Route: {} {}\nPlacement: {}\nHandler: {}\nResponse: {}\nError: {}\nEffects: {}\nArtifact: {}",
            route.method,
            route.path,
            route.placement,
            route.handler.as_deref().unwrap_or("-"),
            route.response_type.as_deref().unwrap_or("-"),
            route.error_type.as_deref().unwrap_or("-"),
            effects,
            route.artifact.as_deref().unwrap_or("-")
        ));
    }

    let mut lines = vec!["METHOD\tPATH\tPLACEMENT\tHANDLER\tRESPONSE".to_string()];
    for route in routes {
        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}",
            route.method,
            route.path,
            route.placement,
            route.handler.as_deref().unwrap_or("-"),
            route.response_type.as_deref().unwrap_or("-")
        ));
    }
    Ok(lines.join("\n"))
}

/// Inspect an existing build artifact. This never builds, executes, or publishes a site.
pub fn run_cli(args: &[String]) -> Result<String, String> {
    let (query, path) = parse_cli_args(args)?;
    let source = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "cannot read deployment IR at '{}': {error}; run 'nulang nula build --web' first",
            path.display()
        )
    })?;
    inspect_ir_json(
        &source,
        query
            .as_ref()
            .map(|(method, path)| (method.as_str(), path.as_str())),
    )
}

fn parse_cli_args(args: &[String]) -> Result<(Option<(String, String)>, PathBuf), String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(USAGE.to_string());
    };
    if !matches!(command, "routes" | "inspect") {
        return Err(USAGE.to_string());
    }

    let mut path = PathBuf::from("dist/nulang-app.ir.json");
    let mut path_specified = false;
    let mut operands = Vec::new();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--ir" => {
                if path_specified {
                    return Err(format!("--ir may be specified only once. {USAGE}"));
                }
                index += 1;
                if index >= args.len() || args[index].starts_with('-') {
                    return Err(format!("--ir requires a file path. {USAGE}"));
                }
                path = PathBuf::from(&args[index]);
                path_specified = true;
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown web inspection option '{flag}'. {USAGE}"));
            }
            operand => operands.push(operand.to_string()),
        }
        index += 1;
    }

    match (command, operands.as_slice()) {
        ("routes", []) => Ok((None, path)),
        ("inspect", [method, route_path]) => Ok((Some((method.clone(), route_path.clone())), path)),
        _ => Err(USAGE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_route_list_and_exact_inspection_arguments() {
        let routes = parse_cli_args(&["routes".to_string()]).unwrap();
        assert_eq!(routes.0, None);
        assert_eq!(routes.1, PathBuf::from("dist/nulang-app.ir.json"));

        let detail = parse_cli_args(&[
            "inspect".to_string(),
            "POST".to_string(),
            "/orders".to_string(),
            "--ir".to_string(),
            "output/ir.json".to_string(),
        ])
        .unwrap();
        assert_eq!(detail.0, Some(("POST".to_string(), "/orders".to_string())));
        assert_eq!(detail.1, PathBuf::from("output/ir.json"));
    }

    #[test]
    fn rejects_extra_operands_and_ambiguous_ir_selection() {
        assert!(parse_cli_args(&["routes".into(), "unexpected".into()]).is_err());
        assert!(parse_cli_args(&["inspect".into(), "GET".into()]).is_err());
        assert!(parse_cli_args(&["routes".into(), "--ir".into()]).is_err());
        assert!(parse_cli_args(&[
            "routes".into(),
            "--ir".into(),
            "first".into(),
            "--ir".into(),
            "second".into(),
        ])
        .is_err());
    }
}
