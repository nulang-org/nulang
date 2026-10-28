//! Compile-time signal reactivity analysis for the Nulang web framework.
//!
//! This pass scans the parsed, type-checked AST for:
//!   - `signal name: Type = init` declarations
//!   - Signal reads inside HTML expressions (children and attribute values)
//!   - `action={handler}` attributes on HTML elements
//!
//! The output is a `SignalGraph` serialized as `.nula/dist/app.signals.json`
//! by `nula build --web` and `nula dev`. It is intentionally conservative:
//! it only reports dependencies that are statically visible in the same module.

use crate::ast::{AstModule, Decl, Expr, Literal};
use crate::effect_checker::EffectChecker;
use crate::types::{Effect, EffectRow, Span};
pub use nulang_ui_protocol::{ActionCoordination, ActionPlacement};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A node in the dependency graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum GraphNode {
    /// A declared reactive signal.
    Signal { name: String },
    /// A read of a signal at a given DOM path.
    Read { signal: String, path: String },
    /// An action handler attached to an element at a DOM path.
    Action {
        handler: String,
        path: String,
        placement: ActionPlacement,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "ActionCoordination::is_replace")]
        coordination: ActionCoordination,
    },
}

/// Compile-time signal/reactivity graph for a web module.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalGraph {
    pub nodes: Vec<GraphNode>,
    /// Diagnostics stay compiler-local, never emitted into deployment artifacts.
    #[serde(skip)]
    pub errors: Vec<String>,
}

impl SignalGraph {
    fn push(&mut self, node: GraphNode) {
        self.nodes.push(node);
    }

    /// Return a compact JSON representation.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Project the compiler-owned interaction graph into the renderer-neutral
    /// nulang-ui/1 protocol. This is intentionally an interaction document,
    /// not a second HTML tree: DOM/SwiftUI/Compose hosts can consume the same
    /// signal/action identities without the web layer reparsing source.
    pub fn to_ui_document(
        &self,
        document_id: impl Into<nulang_ui_protocol::DocumentId>,
        revision: nulang_ui_protocol::Revision,
    ) -> nulang_ui_protocol::UiDocument {
        use nulang_ui_protocol::{ActionBinding, ActionId, NodeId, UiNode, WireValue};

        let mut root = UiNode::new(NodeId::from("root"), "interaction_root");
        let mut nodes = Vec::with_capacity(self.nodes.len() + 1);
        let mut signal_index = 0usize;
        let mut read_index = 0usize;
        let mut action_index = 0usize;

        for graph_node in &self.nodes {
            let node = match graph_node {
                GraphNode::Signal { name } => {
                    let id = NodeId::new(format!("signal:{signal_index}:{name}"));
                    signal_index += 1;
                    let mut node = UiNode::new(id.clone(), "signal");
                    node.properties
                        .insert("name".to_string(), WireValue::from(name.clone()));
                    node.properties.insert("value".to_string(), WireValue::Null);
                    root.children.push(id);
                    node
                }
                GraphNode::Read { signal, path } => {
                    let id = NodeId::new(format!("read:{read_index}:{signal}"));
                    read_index += 1;
                    let mut node = UiNode::new(id.clone(), "signal_read");
                    node.properties
                        .insert("signal".to_string(), WireValue::from(signal.clone()));
                    node.properties
                        .insert("path".to_string(), WireValue::from(path.clone()));
                    node.properties.insert("value".to_string(), WireValue::Null);
                    root.children.push(id);
                    node
                }
                GraphNode::Action {
                    handler,
                    path,
                    placement,
                    target,
                    coordination,
                } => {
                    let id = NodeId::new(format!("action:{action_index}:{handler}"));
                    action_index += 1;
                    let mut node = UiNode::new(id.clone(), "action");
                    node.properties
                        .insert("handler".to_string(), WireValue::from(handler.clone()));
                    node.properties
                        .insert("path".to_string(), WireValue::from(path.clone()));
                    node.actions.push(ActionBinding {
                        event: "activate".to_string(),
                        action_id: ActionId::new(handler.clone()),
                        placement: *placement,
                        target: target.clone(),
                        coordination: *coordination,
                    });
                    root.children.push(id);
                    node
                }
            };
            nodes.push(node);
        }

        nodes.push(root);
        nulang_ui_protocol::UiDocument::new(document_id, revision, NodeId::from("root"), nodes)
    }

    /// Create a protocol patch for one signal update. The patch updates the
    /// canonical signal node and every compiler-known read binding in one
    /// ordered revision transition. Unknown signals return None.
    pub fn signal_patch(
        &self,
        document_id: impl Into<nulang_ui_protocol::DocumentId>,
        base_revision: nulang_ui_protocol::Revision,
        revision: nulang_ui_protocol::Revision,
        signal: &str,
        value: nulang_ui_protocol::WireValue,
    ) -> Option<nulang_ui_protocol::UiPatch> {
        use nulang_ui_protocol::{NodeId, PatchOperation, UiPatch};

        let mut operations = Vec::new();
        let mut signal_index = 0usize;
        let mut read_index = 0usize;
        let mut found_signal = false;

        for graph_node in &self.nodes {
            match graph_node {
                GraphNode::Signal { name } => {
                    if name == signal {
                        found_signal = true;
                        operations.push(PatchOperation::SetProperty {
                            node_id: NodeId::new(format!("signal:{signal_index}:{name}")),
                            name: "value".to_string(),
                            value: value.clone(),
                        });
                    }
                    signal_index += 1;
                }
                GraphNode::Read {
                    signal: read_signal,
                    ..
                } => {
                    if read_signal == signal {
                        operations.push(PatchOperation::SetProperty {
                            node_id: NodeId::new(format!("read:{read_index}:{read_signal}")),
                            name: "value".to_string(),
                            value: value.clone(),
                        });
                    }
                    read_index += 1;
                }
                GraphNode::Action { .. } => {}
            }
        }

        found_signal.then(|| UiPatch::new(document_id, base_revision, revision, operations))
    }
}

/// Analyze a module and build its signal graph.
///
/// If an `EffectChecker` is supplied, action handlers are classified as
/// `client` or `server` based on their effect row. Without a checker or a
/// resolved effect row, placement fails closed to `server`.
pub fn analyze_module(module: &AstModule, checker: Option<&EffectChecker>) -> SignalGraph {
    let mut graph = SignalGraph::default();
    let mut signals: HashSet<String> = HashSet::new();

    // Pass 1: collect declared signal names.
    for decl in &module.decls {
        if let Decl::Signal { name, .. } = decl {
            signals.insert(name.clone());
            graph.push(GraphNode::Signal { name: name.clone() });
        }
    }

    // Pass 2: scan function bodies for HTML expressions and action attributes.
    for decl in &module.decls {
        if let Decl::Function { body, .. } = decl {
            scan_expr(body, &mut Vec::new(), false, &signals, &mut graph, checker);
        }
    }

    graph
}

