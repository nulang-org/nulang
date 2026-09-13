//! Deployment intermediate representation (IR) for Nulang Web apps.
//!
//! `nula build --web` emits `dist/nulang-app.ir.json`, a JSON document that
//! describes routes, static artifacts, required capabilities, signal graph,
//! budgets, and middleware. Adapters consume this IR to deploy to Nulang Cloud,
//! static hosts, or Docker.

use crate::ast::{AstModule, Decl, Expr, WorkflowItem};
use crate::lexer::Lexer;
use crate::package::manifest::BudgetsSection;
use crate::parser::Parser;
use crate::runtime::WebRoute;
use crate::web::modules::ModuleRegistry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

const HOST_CAPABILITIES: &[&str] = &[
    "DB", "Net", "Realtime", "Http", "Web", "Actor", "Timer", "Job", "IO",
];
/// Reserved capability emitted when source metadata could not be derived
/// completely. Deployment policy must never silently grant this marker.
pub const INCOMPLETE_METADATA_CAPABILITY: &str = "__nulang_metadata_incomplete__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrRoute {
    pub method: String,
    pub path: String,
    pub placement: String,
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetsIr {
    pub initial_js_max_bytes: Option<usize>,
    pub lcp_seconds: Option<f64>,
}

/// A cloud environment variable or secret required by an imported module.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloudConfigEntry {
    pub key: String,
    pub required_by: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeploymentIr {
    pub version: u32,
    pub routes: Vec<IrRoute>,
    pub signals: serde_json::Value,
    pub capabilities: Vec<String>,
    pub budgets: BudgetsIr,
    pub middleware: Vec<String>,
    pub cloud_config: Vec<CloudConfigEntry>,
}

impl DeploymentIr {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

#[derive(Debug, Default)]
struct SemanticMetadata {
    capabilities: BTreeSet<String>,
    imports: BTreeSet<String>,
    complete: bool,
}

impl SemanticMetadata {
    fn new() -> Self {
        Self {
            capabilities: BTreeSet::new(),
            imports: BTreeSet::new(),
            complete: true,
        }
    }
}

/// Generate the deployment IR for a web package.
///
/// `routes` are the routes collected by running the compiled entry point.
/// `signal_graph_path` is the optional path to `app.signals.json` emitted by
/// the reactivity pass. `src_root` is the package `src/` directory. Deployment
/// capabilities and first-party module imports are derived from parsed syntax,
/// never substring matching. `budgets` are parsed from `Nulang.toml`.
pub fn generate_deployment_ir(
    routes: &[WebRoute],
    signal_graph_path: Option<&Path>,
    src_root: &Path,
    budgets: &BudgetsSection,
) -> DeploymentIr {
    let mut ir_routes = Vec::new();
    for route in routes {
        let placement =
            if route.path.contains(':') || !matches!(route.method.as_str(), "GET" | "HEAD") {
                "server".to_string()
            } else {
                "static".to_string()
            };
        let artifact = if placement == "static" {
            Some(route_path_to_artifact(&route.path))
        } else {
            None
        };
        ir_routes.push(IrRoute {
            method: route.method.as_str().to_string(),
            path: route.path.clone(),
            placement,
            artifact,
        });
    }

    let signals = signal_graph_path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));

    let metadata = collect_semantic_metadata(src_root);
    let imports: Vec<String> = metadata.imports.iter().cloned().collect();
    let registry = ModuleRegistry::builtin();

    let mut capabilities = metadata.capabilities;
    capabilities.extend(registry.collect_capabilities(&imports));
    if !metadata.complete {
        // Fail closed without changing the public DeploymentIr struct shape:
        // never under-report authority when semantic metadata cannot be fully
        // derived. The reserved marker lets an admission layer reject the
        // deployment outright, while the full host-capability set prevents a
        // legacy consumer from treating the incomplete manifest as least-
        // privilege metadata.
        capabilities.extend(HOST_CAPABILITIES.iter().map(|cap| (*cap).to_string()));
        capabilities.insert(INCOMPLETE_METADATA_CAPABILITY.to_string());
    }
    let capabilities: Vec<String> = capabilities.into_iter().collect();

    let budgets_ir = BudgetsIr {
        initial_js_max_bytes: budgets.initial_js_max_bytes(),
        lcp_seconds: budgets.lcp_seconds(),
    };

    let cloud_config = infer_module_cloud_config(&registry, &imports);
    let middleware = infer_middleware(&imports);

    DeploymentIr {
        version: 1,
        routes: ir_routes,
        signals,
        capabilities,
        budgets: budgets_ir,
        middleware,
        cloud_config,
    }
}

