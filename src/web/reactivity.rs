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
use crate::web::contracts::HandlerParamContract;
pub use nulang_ui_protocol::ActionPlacement;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        params: Vec<HandlerParamContract>,
    },
}

/// Compile-time signal/reactivity graph for a web module.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalGraph {
    pub nodes: Vec<GraphNode>,
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
                    ..
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
    let mut handler_params: HashMap<String, Vec<HandlerParamContract>> = HashMap::new();

    // Pass 1: collect declared signals and compiler-owned function parameter
    // metadata. Action discovery below only records references to these
    // declarations; it does not reconstruct types from rendered HTML.
    for decl in &module.decls {
        match decl {
            Decl::Signal { name, .. } => {
                signals.insert(name.clone());
                graph.push(GraphNode::Signal { name: name.clone() });
            }
            Decl::Function { name, params, .. } => {
                handler_params.insert(
                    name.clone(),
                    params
                        .iter()
                        .map(|param| HandlerParamContract {
                            name: param.name.clone(),
                            ty: param.ty.as_ref().map(ToString::to_string),
                            capability: param.cap.map(|capability| capability.to_string()),
                            request: None,
                        })
                        .collect(),
                );
            }
            _ => {}
        }
    }

    // Pass 2: scan function bodies for HTML expressions and action attributes.
    for decl in &module.decls {
        if let Decl::Function { body, .. } = decl {
            scan_expr(body, &mut Vec::new(), false, &signals, &mut graph, checker);
        }
    }

    for node in &mut graph.nodes {
        if let GraphNode::Action {
            handler, params, ..
        } = node
        {
            *params = handler_params.get(handler).cloned().unwrap_or_default();
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

            for (name, value) in &attrs {
                if name == "action" {
                    if let Some(handler) = action_handler_name(value) {
                        graph.push(GraphNode::Action {
                            handler: handler.clone(),
                            path: path_with_tag(path, &tag),
                            placement: classify_action(&handler, checker),
                            params: Vec::new(),
                        });
                    }
                }
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

fn action_handler_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Var(name, ..) => Some(name.clone()),
        Expr::Literal(Literal::String(s), ..) => Some(s.clone()),
        _ => None,
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
  const signals = {};
  const signalBindings = Object.create(null);

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

  async function runServerAction(handler, el) {
    const form = el.closest('form');
    const body = formBody(form);
    const message = createActionMessage(handler, 'server', body);
    body.append('__nulang_action', handler);
    body.append('__nulang_ui_message', JSON.stringify(message));
    const response = await fetch(window.location.href, { method: 'POST', body: body });
    if (!response.ok) {
      throw new Error('server action failed: ' + response.status);
    }
    window.location.reload();
  }

  function hydrate() {
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
      const handler = el.dataset.action;
      const placement = el.dataset.actionPlacement;
      const listener = function (e) {
        e.preventDefault();
        if (placement === 'server') {
          runServerAction(handler, el).catch(function (err) {
            console.error('nulang: server action failed', err);
          });
        } else {
          runClientAction(handler);
        }
      };
      el.addEventListener('click', listener);
      if (el.tagName === 'FORM') {
        el.addEventListener('submit', listener);
      }
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
            params: Vec::new(),
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
            params: Vec::new(),
        }));
        assert!(graph.nodes.contains(&GraphNode::Action {
            handler: "submit".to_string(),
            path: "div > form".to_string(),
            placement: ActionPlacement::Server,
            params: vec![crate::web::contracts::HandlerParamContract {
                name: "id".to_string(),
                ty: Some("RouteParam".to_string()),
                capability: None,
                request: None,
            }],
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
    #[test]
    fn test_action_graph_preserves_compiler_handler_params() {
        let module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

fn save(title: String, count: Int, active: Bool) {}

fn view() -> Html {
    <button action={save}>Save</button>
}
"#,
        );
        let graph = analyze_module(&module, None);
        let action = graph
            .nodes
            .iter()
            .find_map(|node| match node {
                GraphNode::Action {
                    handler, params, ..
                } if handler == "save" => Some(params),
                _ => None,
            })
            .expect("save action");

        assert_eq!(
            action,
            &vec![
                crate::web::contracts::HandlerParamContract {
                    name: "title".to_string(),
                    ty: Some("String".to_string()),
                    capability: None,
                    request: None,
                },
                crate::web::contracts::HandlerParamContract {
                    name: "count".to_string(),
                    ty: Some("Int".to_string()),
                    capability: None,
                    request: None,
                },
                crate::web::contracts::HandlerParamContract {
                    name: "active".to_string(),
                    ty: Some("Bool".to_string()),
                    capability: None,
                    request: None,
                },
            ]
        );
    }
}