/// Scan an expression for signal reads and action attributes.
fn scan_expr(
    expr: &Expr,
    path: &mut Vec<String>,
    in_html: bool,
    signals: &HashSet<String>,
    graph: &mut SignalGraph,
    checker: Option<&EffectChecker>,
) {
    match expr {
        // HTML element: el(tag, attrs, children)
        Expr::App { func, args, .. } if is_var(func, "el") && args.len() == 3 => {
            let tag = string_literal(&args[0]).unwrap_or_default();
            let attrs = array_tuples(&args[1]);
            let children = array_elements(&args[2]);

            // Policy and target are compiler-owned fields, not arbitrary
            // runtime strings. Only static values are permitted for actions.
            let action = attrs.iter().find_map(|(name, value)| {
                (name == "action" || name == "data-action")
                    .then(|| action_handler_name(value))
                    .flatten()
            });
            if let Some(handler) = action {
                let action_path = path_with_tag(path, &tag);
                let target_attrs: Vec<_> = attrs
                    .iter()
                    .filter(|(name, _)| name == "data-nulang-target")
                    .collect();
                let policy_attrs: Vec<_> = attrs
                    .iter()
                    .filter(|(name, _)| name == "data-nulang-coordination")
                    .collect();
                if target_attrs.len() > 1 || policy_attrs.len() > 1 {
                    graph.errors.push(format!(
                        "web action at {} has duplicate target or coordination attributes",
                        action_path
                    ));
                }
                let target = target_attrs.first().and_then(|(_, value)| {
                    let result = static_html_attr_value(value);
                    if result
                        .as_deref()
                        .is_none_or(|value| !valid_action_target(value))
                    {
                        graph.errors.push(format!(
                            "web action at {} requires a static #id data-nulang-target",
                            action_path
                        ));
                    }
                    result.filter(|value| valid_action_target(value))
                });
                let coordination = match policy_attrs.first() {
                    None => ActionCoordination::Replace,
                    Some((_, value)) => match static_html_attr_value(value).as_deref() {
                        Some("replace") => ActionCoordination::Replace,
                        Some("drop") => ActionCoordination::Drop,
                        Some("queue") => ActionCoordination::Queue,
                        _ => {
                            graph.errors.push(format!(
                                "web action at {} requires a static data-nulang-coordination: replace, drop, or queue",
                                action_path
                            ));
                            ActionCoordination::Replace
                        }
                    },
                };
                if coordination != ActionCoordination::Replace && target.is_none() {
                    graph.errors.push(format!(
                        "web action at {} needs data-nulang-target for {} coordination",
                        action_path,
                        coordination.as_str()
                    ));
                }
                graph.push(GraphNode::Action {
                    handler: handler.clone(),
                    path: action_path,
                    placement: classify_action(&handler, checker),
                    target,
                    coordination,
                });
            }
            for (_, value) in &attrs {
                scan_expr(
                    value,
                    &mut path_with(path, &tag),
                    true,
                    signals,
                    graph,
                    checker,
                );
            }

            path.push(tag);
            for child in &children {
                scan_expr(child, path, true, signals, graph, checker);
            }
            path.pop();
        }

        // Component call: <Tag ...>{slot}</Tag>
        Expr::App { func, args, .. } if is_component_var(func) => {
            let tag = component_name(func).unwrap_or_default();
            if let Some((slot, props)) = args.split_last() {
                for value in props {
                    scan_expr(
                        value,
                        &mut path_with(path, &tag),
                        true,
                        signals,
                        graph,
                        checker,
                    );
                }
                path.push(tag.clone());
                scan_expr(slot, path, true, signals, graph, checker);
                path.pop();
            }
        }

        // Signal read inside HTML context.
        Expr::Var(name, ..) if in_html && signals.contains(name) => {
            graph.push(GraphNode::Read {
                signal: name.clone(),
                path: path_string(path),
            });
        }

        Expr::Lambda { body, .. } => scan_expr(body, path, in_html, signals, graph, checker),
        Expr::App { func, args, .. } => {
            scan_expr(func, path, in_html, signals, graph, checker);
            for arg in args {
                scan_expr(arg, path, in_html, signals, graph, checker);
            }
        }
        Expr::Let { value, body, .. } | Expr::LetRec { value, body, .. } => {
            scan_expr(value, path, in_html, signals, graph, checker);
            scan_expr(body, path, in_html, signals, graph, checker);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            scan_expr(cond, path, in_html, signals, graph, checker);
            scan_expr(then_branch, path, in_html, signals, graph, checker);
            if let Some(else_branch) = else_branch {
                scan_expr(else_branch, path, in_html, signals, graph, checker);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            scan_expr(scrutinee, path, in_html, signals, graph, checker);
            for (_, guard, body) in arms {
                if let Some(guard) = guard {
                    scan_expr(guard, path, in_html, signals, graph, checker);
                }
                scan_expr(body, path, in_html, signals, graph, checker);
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for e in exprs {
                scan_expr(e, path, in_html, signals, graph, checker);
            }
        }
        Expr::Tuple(exprs, ..) | Expr::Array(exprs, ..) => {
            for e in exprs {
                scan_expr(e, path, in_html, signals, graph, checker);
            }
        }
        Expr::Record(fields, ..) | Expr::RecordUpdate { fields, .. } => {
            for (_, e) in fields {
                scan_expr(e, path, in_html, signals, graph, checker);
            }
        }
        Expr::FieldAccess { expr, .. } => scan_expr(expr, path, in_html, signals, graph, checker),
        Expr::Index { arr, idx, .. } => {
            scan_expr(arr, path, in_html, signals, graph, checker);
            scan_expr(idx, path, in_html, signals, graph, checker);
        }
        Expr::Binary { left, right, .. } => {
            scan_expr(left, path, in_html, signals, graph, checker);
            scan_expr(right, path, in_html, signals, graph, checker);
        }
        Expr::Unary { expr, .. } => scan_expr(expr, path, in_html, signals, graph, checker),
        Expr::Assign { target, value, .. } => {
            scan_expr(target, path, in_html, signals, graph, checker);
            scan_expr(value, path, in_html, signals, graph, checker);
        }
        Expr::Pipe { left, right, .. } => {
            scan_expr(left, path, in_html, signals, graph, checker);
            scan_expr(right, path, in_html, signals, graph, checker);
        }
        Expr::For { iterable, body, .. } => {
            scan_expr(iterable, path, in_html, signals, graph, checker);
            scan_expr(body, path, in_html, signals, graph, checker);
        }
        Expr::While { cond, body, .. } => {
            scan_expr(cond, path, in_html, signals, graph, checker);
            scan_expr(body, path, in_html, signals, graph, checker);
        }
        Expr::Return(e, ..) => {
            if let Some(e) = e {
                scan_expr(e, path, in_html, signals, graph, checker);
            }
        }
        Expr::Break(e, ..) => {
            if let Some(e) = e {
                scan_expr(e, path, in_html, signals, graph, checker);
            }
        }
        Expr::FString(parts, ..) => {
            for part in parts {
                scan_expr(part, path, in_html, signals, graph, checker);
            }
        }
        Expr::Consume { expr, .. } => scan_expr(expr, path, in_html, signals, graph, checker),
        Expr::Recover { body, .. } => scan_expr(body, path, in_html, signals, graph, checker),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => {
            scan_expr(expr, path, in_html, signals, graph, checker);
        }
        Expr::Handle { body, handlers, .. } => {
            scan_expr(body, path, in_html, signals, graph, checker);
            for h in handlers {
                scan_expr(&h.body, path, in_html, signals, graph, checker);
            }
        }
        Expr::Perform { args, .. } => {
            for arg in args {
                scan_expr(arg, path, in_html, signals, graph, checker);
            }
        }
        Expr::Emit { args, .. } => {
            for arg in args {
                scan_expr(arg, path, in_html, signals, graph, checker);
            }
        }
        Expr::Spawn {
            actor_type, init, ..
        } => {
            scan_expr(actor_type, path, in_html, signals, graph, checker);
            for (_, e) in init {
                scan_expr(e, path, in_html, signals, graph, checker);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            scan_expr(actor, path, in_html, signals, graph, checker);
            for arg in args {
                scan_expr(arg, path, in_html, signals, graph, checker);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, _, guard, body) in arms {
                if let Some(guard) = guard {
                    scan_expr(guard, path, in_html, signals, graph, checker);
                }
                scan_expr(body, path, in_html, signals, graph, checker);
            }
            if let Some((t, b)) = after {
                scan_expr(t, path, in_html, signals, graph, checker);
                scan_expr(b, path, in_html, signals, graph, checker);
            }
        }
        Expr::GrainRef { key, .. } => scan_expr(key, path, in_html, signals, graph, checker),
        Expr::Resume { value, .. } => scan_expr(value, path, in_html, signals, graph, checker),
        Expr::Migrate { actor, node, .. } => {
            scan_expr(actor, path, in_html, signals, graph, checker);
            scan_expr(node, path, in_html, signals, graph, checker);
        }

        _ => {}
    }
}

fn classify_action(handler: &str, checker: Option<&EffectChecker>) -> ActionPlacement {
    let Some(checker) = checker else {
        return ActionPlacement::Server;
    };
    let Some(row) = checker.function_row(handler) else {
        return ActionPlacement::Server;
    };

    // Fail closed: only a fully-known, browser-local effect row may execute
    // client-side. Open rows and every other effect stay server-side so adding
    // a new privileged effect cannot silently expand browser authority.
    match row {
        EffectRow::Closed(effects)
            if effects
                .iter()
                .all(|effect| matches!(effect, Effect::Client)) =>
        {
            ActionPlacement::Client
        }
        EffectRow::Closed(_) | EffectRow::Open(_, _) => ActionPlacement::Server,
    }
}

fn is_var(expr: &Expr, name: &str) -> bool {
    matches!(expr, Expr::Var(n, ..) if n == name)
}

fn is_component_var(expr: &Expr) -> bool {
    matches!(expr, Expr::Var(n, ..) if n.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
}

fn component_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Var(n, ..) if n.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) => {
            Some(n.clone())
        }
        _ => None,
    }
}

fn string_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Literal(Literal::String(s), ..) => Some(s.clone()),
        _ => None,
    }
}