fn route_path_to_artifact(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        "index.html".to_string()
    } else {
        format!("{}/index.html", trimmed)
    }
}

/// Parse every `.nula` source file below `src_root` and derive deployment
/// metadata from the AST. Text in comments and string literals must never
/// grant a Cloud capability.
fn collect_semantic_metadata(src_root: &Path) -> SemanticMetadata {
    let mut metadata = SemanticMetadata::new();
    collect_semantic_metadata_recursive(src_root, &mut metadata);
    metadata
}

fn collect_semantic_metadata_recursive(path: &Path, metadata: &mut SemanticMetadata) {
    if !path.is_dir() {
        metadata.complete = false;
        return;
    }

    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => {
            metadata.complete = false;
            return;
        }
    };
    let mut entries: Vec<_> = entries.filter_map(|entry| entry.ok()).collect();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_semantic_metadata_recursive(&path, metadata);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("nula") {
            continue;
        }

        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(_) => {
                metadata.complete = false;
                continue;
            }
        };
        if !collect_source_metadata(&source, metadata) {
            metadata.complete = false;
        }
    }
}

fn collect_source_metadata(source: &str, metadata: &mut SemanticMetadata) -> bool {
    let mut lexer = Lexer::new(source);
    let tokens = match lexer.lex() {
        Ok(tokens) => tokens,
        Err(_) => return false,
    };
    let mut parser = Parser::new(tokens);
    let module = match parser.parse_module() {
        Ok(module) => module,
        Err(_) => return false,
    };
    collect_module_metadata(&module, metadata);
    true
}

fn collect_module_metadata(module: &AstModule, metadata: &mut SemanticMetadata) {
    for decl in &module.decls {
        collect_decl_metadata(decl, metadata);
    }
}

fn collect_decl_metadata(decl: &Decl, metadata: &mut SemanticMetadata) {
    match decl {
        Decl::Function {
            default_values,
            requires,
            ensures,
            body,
            ..
        } => {
            for expr in default_values.iter().flatten() {
                collect_expr_metadata(expr, metadata);
            }
            for expr in requires.iter().chain(ensures.iter()) {
                collect_expr_metadata(expr, metadata);
            }
            collect_expr_metadata(body, metadata);
        }
        Decl::Actor {
            state_fields,
            behaviors,
            init,
            initializer,
            apply_handlers,
            migrations,
            ..
        } => {
            for (_, _, _, expr) in state_fields {
                collect_expr_metadata(expr, metadata);
            }
            for behavior in behaviors {
                collect_expr_metadata(&behavior.body, metadata);
            }
            for (_, expr) in init {
                collect_expr_metadata(expr, metadata);
            }
            if let Some((_, _, body)) = initializer {
                collect_expr_metadata(body, metadata);
            }
            for handler in apply_handlers {
                collect_expr_metadata(&handler.body, metadata);
            }
            for migration in migrations {
                if let Some(body) = &migration.state_body {
                    collect_expr_metadata(body, metadata);
                }
                for (_, _, body) in &migration.event_migrations {
                    collect_expr_metadata(body, metadata);
                }
            }
        }
        Decl::StateMachine {
            entry_hooks,
            exit_hooks,
            ..
        } => {
            for (_, expr) in entry_hooks.iter().chain(exit_hooks.iter()) {
                collect_expr_metadata(expr, metadata);
            }
        }
        Decl::Module { decls, .. } => {
            for decl in decls {
                collect_decl_metadata(decl, metadata);
            }
        }
        Decl::Import { path, .. } => {
            if path.starts_with("@nulang/") {
                metadata.imports.insert(path.clone());
            }
        }
        Decl::Workflow {
            items, compensate, ..
        } => {
            for item in items {
                match item {
                    WorkflowItem::Step(step) => collect_workflow_step_metadata(step, metadata),
                    WorkflowItem::Parallel(steps) => {
                        for step in steps {
                            collect_workflow_step_metadata(step, metadata);
                        }
                    }
                }
            }
            if let Some(expr) = compensate {
                collect_expr_metadata(expr, metadata);
            }
        }
        Decl::CrdtDecl { fields, .. } => {
            for (_, _, _, expr) in fields {
                collect_expr_metadata(expr, metadata);
            }
        }
        Decl::NamedHandler { handlers, .. } => {
            for handler in handlers {
                collect_expr_metadata(&handler.body, metadata);
            }
        }
        Decl::Class { methods, .. } => {
            for method in methods {
                if let Some(body) = &method.default_body {
                    collect_expr_metadata(body, metadata);
                }
            }
        }
        Decl::Impl { methods, .. } => {
            for method in methods {
                collect_expr_metadata(&method.body, metadata);
            }
        }
        Decl::LetBinding { value, .. }
        | Decl::Signal { init: value, .. }
        | Decl::Given { value, .. } => collect_expr_metadata(value, metadata),
        Decl::TypeAlias { .. }
        | Decl::RecordType { .. }
        | Decl::VariantType { .. }
        | Decl::EffectDecl { .. }
        | Decl::Extern { .. }
        | Decl::Agent { .. }
        | Decl::Database { .. } => {}
    }
}

