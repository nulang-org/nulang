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

use crate::ast::{AstModule, Decl, Expr, FunctionAnnotation, Literal};
use crate::effect_checker::EffectChecker;
use crate::types::{infer_web_placement, Placement, Span};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Compile-time placement decision for an action handler.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionPlacement {
    /// The handler only touches browser-local signals/DOM; run it in the client.
    Client,
    /// The handler needs the server (DB, request, actor, network, etc.).
    Server,
}

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
}

/// One statically discovered web component and its intrinsic execution
/// environment. Child components stay separate so they can become islands
/// instead of forcing the parent into the child's placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentNode {
    pub name: String,
    pub placement: Option<Placement>,
    pub children: Vec<String>,
}

/// Compile-time component call graph.
///
/// This is deliberately separate from `SignalGraph`: signal/action metadata is
/// already a runtime-facing artifact, while component placement is compiler
/// planning metadata that will feed island partitioning in a later pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComponentGraph {
    pub components: Vec<ComponentNode>,
}

/// Build a deterministic component call graph from a parsed module.
///
/// Components are functions whose names are capitalized or functions referenced
/// by a capitalized component call. Placement uses explicit `@placement`
/// annotations first, then the effect checker's row, then a declared row as a
/// fallback. In component context an otherwise-pure component is static by
/// default; child placement is represented by graph edges rather than promoted
/// into the parent.
pub fn analyze_component_graph(
    module: &AstModule,
    checker: Option<&EffectChecker>,
) -> ComponentGraph {
    let mut component_names = BTreeSet::new();
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    collect_component_edges(&module.decls, &mut component_names, &mut edges);

    let mut components = Vec::new();
    collect_component_nodes(
        &module.decls,
        checker,
        &component_names,
        &edges,
        &mut components,
    );
    components.sort_by(|a, b| a.name.cmp(&b.name));

    ComponentGraph { components }
}

fn collect_component_edges(
    decls: &[Decl],
    component_names: &mut BTreeSet<String>,
    edges: &mut BTreeMap<String, BTreeSet<String>>,
) {
    for decl in decls {
        match decl {
            Decl::Function { name, body, .. } => {
                if is_component_identifier(name) {
                    component_names.insert(name.clone());
                }

                let mut calls = BTreeSet::new();
                collect_component_calls(body, &mut calls);
                for child in &calls {
                    component_names.insert(child.clone());
                }
                if !calls.is_empty() {
                    edges.entry(name.clone()).or_default().extend(calls);
                }
            }
            Decl::Module { decls, .. } => {
                collect_component_edges(decls, component_names, edges);
            }
            _ => {}
        }
    }
}

fn collect_component_nodes(
    decls: &[Decl],
    checker: Option<&EffectChecker>,
    component_names: &BTreeSet<String>,
    edges: &BTreeMap<String, BTreeSet<String>>,
    out: &mut Vec<ComponentNode>,
) {
    for decl in decls {
        match decl {
            Decl::Function {
                name,
                effect,
                annotations,
                ..
            } if component_names.contains(name) => {
                let explicit = annotations.iter().find_map(|annotation| match annotation {
                    FunctionAnnotation::Placement(placement) => Some(*placement),
                    _ => None,
                });
                let inferred = checker
                    .and_then(|checker| checker.function_row(name))
                    .or(effect.as_ref())
                    .and_then(infer_web_placement);
                let children = edges
                    .get(name)
                    .map(|children| children.iter().cloned().collect())
                    .unwrap_or_default();

                out.push(ComponentNode {
                    name: name.clone(),
                    placement: Some(explicit.or(inferred).unwrap_or(Placement::Static)),
                    children,
                });
            }
            Decl::Module { decls, .. } => {
                collect_component_nodes(decls, checker, component_names, edges, out);
            }
            _ => {}
        }
    }
}