fn array_elements(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Array(exprs, ..) => exprs.iter().collect(),
        _ => Vec::new(),
    }
}

fn array_tuples(expr: &Expr) -> Vec<(String, &Expr)> {
    let mut out = Vec::new();
    for e in array_elements(expr) {
        match e {
            Expr::Tuple(parts, ..) if parts.len() == 2 => {
                if let Some(name) = string_literal(&parts[0]) {
                    out.push((name, &parts[1]));
                }
            }
            _ => {}
        }
    }
    out
}

fn static_html_attr_value(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Literal(Literal::String(value), ..) => Some(value.clone()),
        Expr::App { func, args, .. } if is_var(func, "text") && args.len() == 1 => {
            string_literal(&args[0])
        }
        _ => None,
    }
}

fn valid_action_target(target: &str) -> bool {
    let Some(id) = target.strip_prefix('#') else {
        return false;
    };
    let mut chars = id.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn action_handler_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Var(name, ..) => Some(name.clone()),
        _ => static_html_attr_value(expr),
    }
}

fn path_string(path: &[String]) -> String {
    if path.is_empty() {
        "root".to_string()
    } else {
        path.join(" > ")
    }
}

fn path_with(path: &[String], tag: &str) -> Vec<String> {
    let mut p = path.to_vec();
    p.push(tag.to_string());
    p
}

fn path_with_tag(path: &[String], tag: &str) -> String {
    path_string(&path_with(path, tag))
}

/// Inject a `<script src="/app.client.js">` tag into the generated HTML so the
/// client-side signal micro-runtime can hydrate the page. The tag is placed
/// just before `</body>` when present, otherwise appended at the end.
pub fn inject_client_runtime_script(html: &str) -> String {
    if !html.contains("data-signal=") && !html.contains("data-action=") {
        return html.to_string();
    }
    let script = r#"<script src="/app.client.js"></script>"#;
    if let Some(pos) = html.rfind("</body>") {
        let mut out = String::with_capacity(html.len() + script.len() + 1);
        out.push_str(&html[..pos]);
        out.push_str(script);
        out.push_str(&html[pos..]);
        out
    } else {
        format!("{}{}", html, script)
    }
}

/// Rewrite a module's HTML expressions so the client micro-runtime can hydrate
/// signal reads and action handlers.
///
/// This pass runs after type and effect checking. It is intentionally
/// conservative: it only rewrites `html.el` calls it can recognize statically.
pub fn rewrite_module(module: &mut AstModule, checker: Option<&EffectChecker>) {
    let mut signals: HashSet<String> = HashSet::new();
    for decl in &module.decls {
        if let Decl::Signal { name, .. } = decl {
            signals.insert(name.clone());
        }
    }
    for decl in &mut module.decls {
        if let Decl::Function { body, .. } = decl {
            rewrite_expr(body, false, &signals, checker);
        }
    }
}