fn collect_workflow_step_metadata(
    step: &crate::ast::WorkflowStep,
    metadata: &mut SemanticMetadata,
) {
    collect_expr_metadata(&step.body, metadata);
    if let Some(expr) = &step.compensate {
        collect_expr_metadata(expr, metadata);
    }
}

fn collect_expr_metadata(expr: &Expr, metadata: &mut SemanticMetadata) {
    match expr {
        Expr::Literal(_, _) | Expr::Var(_, _) | Expr::SelfRef(_) | Expr::Panic(_, _) => {}
        Expr::FString(exprs, _) | Expr::Tuple(exprs, _) | Expr::Array(exprs, _) => {
            for expr in exprs {
                collect_expr_metadata(expr, metadata);
            }
        }
        Expr::Lambda { body, .. }
        | Expr::Unary { expr: body, .. }
        | Expr::FieldAccess { expr: body, .. }
        | Expr::Resume { value: body, .. }
        | Expr::GrainRef { key: body, .. }
        | Expr::CapAnnotate { expr: body, .. }
        | Expr::TypeAnnotate { expr: body, .. }
        | Expr::Consume { expr: body, .. }
        | Expr::Recover { body, .. }
        | Expr::Defer { expr: body, .. }
        | Expr::Hide { body, .. }
        | Expr::Seal { body, .. } => collect_expr_metadata(body, metadata),
        Expr::App { func, args, .. } => {
            collect_expr_metadata(func, metadata);
            for arg in args {
                collect_expr_metadata(arg, metadata);
            }
        }
        Expr::Let { value, body, .. } | Expr::LetRec { value, body, .. } => {
            collect_expr_metadata(value, metadata);
            collect_expr_metadata(body, metadata);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            collect_expr_metadata(cond, metadata);
            collect_expr_metadata(then_branch, metadata);
            if let Some(expr) = else_branch {
                collect_expr_metadata(expr, metadata);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_expr_metadata(scrutinee, metadata);
            for (_, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_expr_metadata(guard, metadata);
                }
                collect_expr_metadata(body, metadata);
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for expr in exprs {
                collect_expr_metadata(expr, metadata);
            }
        }
        Expr::Record(fields, _) => {
            for (_, expr) in fields {
                collect_expr_metadata(expr, metadata);
            }
        }
        Expr::RecordUpdate { base, fields, .. } => {
            collect_expr_metadata(base, metadata);
            for (_, expr) in fields {
                collect_expr_metadata(expr, metadata);
            }
        }
        Expr::Index { arr, idx, .. }
        | Expr::Binary {
            left: arr,
            right: idx,
            ..
        }
        | Expr::Pipe {
            left: arr,
            right: idx,
            ..
        }
        | Expr::Migrate {
            actor: arr,
            node: idx,
            ..
        } => {
            collect_expr_metadata(arr, metadata);
            collect_expr_metadata(idx, metadata);
        }
        Expr::Assign { target, value, .. } => {
            collect_expr_metadata(target, metadata);
            collect_expr_metadata(value, metadata);
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            target_node,
            ..
        } => {
            collect_expr_metadata(actor_type, metadata);
            for (_, expr) in init {
                collect_expr_metadata(expr, metadata);
            }
            if let Some(args) = positional_args {
                for arg in args {
                    collect_expr_metadata(arg, metadata);
                }
            }
            if let Some(node) = target_node {
                collect_expr_metadata(node, metadata);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            collect_expr_metadata(actor, metadata);
            for arg in args {
                collect_expr_metadata(arg, metadata);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, _, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_expr_metadata(guard, metadata);
                }
                collect_expr_metadata(body, metadata);
            }
            if let Some((timeout, body)) = after {
                collect_expr_metadata(timeout, metadata);
                collect_expr_metadata(body, metadata);
            }
        }
        Expr::Emit { args, .. } => {
            for arg in args {
                collect_expr_metadata(arg, metadata);
            }
        }
        Expr::Perform { effect, args, .. } => {
            if is_host_capability(effect) {
                metadata.capabilities.insert(effect.clone());
            }
            for arg in args {
                collect_expr_metadata(arg, metadata);
            }
        }
        Expr::Handle { body, handlers, .. } => {
            collect_expr_metadata(body, metadata);
            for handler in handlers {
                collect_expr_metadata(&handler.body, metadata);
            }
        }
        Expr::For { iterable, body, .. } => {
            collect_expr_metadata(iterable, metadata);
            collect_expr_metadata(body, metadata);
        }
        Expr::While { cond, body, .. } => {
            collect_expr_metadata(cond, metadata);
            collect_expr_metadata(body, metadata);
        }
        Expr::Return(value, _) | Expr::Break(value, _) => {
            if let Some(value) = value {
                collect_expr_metadata(value, metadata);
            }
        }
    }
}

fn is_host_capability(effect: &str) -> bool {
    HOST_CAPABILITIES.contains(&effect)
}

/// Infer cloud config keys required by structurally imported `@nulang/*` modules.
fn infer_module_cloud_config(
    registry: &ModuleRegistry,
    imports: &[String],
) -> Vec<CloudConfigEntry> {
    let mut entries = Vec::new();
    for name in imports {
        if let Some(spec) = registry.get(name) {
            for key in &spec.cloud_config_keys {
                entries.push(CloudConfigEntry {
                    key: key.clone(),
                    required_by: name.clone(),
                });
            }
        }
    }
    entries
}

/// Infer default middleware stack, extending it when imported modules
/// contribute security concerns (e.g., auth sessions).
fn infer_middleware(imports: &[String]) -> Vec<String> {
    let mut stack = vec![
        "security_headers".to_string(),
        "request_log".to_string(),
        "csrf".to_string(),
    ];
    if imports.iter().any(|name| name == "@nulang/auth") {
        stack.push("auth".to_string());
    }
    stack
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata_from_source(source: &str) -> SemanticMetadata {
        let mut metadata = SemanticMetadata::new();
        assert!(collect_source_metadata(source, &mut metadata));
        metadata
    }

    #[test]
    fn test_route_path_to_artifact() {
        assert_eq!(route_path_to_artifact("/"), "index.html");
        assert_eq!(route_path_to_artifact("/about"), "about/index.html");
        assert_eq!(
            route_path_to_artifact("/blog/:slug"),
            "blog/:slug/index.html"
        );
    }

    #[test]
    fn test_semantic_capability_collection() {
        let metadata = metadata_from_source(
            r#"
            fn foo() {
                perform DB.query("...")
                perform Realtime.broadcast("room", "hi")
            }
            "#,
        );
        assert!(metadata.capabilities.contains("DB"));
        assert!(metadata.capabilities.contains("Realtime"));
        assert!(!metadata.capabilities.contains("Net"));
    }

    #[test]
    fn test_comments_and_strings_do_not_grant_capabilities() {
        let metadata = metadata_from_source(
            r#"
            fn main() {
                // perform DB.query("must not count")
                let example = "perform Net.connect must not count"
                perform IO.print(example)
            }
            "#,
        );
        assert!(metadata.capabilities.contains("IO"));
        assert!(!metadata.capabilities.contains("DB"));
        assert!(!metadata.capabilities.contains("Net"));
    }

    #[test]
    fn test_module_metadata_from_structured_imports() {
        let metadata = metadata_from_source(
            r#"
            import @nulang/auth
            import @nulang/postgres
            fn main() {}
            "#,
        );
        let imports: Vec<String> = metadata.imports.iter().cloned().collect();
        let registry = ModuleRegistry::builtin();
        let caps = registry.collect_capabilities(&imports);
        assert!(caps.contains(&"auth".to_string()));
        assert!(caps.contains(&"DB".to_string()));
        assert!(!caps.contains(&"payments".to_string()));

        let entries = infer_module_cloud_config(&registry, &imports);
        assert!(entries.iter().any(|entry| {
            entry.key == "AUTH_COOKIE_SECRET" && entry.required_by == "@nulang/auth"
        }));
        assert!(entries.iter().any(|entry| {
            entry.key == "DATABASE_URL" && entry.required_by == "@nulang/postgres"
        }));

        let middleware = infer_middleware(&imports);
        assert!(middleware.contains(&"auth".to_string()));
        assert!(middleware.contains(&"csrf".to_string()));
    }

    #[test]
    fn test_invalid_source_is_rejected() {
        let mut metadata = SemanticMetadata::new();
        assert!(!collect_source_metadata("fn broken( {", &mut metadata));
    }

    #[test]
    fn incomplete_metadata_marker_is_reserved_and_not_a_host_capability() {
        assert!(!is_host_capability(INCOMPLETE_METADATA_CAPABILITY));
        assert!(HOST_CAPABILITIES.contains(&"DB"));
        assert!(HOST_CAPABILITIES.contains(&"Net"));
    }
}