fn collect_component_calls(expr: &Expr, out: &mut BTreeSet<String>) {
    if let Some(name) = component_call_name(expr) {
        out.insert(name);
    }

    match expr {
        Expr::Lambda { body, .. } => collect_component_calls(body, out),
        Expr::App { func, args, .. } => {
            collect_component_calls(func, out);
            for arg in args {
                collect_component_calls(arg, out);
            }
        }
        Expr::Let { value, body, .. } | Expr::LetRec { value, body, .. } => {
            collect_component_calls(value, out);
            collect_component_calls(body, out);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            collect_component_calls(cond, out);
            collect_component_calls(then_branch, out);
            if let Some(else_branch) = else_branch {
                collect_component_calls(else_branch, out);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_component_calls(scrutinee, out);
            for (_, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_component_calls(guard, out);
                }
                collect_component_calls(body, out);
            }
        }
        Expr::Block { exprs, .. }
        | Expr::Par { exprs, .. }
        | Expr::Tuple(exprs, ..)
        | Expr::Array(exprs, ..)
        | Expr::FString(exprs, ..) => {
            for expr in exprs {
                collect_component_calls(expr, out);
            }
        }
        Expr::Record(fields, ..) | Expr::RecordUpdate { fields, .. } => {
            for (_, expr) in fields {
                collect_component_calls(expr, out);
            }
        }
        Expr::FieldAccess { expr, .. }
        | Expr::Unary { expr, .. }
        | Expr::Consume { expr, .. }
        | Expr::CapAnnotate { expr, .. }
        | Expr::TypeAnnotate { expr, .. } => collect_component_calls(expr, out),
        Expr::Index { arr, idx, .. } => {
            collect_component_calls(arr, out);
            collect_component_calls(idx, out);
        }
        Expr::Binary { left, right, .. } | Expr::Pipe { left, right, .. } => {
            collect_component_calls(left, out);
            collect_component_calls(right, out);
        }
        Expr::Assign { target, value, .. } => {
            collect_component_calls(target, out);
            collect_component_calls(value, out);
        }
        Expr::For { iterable, body, .. } => {
            collect_component_calls(iterable, out);
            collect_component_calls(body, out);
        }
        Expr::While { cond, body, .. } => {
            collect_component_calls(cond, out);
            collect_component_calls(body, out);
        }
        Expr::Return(value, ..) | Expr::Break(value, ..) => {
            if let Some(value) = value {
                collect_component_calls(value, out);
            }
        }
        Expr::Recover { body, .. } | Expr::Hide { body, .. } | Expr::Seal { body, .. } => {
            collect_component_calls(body, out);
        }
        Expr::Defer { expr, .. } => collect_component_calls(expr, out),
        Expr::Handle { body, handlers, .. } => {
            collect_component_calls(body, out);
            for handler in handlers {
                collect_component_calls(&handler.body, out);
            }
        }
        Expr::Perform { args, .. } | Expr::Emit { args, .. } => {
            for arg in args {
                collect_component_calls(arg, out);
            }
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            target_node,
            ..
        } => {
            collect_component_calls(actor_type, out);
            for (_, expr) in init {
                collect_component_calls(expr, out);
            }
            if let Some(args) = positional_args {
                for arg in args {
                    collect_component_calls(arg, out);
                }
            }
            if let Some(target) = target_node {
                collect_component_calls(target, out);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            collect_component_calls(actor, out);
            for arg in args {
                collect_component_calls(arg, out);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, _, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_component_calls(guard, out);
                }
                collect_component_calls(body, out);
            }
            if let Some((timeout, body)) = after {
                collect_component_calls(timeout, out);
                collect_component_calls(body, out);
            }
        }
        Expr::GrainRef { key, .. } => collect_component_calls(key, out),
        Expr::Resume { value, .. } => collect_component_calls(value, out),
        Expr::Migrate { actor, node, .. } => {
            collect_component_calls(actor, out);
            collect_component_calls(node, out);
        }
        _ => {}
    }
}

fn component_call_name(expr: &Expr) -> Option<String> {
    match expr {
        // JSX currently lowers every tag to el("tag", attrs, children).
        // Preserve capitalized-tag identity here so compiler planning can see
        // component boundaries even before the parser grows a dedicated
        // component-call AST node.
        Expr::App { func, args, .. } if is_var(func, "el") && args.len() == 3 => {
            string_literal(&args[0]).filter(|name| is_component_identifier(name))
        }
        Expr::App { func, .. } => component_name(func),
        _ => None,
    }
}