fn rewrite_expr(
    expr: &mut Expr,
    in_html: bool,
    signals: &HashSet<String>,
    checker: Option<&EffectChecker>,
) {
    match expr {
        // HTML element: el(tag, attrs, children)
        Expr::App { func, args, span } if is_var(func, "el") && args.len() == 3 => {
            // Rewrite attrs and children first.
            rewrite_expr(&mut args[1], true, signals, checker);
            rewrite_expr(&mut args[2], true, signals, checker);

            let mut data_attrs: Vec<Expr> = Vec::new();

            // Inspect attrs array for action={fn} attributes.
            if let Expr::Array(attrs, _) = &mut args[1] {
                for attr in attrs.iter_mut() {
                    if let Expr::Tuple(parts, _) = attr {
                        if parts.len() == 2 {
                            if let Some(name) = string_literal(&parts[0]) {
                                if name == "action" {
                                    if let Some(handler) = action_handler_name(&parts[1]) {
                                        let placement = classify_action(&handler, checker);
                                        data_attrs.push(data_attr("data-action", &handler, *span));
                                        data_attrs.push(data_attr(
                                            "data-action-placement",
                                            action_placement_str(placement),
                                            *span,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Wrap direct signal reads in children with <span data-signal="...">.
            if let Expr::Array(children, _) = &mut args[2] {
                for child in children.iter_mut() {
                    if let Expr::Var(name, child_span) = child {
                        if signals.contains(name) {
                            let name = name.clone();
                            let child_span = *child_span;
                            *child = wrap_signal_read(&name, child_span);
                        }
                    }
                }
            }

            // Add data attributes to the element's attrs.
            if let Expr::Array(attrs, _) = &mut args[1] {
                attrs.extend(data_attrs);
            }
        }

        // Component call: <Tag ...>{slot}</Tag>
        Expr::App { func, args, span } if is_component_var(func) => {
            for arg in args.iter_mut() {
                rewrite_expr(arg, true, signals, checker);
            }
        }

        // Recurse through other expression forms.
        Expr::Lambda { body, .. } => rewrite_expr(body, in_html, signals, checker),
        Expr::App { func, args, .. } => {
            rewrite_expr(func, in_html, signals, checker);
            for arg in args.iter_mut() {
                rewrite_expr(arg, in_html, signals, checker);
            }
        }
        Expr::Let { value, body, .. } | Expr::LetRec { value, body, .. } => {
            rewrite_expr(value, in_html, signals, checker);
            rewrite_expr(body, in_html, signals, checker);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            rewrite_expr(cond, in_html, signals, checker);
            rewrite_expr(then_branch, in_html, signals, checker);
            if let Some(else_branch) = else_branch {
                rewrite_expr(else_branch, in_html, signals, checker);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            rewrite_expr(scrutinee, in_html, signals, checker);
            for (_, guard, body) in arms {
                if let Some(guard) = guard {
                    rewrite_expr(guard, in_html, signals, checker);
                }
                rewrite_expr(body, in_html, signals, checker);
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for e in exprs.iter_mut() {
                rewrite_expr(e, in_html, signals, checker);
            }
        }
        Expr::Tuple(exprs, ..) | Expr::Array(exprs, ..) => {
            for e in exprs.iter_mut() {
                rewrite_expr(e, in_html, signals, checker);
            }
        }
        Expr::Record(fields, ..) | Expr::RecordUpdate { fields, .. } => {
            for (_, e) in fields.iter_mut() {
                rewrite_expr(e, in_html, signals, checker);
            }
        }
        Expr::FieldAccess { expr, .. } => rewrite_expr(expr, in_html, signals, checker),
        Expr::Index { arr, idx, .. } => {
            rewrite_expr(arr, in_html, signals, checker);
            rewrite_expr(idx, in_html, signals, checker);
        }
        Expr::Binary { left, right, .. } => {
            rewrite_expr(left, in_html, signals, checker);
            rewrite_expr(right, in_html, signals, checker);
        }
        Expr::Unary { expr, .. } => rewrite_expr(expr, in_html, signals, checker),
        Expr::Assign { target, value, .. } => {
            rewrite_expr(target, in_html, signals, checker);
            rewrite_expr(value, in_html, signals, checker);
        }
        Expr::Pipe { left, right, .. } => {
            rewrite_expr(left, in_html, signals, checker);
            rewrite_expr(right, in_html, signals, checker);
        }
        Expr::For { iterable, body, .. } => {
            rewrite_expr(iterable, in_html, signals, checker);
            rewrite_expr(body, in_html, signals, checker);
        }
        Expr::While { cond, body, .. } => {
            rewrite_expr(cond, in_html, signals, checker);
            rewrite_expr(body, in_html, signals, checker);
        }
        Expr::Return(e, ..) => {
            if let Some(e) = e {
                rewrite_expr(e, in_html, signals, checker);
            }
        }
        Expr::Break(e, ..) => {
            if let Some(e) = e {
                rewrite_expr(e, in_html, signals, checker);
            }
        }
        Expr::FString(parts, ..) => {
            for part in parts.iter_mut() {
                rewrite_expr(part, in_html, signals, checker);
            }
        }
        Expr::Consume { expr, .. } => rewrite_expr(expr, in_html, signals, checker),
        Expr::Recover { body, .. } => rewrite_expr(body, in_html, signals, checker),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => {
            rewrite_expr(expr, in_html, signals, checker);
        }
        Expr::Handle { body, handlers, .. } => {
            rewrite_expr(body, in_html, signals, checker);
            for h in handlers.iter_mut() {
                rewrite_expr(&mut h.body, in_html, signals, checker);
            }
        }
        Expr::Perform { args, .. } => {
            for arg in args.iter_mut() {
                rewrite_expr(arg, in_html, signals, checker);
            }
        }
        Expr::Emit { args, .. } => {
            for arg in args.iter_mut() {
                rewrite_expr(arg, in_html, signals, checker);
            }
        }
        Expr::Spawn {
            actor_type, init, ..
        } => {
            rewrite_expr(actor_type, in_html, signals, checker);
            for (_, e) in init.iter_mut() {
                rewrite_expr(e, in_html, signals, checker);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            rewrite_expr(actor, in_html, signals, checker);
            for arg in args.iter_mut() {
                rewrite_expr(arg, in_html, signals, checker);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, _, guard, body) in arms {
                if let Some(guard) = guard {
                    rewrite_expr(guard, in_html, signals, checker);
                }
                rewrite_expr(body, in_html, signals, checker);
            }
            if let Some((t, b)) = after {
                rewrite_expr(t, in_html, signals, checker);
                rewrite_expr(b, in_html, signals, checker);
            }
        }
        Expr::GrainRef { key, .. } => rewrite_expr(key, in_html, signals, checker),
        Expr::Resume { value, .. } => rewrite_expr(value, in_html, signals, checker),
        Expr::Migrate { actor, node, .. } => {
            rewrite_expr(actor, in_html, signals, checker);
            rewrite_expr(node, in_html, signals, checker);
        }

        _ => {}
    }
}

fn action_placement_str(placement: ActionPlacement) -> &'static str {
    match placement {
        ActionPlacement::Client => "client",
        ActionPlacement::Server => "server",
    }
}

fn data_attr(name: &str, value: &str, span: Span) -> Expr {
    Expr::Tuple(
        vec![
            Expr::Literal(Literal::String(name.to_string()), span),
            Expr::Literal(Literal::String(value.to_string()), span),
        ],
        span,
    )
}

/// Generate the generic client-side micro-runtime that hydrates signals and
/// actions from server-rendered HTML. Bindings are discovered once from
/// `data-signal` and `data-action` attributes emitted by the compiler.
pub fn generate_client_runtime() -> String {
    r#"(function () {
  // Signal names come from compiled bindings; they must never resolve through
  // Object.prototype (e.g. constructor, toString, __proto__).
  const signals = Object.create(null);
  const signalBindings = Object.create(null);
  const hydratedActions = new WeakSet();
  // Per-target action coordination. In replace mode, a newer submission
  // supersedes the older UI update, but cannot undo a server mutation.
  const pendingPatches = new Map();
  const MAX_QUEUED_ACTIONS_PER_TARGET = 32;

  function refreshSignal(name) {
    const bindings = signalBindings[name];
    if (!bindings) return;
    bindings.forEach(function (el) {
      el.textContent = signals[name];
    });
  }

  function runClientAction(handler) {
    if (window.nulangActions && typeof window.nulangActions[handler] === 'function') {
      window.nulangActions[handler]();
    } else {
      console.warn('nulang: missing client action handler', handler);
    }
  }

  function actionMessageId(prefix) {
    if (globalThis.crypto && typeof globalThis.crypto.randomUUID === 'function') {
      return prefix + ':' + globalThis.crypto.randomUUID();
    }
    return prefix + ':' + Date.now() + ':' + Math.random().toString(16).slice(2);
  }

  function wireFormPayload(body) {
    const fields = {};
    body.forEach(function (value, key) {
      if (typeof value === 'string') {
        fields[key] = { type: 'string', value: value };
      }
    });
    return { type: 'object', value: fields };
  }

  function createActionMessage(handler, placement, body) {
    const correlationId = actionMessageId('corr');
    return {
      type: 'invoke_action',
      protocol: 'nulang-ui-msg/1',
      request: {
        document_id: 'app',
        revision: '0',
        action_id: handler,
        placement: placement,
        correlation_id: correlationId,
        idempotency_key: actionMessageId('idem'),
        payload: wireFormPayload(body)
      }
    };
  }

  function formBody(form) {
    const body = new URLSearchParams();
    if (!form) return body;
    new FormData(form).forEach(function (value, key) {
      if (typeof value === 'string') {
        body.append(key, value);
      }
    });
    return body;
  }

  // Reuse matching DOM nodes rather than replacing focused controls, losing
  // event listeners, or discarding client-owned input state. Keyed siblings
  // have a stable identity via id or data-nulang-key.
  function nodeKey(node) {
    if (node.nodeType !== 1) return null;
    const key = node.getAttribute('data-nulang-key');
    if (key !== null) return 'key:' + key;
    return node.id ? 'id:' + node.id : null;
  }

  function sameKind(current, incoming) {
    return current.nodeType === incoming.nodeType &&
      (current.nodeType !== 1 ||
       (current.tagName === incoming.tagName &&
        current.namespaceURI === incoming.namespaceURI));
  }

  function morphNode(current, incoming) {
    if (!sameKind(current, incoming) ||
        (nodeKey(current) !== null && nodeKey(current) !== nodeKey(incoming))) {
      const replacement = document.importNode(incoming, true);
      current.replaceWith(replacement);
      return replacement;
    }
    if (current.nodeType !== 1) {
      if (current.nodeValue !== incoming.nodeValue) {
        current.nodeValue = incoming.nodeValue;
      }
      return current;
    }

    const focused = document.activeElement === current;
    const tag = current.tagName;
    // A user edit is owned by the browser until the application explicitly
    // resets it. Do not overwrite it with a stale server-rendered default.
    const dirtyValue = (tag === 'TEXTAREA' ||
       (tag === 'INPUT' && current.type !== 'file')) &&
      (focused || current.value !== current.defaultValue);
    const dirtyChecked = tag === 'INPUT' &&
      (focused || current.checked !== current.defaultChecked);
    const dirtySelect = tag === 'SELECT' &&
      (focused || Array.from(current.options).some(function (option) {
        return option.selected !== option.defaultSelected;
      }));
    const value = dirtyValue ? current.value : null;
    const checked = dirtyChecked ? current.checked : null;
    const selected = dirtySelect
      ? Array.from(current.options).filter(function (option) { return option.selected; })
          .map(function (option) { return option.value; })
      : null;
    const selection = focused && typeof current.selectionStart === 'number'
      ? [current.selectionStart, current.selectionEnd, current.selectionDirection]
      : null;

    // Synchronize attributes, including removals, for SSR parity.
    Array.from(current.attributes).forEach(function (attr) {
      if (!incoming.hasAttribute(attr.name)) current.removeAttribute(attr.name);
    });
    Array.from(incoming.attributes).forEach(function (attr) {
      if (current.getAttribute(attr.name) !== attr.value) {
        current.setAttribute(attr.name, attr.value);
      }
    });

    const oldChildren = Array.from(current.childNodes);
    const keyed = new Map();
    const unkeyed = [];
    oldChildren.forEach(function (child) {
      const key = nodeKey(child);
      if (key === null) {
        unkeyed.push(child);
      } else if (!keyed.has(key)) {
        keyed.set(key, child);
      }
    });
    const reused = new Set();
    let unkeyedIndex = 0;
    let position = 0;
    Array.from(incoming.childNodes).forEach(function (child) {
      const key = nodeKey(child);
      let match = null;
      if (key !== null) {
        const candidate = keyed.get(key);
        if (candidate && !reused.has(candidate) && sameKind(candidate, child)) {
          match = candidate;
        }
      } else {
        while (unkeyedIndex < unkeyed.length) {
          const candidate = unkeyed[unkeyedIndex++];
          if (sameKind(candidate, child)) {
            match = candidate;
            break;
          }
        }
      }
      let node;
      if (match) {
        reused.add(match);
        node = morphNode(match, child);
      } else {
        node = document.importNode(child, true);
      }
      const before = current.childNodes[position] || null;
      if (node !== before) current.insertBefore(node, before);
      position++;
    });
    oldChildren.forEach(function (child) {
      if (!reused.has(child) && child.parentNode === current) child.remove();
    });

    if (dirtyValue) current.value = value;
    if (dirtyChecked) current.checked = checked;
    if (dirtySelect) {
      Array.from(current.options).forEach(function (option) {
        option.selected = selected.includes(option.value);
      });
    }
    if (selection) {
      // Selection APIs can throw on unsupported input types.
      try { current.setSelectionRange(selection[0], selection[1], selection[2]); }
      catch (_) {}
    }
    return current;
  }

  // Progressive HTML updates are opt-in. The response may contain an entire
  // server-rendered page or only the element identified by the target ID.
  // Legacy actions still reload when no target is declared.
  async function patchActionResponse(response, selector, isCurrent) {
    if (!/^#[A-Za-z][A-Za-z0-9_-]*$/.test(selector)) {
      throw new Error('invalid Nulang patch target (expected an element ID)');
    }
    const contentType = response.headers.get('content-type') || '';
    if (!/^text\/html(?:\s*;|\s*$)/i.test(contentType.trim())) {
      throw new Error('Nulang patch response must be text/html');
    }
    // Check again after reading the response stream: a newer mutation may
    // have been submitted while this body was still being downloaded.
    const html = await response.text();
    if (isCurrent && !isCurrent()) return;
    const page = new DOMParser().parseFromString(html, 'text/html');
    const incoming = page.querySelector(selector);
    const current = document.querySelector(selector);
    if (!incoming || !current) {
      throw new Error('Nulang patch target not found: ' + selector);
    }
    // Tiny DOM stubs used by the existing no-dependency regression test do
    // not implement the Node interface; real browser nodes take the morph path.
    if (typeof current.nodeType !== 'number' ||
        typeof incoming.nodeType !== 'number') {
      current.replaceWith(document.importNode(incoming, true));
    } else {
      // Moving a retained focused node with insertBefore can temporarily
      // blur it. Restore focus only if that same node survives the morph.
      const active = document.activeElement;
      const restoreFocus = active && (active === current || current.contains(active));
      const selection = restoreFocus && typeof active.selectionStart === 'number'
        ? [active.selectionStart, active.selectionEnd, active.selectionDirection]
        : null;
      morphNode(current, incoming);
      if (restoreFocus && active.isConnected) {
        active.focus({ preventScroll: true });
        if (selection) {
          try { active.setSelectionRange(selection[0], selection[1], selection[2]); }
          catch (_) {}
        }
      }
    }
    hydrate();
  }

  // A target is a coordination lane. The revision (generation) identifies the
  // one request permitted to patch that lane. Aborting a fetch only cancels
  // client-side work; the server must still enforce mutation idempotency.
  async function dispatchServerAction(handler, target, body, lane, generation) {
    const controller = lane && typeof AbortController === 'function'
      ? new AbortController() : null;
    if (lane) lane.controller = controller;
    const isCurrent = () => !lane ||
      (pendingPatches.get(target) === lane && lane.generation === generation);
    try {
      const options = { method: 'POST', body: body };
      if (controller) options.signal = controller.signal;
      const response = await fetch(window.location.href, options);
      if (!isCurrent()) return;
      if (!response.ok) {
        throw new Error('server action failed: ' + response.status);
      }
      if (target) {
        await patchActionResponse(response, target, isCurrent);
      } else {
        window.location.reload();
      }
    } catch (error) {
      // An aborted/superseded request must not update DOM or log an error.
      if (isCurrent()) throw error;
    } finally {
      if (lane && isCurrent()) {
        if (lane.policy === 'queue' && lane.queue.length) {
          const next = lane.queue.shift();
          const nextGeneration = ++lane.generation;
          dispatchServerAction(next.handler, target, next.body, lane, nextGeneration)
            .then(next.resolve, next.reject);
        } else {
          pendingPatches.delete(target);
        }
      }
    }
  }

  function runServerAction(handler, el) {
    const form = el.closest('form');
    const target = el.dataset.nulangTarget || (form && form.dataset.nulangTarget);
    const policy = el.dataset.nulangCoordination ||
      (form && form.dataset.nulangCoordination) || 'replace';
    // Invalid policies are programmer errors. Never silently choose a more
    // aggressive mutation scheduling policy than the one requested.
    if (!['replace', 'drop', 'queue'].includes(policy)) {
      return Promise.reject(new Error('invalid Nulang action coordination policy: ' + policy));
    }
    if (!target && policy !== 'replace') {
      return Promise.reject(new Error('action coordination requires a patch target'));
    }
    const previous = target && pendingPatches.get(target);
    if (previous && previous.policy !== policy) {
      return Promise.reject(new Error('cannot change coordination policy while target is busy'));
    }
    // Drop redundant actions while pending; do not cancel the first request.
    if (previous && policy === 'drop') return Promise.resolve();
    // High-frequency inputs must not enqueue unbounded mutation backlogs.
    if (previous && policy === 'queue' &&
        previous.queue.length >= MAX_QUEUED_ACTIONS_PER_TARGET) {
      return Promise.reject(new Error('Nulang action queue is full for target ' + target));
    }

    // Snapshot the body and idempotency key at the time of submission, not
    // when an action eventually leaves the queue.
    const body = formBody(form);
    const message = createActionMessage(handler, 'server', body);
    body.append('__nulang_action', handler);
    body.append('__nulang_ui_message', JSON.stringify(message));

    if (previous && policy === 'queue') {
      return new Promise(function (resolve, reject) {
        previous.queue.push({ handler, body, resolve, reject });
      });
    }

    const lane = target
      ? (previous || { policy, controller: null, generation: 0, queue: [] })
      : null;
    if (lane) {
      const oldController = lane.controller;
      ++lane.generation;
      pendingPatches.set(target, lane);
      if (oldController) oldController.abort();
    }
    return dispatchServerAction(handler, target, body, lane, lane ? lane.generation : 0);
  }

  function hydrate() {
    // Rebuild the signal index after patches; detached elements must not stay
    // subscribed. Preserve signal values instead of reinitializing them.
    Object.keys(signalBindings).forEach(function (name) {
      signalBindings[name] = [];
    });
    document.querySelectorAll('[data-signal]').forEach(function (el) {
      const name = el.dataset.signal;
      if (!(name in signals)) {
        signals[name] = el.textContent;
      }
      if (!signalBindings[name]) {
        signalBindings[name] = [];
      }
      signalBindings[name].push(el);
    });

    document.querySelectorAll('[data-action]').forEach(function (el) {
      if (hydratedActions.has(el)) return;
      hydratedActions.add(el);
      const listener = function (e) {
        // Morphing may change the compiled action on an existing DOM node.
        // Read the latest binding rather than retaining an obsolete handler.
        const handler = el.dataset.action;
        if (!handler) return;
        const placement = el.dataset.actionPlacement;
        e.preventDefault();
        if (placement === 'server') {
          runServerAction(handler, el).catch(function (err) {
            console.error('nulang: server action failed', err);
          });
        } else {
          runClientAction(handler);
        }
      };
      // Clicks bubble from every control in a form. Only an actual submit
      // should invoke its action; non-form actions remain click-driven.
      el.addEventListener(el.tagName === 'FORM' ? 'submit' : 'click', listener);
    });
  }

  window.nulang = {
    signal: function (name) {
      return {
        get: function () { return signals[name]; },
        set: function (value) {
          signals[name] = value;
          refreshSignal(name);
        }
      };
    }
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', hydrate);
  } else {
    hydrate();
  }
})();"#
        .to_string()
}

fn wrap_signal_read(name: &str, span: Span) -> Expr {
    Expr::App {
        func: Box::new(Expr::Var("el".to_string(), span)),
        args: vec![
            Expr::Literal(Literal::String("span".to_string()), span),
            Expr::Array(vec![data_attr("data-signal", name, span)], span),
            Expr::Array(vec![Expr::Var(name.to_string(), span)], span),
        ],
        span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> AstModule {
        let tokens = Lexer::new(source).lex().unwrap();
        Parser::new(tokens).parse_module().unwrap()
    }

    #[test]
    fn test_signal_decl_collected() {
        let module = parse(
            r#"
signal count: Int = 0

fn main() {
    perform IO.print(count)
}
"#,
        );
        let graph = analyze_module(&module, None);
        assert!(graph.nodes.contains(&GraphNode::Signal {
            name: "count".to_string()
        }));
    }

    #[test]
    fn test_signal_read_in_html() {
        let module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

signal count: Int = 0

fn badge() -> Html {
    <span>{count}</span>
}
"#,
        );
        let graph = analyze_module(&module, None);
        assert!(graph.nodes.contains(&GraphNode::Read {
            signal: "count".to_string(),
            path: "span".to_string(),
        }));
    }

    #[test]
    fn test_action_coordination_projects_to_typed_protocol() {
        let module = parse(
            r##"
fn save() {}
fn view() {
    el("form", [
        ("action", save),
        ("data-nulang-target", text("#status")),
        ("data-nulang-coordination", text("queue"))
    ], [])
}
"##,
        );
        let graph = analyze_module(&module, None);
        assert!(graph.errors.is_empty(), "{:?}", graph.errors);
        let action = graph
            .nodes
            .iter()
            .find_map(|node| match node {
                GraphNode::Action {
                    target,
                    coordination,
                    ..
                } => Some((target, coordination)),
                _ => None,
            })
            .expect("form action");
        assert_eq!(action.0.as_deref(), Some("#status"));
        assert_eq!(*action.1, nulang_ui_protocol::ActionCoordination::Queue);
        let document = graph.to_ui_document("app", nulang_ui_protocol::Revision(1));
        assert_eq!(
            document
                .nodes
                .iter()
                .find(|n| n.kind == "action")
                .unwrap()
                .actions[0]
                .coordination,
            nulang_ui_protocol::ActionCoordination::Queue
        );
        assert_eq!(
            document
                .nodes
                .iter()
                .find(|n| n.kind == "action")
                .unwrap()
                .actions[0]
                .target
                .as_deref(),
            Some("#status")
        );
    }

    #[test]
    fn test_action_coordination_fails_closed_on_invalid_or_dynamic_policy() {
        for policy in ["text(\"unknown\")", "text(input)"] {
            let module = parse(&format!(
                "fn save() {{}}\nfn view() {{ el(\"form\", [(\"action\", save), (\"data-nulang-target\", text(\"#status\")), (\"data-nulang-coordination\", {})], []) }}",
                policy
            ));
            let graph = analyze_module(&module, None);
            assert!(
                !graph.errors.is_empty(),
                "invalid or dynamic policy accepted: {}",
                policy
            );
        }
        let module = parse(
            r#"
fn save() {}
fn view() {
    el("form", [("action", save), ("data-nulang-coordination", text("drop"))], [])
}
"#,
        );
        let graph = analyze_module(&module, None);
        assert!(graph.errors.iter().any(|e| e.contains("target")));
    }

    #[test]
    fn test_action_attribute() {
        let module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

fn add() {}

fn button() -> Html {
    <button action={add}>Click</button>
}
"#,
        );
        let graph = analyze_module(&module, None);
        assert!(graph.nodes.contains(&GraphNode::Action {
            handler: "add".to_string(),
            path: "button".to_string(),
            placement: ActionPlacement::Server,
            target: None,
            coordination: ActionCoordination::Replace,
        }));
    }

    #[test]
    fn test_action_server_placement() {
        let module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

fn add() {}

fn submit(id: RouteParam) -> Html ! {Request, Web, Render} {
    let _ = perform Web.param("id")
    text("ok")
}

fn view() -> Html {
    <div>
        <button action={add}>Add</button>
        <form action={submit}>Submit</form>
    </div>
}
"#,
        );
        let mut checker = crate::effect_checker::EffectChecker::new();
        let _ = checker.check_module(&module.decls);
        let graph = analyze_module(&module, Some(&checker));
        assert!(graph.nodes.contains(&GraphNode::Action {
            handler: "add".to_string(),
            path: "div > button".to_string(),
            placement: ActionPlacement::Client,
            target: None,
            coordination: ActionCoordination::Replace,
        }));
        assert!(graph.nodes.contains(&GraphNode::Action {
            handler: "submit".to_string(),
            path: "div > form".to_string(),
            placement: ActionPlacement::Server,
            target: None,
            coordination: ActionCoordination::Replace,
        }));
    }

    fn find_function_body<'a>(module: &'a AstModule, name: &str) -> &'a Expr {
        module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name: n, body, .. } if n == name => Some(body),
                _ => None,
            })
            .unwrap_or_else(|| panic!("function {} not found", name))
    }

    #[test]
    fn test_rewrite_wraps_signal_reads() {
        let mut module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

signal count: Html = text("0")

fn view() -> Html {
    <div><span>{count}</span></div>
}
"#,
        );
        let mut checker = crate::effect_checker::EffectChecker::new();
        let _ = checker.check_module(&module.decls);
        rewrite_module(&mut module, Some(&checker));
        let body = find_function_body(&module, "view");
        let html = format!("{:?}", body);
        assert!(
            html.contains("data-signal"),
            "data-signal not found in {}",
            html
        );
    }

    #[test]
    fn test_rewrite_action_data_attrs() {
        let mut module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

fn add() {}

fn view() -> Html {
    <button action={add}>Add</button>
}
"#,
        );
        let mut checker = crate::effect_checker::EffectChecker::new();
        let _ = checker.check_module(&module.decls);
        rewrite_module(&mut module, Some(&checker));
        let body = find_function_body(&module, "view");
        let html = format!("{:?}", body);
        assert!(html.contains("data-action"), "data-action not found");
        assert!(
            html.contains("data-action-placement"),
            "data-action-placement not found"
        );
    }

    #[test]
    fn test_client_runtime_contains_hydrate_and_signal_api() {
        let js = generate_client_runtime();
        assert!(js.contains("hydrate"), "hydrate not found");
        assert!(js.contains("window.nulang"), "nulang global not found");
        assert!(js.contains("data-signal"), "data-signal selector not found");
        assert!(js.contains("data-action"), "data-action selector not found");
        assert!(
            js.contains("signalBindings"),
            "signal binding index not found"
        );
        assert!(
            js.contains("window.location.reload"),
            "server actions should reload the page after POST"
        );
    }

    #[test]
    fn test_static_html_does_not_inject_client_script() {
        let html = "<html><body><h1>Hi</h1></body></html>";
        assert_eq!(inject_client_runtime_script(html), html);
    }

    #[test]
    fn test_interactive_html_injects_client_script_before_body() {
        let html = r#"<html><body><span data-signal="count">0</span></body></html>"#;
        let out = inject_client_runtime_script(html);
        assert!(out.contains(r#"<script src="/app.client.js"></script>"#));
        assert!(out.find("<script").unwrap() < out.find("</body>").unwrap());
    }

    #[test]
    fn test_interactive_fragment_injects_client_script() {
        let html = r#"<button data-action="save">Save</button>"#;
        let out = inject_client_runtime_script(html);
        assert!(out.ends_with(r#"<script src="/app.client.js"></script>"#));
    }

    #[test]
    fn test_nested_path() {
        let module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

signal count: Int = 0

fn card() -> Html {
    <div class="card"><span>{count}</span></div>
}
"#,
        );
        let graph = analyze_module(&module, None);
        assert!(graph.nodes.contains(&GraphNode::Read {
            signal: "count".to_string(),
            path: "div > span".to_string(),
        }));
    }

    #[test]
    fn test_signal_graph_projects_to_valid_ui_protocol_document() {
        let module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

signal count: Html = text("0")

fn add() {}

fn view() -> Html {
    <div>
        <span>{count}</span>
        <button action={add}>Add</button>
    </div>
}
"#,
        );
        let mut checker = crate::effect_checker::EffectChecker::new();
        let _ = checker.check_module(&module.decls);
        let graph = analyze_module(&module, Some(&checker));

        let document = graph.to_ui_document("app", nulang_ui_protocol::Revision(1));
        document
            .validate()
            .expect("interaction document must be valid");

        assert_eq!(document.document_id.as_str(), "app");
        assert_eq!(document.revision, nulang_ui_protocol::Revision(1));

        let signal = document
            .nodes
            .iter()
            .find(|node| node.kind == "signal")
            .expect("signal node");
        assert_eq!(
            signal.properties.get("name"),
            Some(&nulang_ui_protocol::WireValue::from("count"))
        );

        let action = document
            .nodes
            .iter()
            .find(|node| node.kind == "action")
            .expect("action node");
        assert_eq!(action.actions.len(), 1);
        assert_eq!(action.actions[0].action_id.as_str(), "add");
        assert_eq!(action.actions[0].event, "activate");
        assert_eq!(action.actions[0].placement, ActionPlacement::Client);
    }

    #[test]
    fn test_signal_patch_updates_state_and_all_read_bindings() {
        let module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

signal count: Html = text("0")

fn view() -> Html {
    <div><span>{count}</span><strong>{count}</strong></div>
}
"#,
        );
        let graph = analyze_module(&module, None);
        let mut document = graph.to_ui_document("app", nulang_ui_protocol::Revision(4));

        let patch = graph
            .signal_patch(
                "app",
                nulang_ui_protocol::Revision(4),
                nulang_ui_protocol::Revision(5),
                "count",
                nulang_ui_protocol::WireValue::from("1"),
            )
            .expect("known signal should produce a patch");

        assert_eq!(patch.operations.len(), 3);
        document.apply_patch(&patch).expect("patch should apply");

        let updated: Vec<_> = document
            .nodes
            .iter()
            .filter(|node| {
                node.properties.get("signal") == Some(&nulang_ui_protocol::WireValue::from("count"))
                    || node.properties.get("name")
                        == Some(&nulang_ui_protocol::WireValue::from("count"))
            })
            .collect();
        assert_eq!(updated.len(), 3);
        assert!(updated.iter().all(|node| {
            node.properties.get("value") == Some(&nulang_ui_protocol::WireValue::from("1"))
        }));

        assert!(graph
            .signal_patch(
                "app",
                nulang_ui_protocol::Revision(5),
                nulang_ui_protocol::Revision(6),
                "missing",
                nulang_ui_protocol::WireValue::from("x"),
            )
            .is_none());
    }
    #[test]
    fn test_client_runtime_emits_canonical_ui_action_message() {
        let js = generate_client_runtime();
        assert!(js.contains("__nulang_ui_message"));
        assert!(js.contains("nulang-ui-msg/1"));
        assert!(js.contains("invoke_action"));
        assert!(js.contains("document_id"));
        assert!(js.contains("idempotency_key"));
        assert!(js.contains("__nulang_action"));
        assert!(
            js.contains("new URLSearchParams()"),
            "server actions must use the URL-encoded transport parsed by the runtime"
        );
        assert!(
            js.contains("typeof value === 'string'"),
            "only string form fields are currently part of the action wire payload"
        );
    }
}