fn is_component_identifier(name: &str) -> bool {
    name.chars()
        .next()
        .map(|ch| ch.is_uppercase())
        .unwrap_or(false)
}

/// Analyze a module and build its signal graph.
///
/// If an `EffectChecker` is supplied, action handlers are classified as
/// `client` or `server` based on their effect row. Without it, every action
/// defaults to `client`.
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

            for (name, value) in &attrs {
                if name == "action" {
                    if let Some(handler) = action_handler_name(value) {
                        graph.push(GraphNode::Action {
                            handler: handler.clone(),
                            path: path_with_tag(path, &tag),
                            placement: classify_action(&handler, checker),
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
        return ActionPlacement::Client;
    };
    let Some(row) = checker.function_row(handler) else {
        return ActionPlacement::Client;
    };

    match infer_web_placement(row) {
        Some(Placement::Server | Placement::Edge | Placement::Actor | Placement::Workflow) => {
            ActionPlacement::Server
        }
        Some(Placement::Static | Placement::Client) | None => ActionPlacement::Client,
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
/// actions from server-rendered HTML. The runtime reads `app.signals.json`
/// to learn about declared signals and actions, then uses `data-signal` and
/// `data-action` attributes on the DOM to bind live updates.
pub fn generate_client_runtime() -> String {
    r#"(function () {
  const signals = {};

  function refreshSignal(name) {
    document.querySelectorAll('[data-signal="' + name + '"]').forEach(function (el) {
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

  async function runServerAction(handler, el) {
    const form = el.closest('form');
    const body = form ? new FormData(form) : new FormData();
    body.append('__nulang_action', handler);
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
            placement: ActionPlacement::Client,
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
        }));
        assert!(graph.nodes.contains(&GraphNode::Action {
            handler: "submit".to_string(),
            path: "div > form".to_string(),
            placement: ActionPlacement::Server,
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
            js.contains("window.location.reload"),
            "server actions should reload the page after POST"
        );
    }

    #[test]
    fn test_inject_client_script_before_body() {
        let html = "<html><body><h1>Hi</h1></body></html>";
        let out = inject_client_runtime_script(html);
        assert!(out.contains(r#"<script src="/app.client.js"></script>"#));
        assert!(out.contains("</body>"));
        assert!(out.find("<script").unwrap() < out.find("</body>").unwrap());
    }

    #[test]
    fn test_inject_client_script_appends_when_no_body() {
        let html = "<h1>Hi</h1>";
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
    fn test_component_graph_tracks_calls_and_inferred_placements() {
        let module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types

fn StaticCard() -> Html ! {Render, Web} {
    <span>Static</span>
}

fn ServerPanel() -> Html ! {DB, Render, Web} {
    <section>Server</section>
}

fn Dashboard() -> Html {
    <div>
        <StaticCard />
        <ServerPanel />
    </div>
}
"#,
        );
        let mut checker = crate::effect_checker::EffectChecker::new();
        checker.check_module(&module.decls).unwrap();

        let graph = analyze_component_graph(&module, Some(&checker));
        let dashboard = graph
            .components
            .iter()
            .find(|component| component.name == "Dashboard")
            .expect("Dashboard component");
        let static_card = graph
            .components
            .iter()
            .find(|component| component.name == "StaticCard")
            .expect("StaticCard component");
        let server_panel = graph
            .components
            .iter()
            .find(|component| component.name == "ServerPanel")
            .expect("ServerPanel component");

        assert_eq!(
            dashboard.children,
            vec!["ServerPanel".to_string(), "StaticCard".to_string()]
        );
        // A server child is an island boundary; it must not force the static
        // shell itself onto the server.
        assert_eq!(dashboard.placement, Some(Placement::Static));
        assert_eq!(static_card.placement, Some(Placement::Static));
        assert_eq!(server_panel.placement, Some(Placement::Server));
    }
}
