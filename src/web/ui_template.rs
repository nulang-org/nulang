//! Conservative compiler-owned JSX -> semantic UI plan.
//! Only literal values, typed paths and keyed array comprehensions are accepted;
//! arbitrary Nulang expressions are never evaluated by this renderer.
use crate::ast::{AstModule, Decl, Expr, Literal};
use nulang_ui_protocol::{document_from_keyed_tree, KeyedNode, Revision, UiDocument, WireValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValueExpr {
    Literal(WireValue),
    Path(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementTemplate {
    pub kind: String,
    pub key: Option<ValueExpr>,
    pub attrs: BTreeMap<String, ValueExpr>,
    pub children: Vec<TemplateChild>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemplateChild {
    Element(ElementTemplate),
    Text(ValueExpr),
    Each {
        variable: String,
        collection: ValueExpr,
        template: Box<ElementTemplate>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiTemplate {
    pub function: String,
    pub root: ElementTemplate,
}

/// Experimental compiler artifact. Contains declarative rendering plans,
/// never environment values, credentials, or executable handlers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiTemplateManifest {
    pub version: u32,
    pub templates: BTreeMap<String, UiTemplate>,
}

impl UiTemplateManifest {
    /// Select the supported JSX-root functions in one already-validated AST.
    /// Arbitrary functions are not reinterpreted as templates. Unsupported
    /// JSX remains served by the existing standard web renderer.
    pub fn compile_supported(module: &AstModule) -> Self {
        let mut templates = BTreeMap::new();
        for decl in &module.decls {
            let Decl::Function { name, body, .. } = decl else {
                continue;
            };
            let Ok(Expr::App { func, .. }) = last_expression(body) else {
                continue;
            };
            if !matches!(func.as_ref(), Expr::Var(symbol, _) if symbol == "el") {
                continue;
            }
            if let Ok(template) = UiTemplate::compile(module, name) {
                templates.insert(name.clone(), template);
            }
        }
        Self {
            version: 1,
            templates,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    UnknownFunction(String),
    UnsupportedExpression,
    UnsupportedAttribute(String),
    MissingBinding(String),
    InvalidValue(String),
    InvalidChildren,
    MissingKeyOnLoop,
    KeyedTree(String),
}

fn last_expression(expr: &Expr) -> Result<&Expr, TemplateError> {
    match expr {
        Expr::Block { exprs, .. } if exprs.len() == 1 => Ok(&exprs[0]),
        Expr::Block { .. } => Err(TemplateError::UnsupportedExpression),
        other => Ok(other),
    }
}

fn expr_as_value(expr: &Expr) -> Result<ValueExpr, TemplateError> {
    match expr {
        Expr::Literal(Literal::String(value), _) => {
            Ok(ValueExpr::Literal(WireValue::from(value.clone())))
        }
        Expr::Literal(Literal::Bool(value), _) => Ok(ValueExpr::Literal(WireValue::Bool(*value))),
        Expr::Literal(Literal::Int(value), _) => Ok(ValueExpr::Literal(WireValue::from(*value))),
        Expr::Var(name, _) => Ok(ValueExpr::Path(vec![name.clone()])),
        Expr::FieldAccess { expr, field, .. } => {
            let ValueExpr::Path(mut path) = expr_as_value(expr)? else {
                return Err(TemplateError::UnsupportedExpression);
            };
            path.push(field.clone());
            Ok(ValueExpr::Path(path))
        }
        Expr::App { func, args, .. }
            if matches!(func.as_ref(), Expr::Var(name, _) if name=="text") && args.len() == 1 =>
        {
            expr_as_value(&args[0])
        }
        _ => Err(TemplateError::UnsupportedExpression),
    }
}

fn allowed_tag(tag: &str) -> bool {
    matches!(
        tag,
        "a" | "article"
            | "aside"
            | "button"
            | "div"
            | "fieldset"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "header"
            | "input"
            | "label"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "option"
            | "p"
            | "section"
            | "select"
            | "span"
            | "strong"
            | "table"
            | "tbody"
            | "td"
            | "textarea"
            | "th"
            | "thead"
            | "tr"
            | "ul"
    )
}

fn allowed_attr(name: &str) -> bool {
    matches!(
        name,
        "class"
            | "id"
            | "title"
            | "role"
            | "name"
            | "type"
            | "placeholder"
            | "autocomplete"
            | "alt"
            | "href"
            | "src"
            | "target"
            | "rel"
            | "for"
            | "disabled"
            | "required"
            | "readonly"
            | "multiple"
            | "checked"
            | "value"
    ) || (name.starts_with("aria-")
        && name[5..]
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
}

fn compile_element(expr: &Expr) -> Result<ElementTemplate, TemplateError> {
    let Expr::App { func, args, .. } = last_expression(expr)? else {
        return Err(TemplateError::UnsupportedExpression);
    };
    if !matches!(func.as_ref(), Expr::Var(name, _) if name=="el") || args.len() != 3 {
        return Err(TemplateError::UnsupportedExpression);
    }
    let Expr::Literal(Literal::String(tag), _) = &args[0] else {
        return Err(TemplateError::UnsupportedExpression);
    };
    if !allowed_tag(tag) {
        return Err(TemplateError::UnsupportedExpression);
    }
    let Expr::Array(attrs, _) = &args[1] else {
        return Err(TemplateError::UnsupportedExpression);
    };
    let mut key = None;
    let mut properties = BTreeMap::new();
    for attr in attrs {
        let Expr::Tuple(pair, _) = attr else {
            return Err(TemplateError::UnsupportedExpression);
        };
        if pair.len() != 2 {
            return Err(TemplateError::UnsupportedExpression);
        }
        let Expr::Literal(Literal::String(name), _) = &pair[0] else {
            return Err(TemplateError::UnsupportedExpression);
        };
        if name == "key" || name == "data-nulang-key" {
            if key.is_some() {
                return Err(TemplateError::UnsupportedAttribute(name.clone()));
            }
            key = Some(expr_as_value(&pair[1])?);
            continue;
        }
        if !allowed_attr(name) || properties.contains_key(name) {
            return Err(TemplateError::UnsupportedAttribute(name.clone()));
        }
        properties.insert(name.clone(), expr_as_value(&pair[1])?);
    }
    let Expr::Array(children, _) = &args[2] else {
        return Err(TemplateError::UnsupportedExpression);
    };
    let mut compiled_children = Vec::new();
    for child in children {
        match child {
            Expr::For {
                var,
                iterable,
                body,
                ..
            } => {
                let template = compile_element(body)?;
                if template.key.is_none() {
                    return Err(TemplateError::MissingKeyOnLoop);
                }
                compiled_children.push(TemplateChild::Each {
                    variable: var.clone(),
                    collection: expr_as_value(iterable)?,
                    template: Box::new(template),
                });
            }
            Expr::App { func, .. } if matches!(func.as_ref(), Expr::Var(name, _) if name=="el") => {
                compiled_children.push(TemplateChild::Element(compile_element(child)?));
            }
            _ => compiled_children.push(TemplateChild::Text(expr_as_value(child)?)),
        }
    }
    Ok(ElementTemplate {
        kind: tag.clone(),
        key,
        attrs: properties,
        children: compiled_children,
    })
}

fn resolve_value(
    expr: &ValueExpr,
    env: &BTreeMap<String, WireValue>,
) -> Result<WireValue, TemplateError> {
    match expr {
        ValueExpr::Literal(value) => Ok(value.clone()),
        ValueExpr::Path(path) => {
            if path.is_empty() {
                return Err(TemplateError::UnsupportedExpression);
            }
            let mut current = env
                .get(&path[0])
                .ok_or_else(|| TemplateError::MissingBinding(path[0].clone()))?;
            for field in &path[1..] {
                let WireValue::Object(fields) = current else {
                    return Err(TemplateError::InvalidValue(path.join(".")));
                };
                current = fields
                    .get(field)
                    .ok_or_else(|| TemplateError::MissingBinding(path.join(".")))?;
            }
            Ok(current.clone())
        }
    }
}

fn into_string(value: WireValue) -> Result<String, TemplateError> {
    match value {
        WireValue::String(value) => Ok(value),
        WireValue::I64(value) => Ok(value.0.to_string()),
        _ => Err(TemplateError::InvalidValue(
            "expected string or integer".into(),
        )),
    }
}

fn valid_url(value: &str, kind: &str) -> bool {
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.chars().any(char::is_whitespace)
        || value.contains('\\')
        || value.starts_with("//")
    {
        return false;
    }
    if value.starts_with('/') || value.starts_with('#') || value.starts_with('?') {
        return true;
    }
    let lowered = value.to_ascii_lowercase();
    lowered.starts_with("https://")
        || lowered.starts_with("http://")
        || (kind == "href" && (lowered.starts_with("mailto:") || lowered.starts_with("tel:")))
}

fn render_element(
    template: &ElementTemplate,
    env: &BTreeMap<String, WireValue>,
    remaining: &mut usize,
    depth: usize,
) -> Result<KeyedNode, TemplateError> {
    if depth > 64 || *remaining == 0 {
        return Err(TemplateError::InvalidValue(
            "UI tree budget exceeded".into(),
        ));
    }
    *remaining -= 1;
    let mut node = KeyedNode::new(template.kind.clone());
    if let Some(key) = &template.key {
        node = node.with_key(into_string(resolve_value(key, env)?)?);
    }
    for (name, expr) in &template.attrs {
        let value = resolve_value(expr, env)?;
        if matches!(name.as_str(), "href" | "src") {
            let WireValue::String(ref text) = value else {
                return Err(TemplateError::InvalidValue(name.clone()));
            };
            if !valid_url(text, name) {
                return Err(TemplateError::InvalidValue(name.clone()));
            }
        } else if matches!(
            name.as_str(),
            "checked" | "disabled" | "required" | "readonly" | "multiple"
        ) {
            if !matches!(value, WireValue::Bool(_)) {
                return Err(TemplateError::InvalidValue(name.clone()));
            }
        } else if !matches!(value, WireValue::String(_)) {
            return Err(TemplateError::InvalidValue(name.clone()));
        }
        node.properties.insert(name.clone(), value);
    }
    let mut rendered_children = Vec::new();
    let mut text = String::new();
    let mut has_text = false;
    for child in &template.children {
        match child {
            TemplateChild::Text(expr) => {
                text.push_str(&into_string(resolve_value(expr, env)?)?);
                has_text = true;
            }
            TemplateChild::Element(element) => {
                rendered_children.push(render_element(element, env, remaining, depth + 1)?)
            }
            TemplateChild::Each {
                variable,
                collection,
                template,
            } => {
                let WireValue::Array(items) = resolve_value(collection, env)? else {
                    return Err(TemplateError::InvalidValue(
                        "loop collection must be array".into(),
                    ));
                };
                if items.len() > 10_000 {
                    return Err(TemplateError::InvalidValue(
                        "loop exceeds node budget".into(),
                    ));
                }
                for item in items {
                    let mut child_env = env.clone();
                    child_env.insert(variable.clone(), item);
                    if template.key.is_none() {
                        return Err(TemplateError::MissingKeyOnLoop);
                    }
                    rendered_children.push(render_element(
                        template,
                        &child_env,
                        remaining,
                        depth + 1,
                    )?);
                }
            }
        }
    }
    if has_text && !rendered_children.is_empty() {
        return Err(TemplateError::InvalidChildren);
    }
    if has_text {
        node.properties
            .insert("text".into(), WireValue::String(text));
    }
    Ok(node.with_children(rendered_children))
}

impl UiTemplate {
    /// Compile a function containing a single JSX root into a side-effect-free plan.
    pub fn compile(module: &AstModule, function: &str) -> Result<Self, TemplateError> {
        let body = module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Function { name, body, .. } if name == function => Some(body),
                _ => None,
            })
            .ok_or_else(|| TemplateError::UnknownFunction(function.to_owned()))?;
        Ok(Self {
            function: function.into(),
            root: compile_element(body)?,
        })
    }

    /// Instantiate with server-authorized values already bound by the caller.
    /// The input map is data, *never* a source of execution authority.
    pub fn render(
        &self,
        document_id: &str,
        revision: Revision,
        bindings: &BTreeMap<String, WireValue>,
    ) -> Result<UiDocument, TemplateError> {
        let mut remaining = 10_000;
        let tree = render_element(&self.root, bindings, &mut remaining, 0)?;
        document_from_keyed_tree(document_id, revision, &tree)
            .map_err(|error| TemplateError::KeyedTree(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(src: &str) -> AstModule {
        Parser::new(Lexer::new(src).lex().unwrap())
            .parse_module()
            .unwrap()
    }

    fn row(id: &str, label: &str) -> WireValue {
        WireValue::Object(BTreeMap::from([
            ("id".to_owned(), WireValue::from(id)),
            ("label".to_owned(), WireValue::from(label)),
        ]))
    }

    #[test]
    fn compiled_template_manifest_selects_supported_view_and_skips_effectful_code() {
        let module = parse(
            r#"
import stdlib::web::types
fn helper() -> String { "plain" }
fn valid() -> Html { <ul><li key="one">hello</li></ul> }
fn unsupported() -> Html { <div>{perform Web.param("id")}</div> }
"#,
        );
        let manifest = UiTemplateManifest::compile_supported(&module);
        assert_eq!(manifest.version, 1);
        assert_eq!(
            manifest.templates.keys().cloned().collect::<Vec<_>>(),
            vec!["valid"]
        );
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        assert_eq!(
            serde_json::from_str::<UiTemplateManifest>(&json).unwrap(),
            manifest
        );
    }

    #[test]
    fn compile_keyed_for_and_render_reorder_without_recreating_rows() {
        let module = parse(
            r#"
import stdlib::web::html
import stdlib::web::types
fn view() -> Html {
  <ul>{for item in rows { <li key={item.id}>{item.label}</li> }}</ul>
}
"#,
        );
        let plan = UiTemplate::compile(&module, "view").unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<UiTemplate>(&json).unwrap(), plan);
        let initial = BTreeMap::from([(
            "rows".into(),
            WireValue::Array(vec![row("a", "A"), row("é", "É")]),
        )]);
        let next = BTreeMap::from([(
            "rows".into(),
            WireValue::Array(vec![row("é", "É"), row("a", "A")]),
        )]);
        let a = plan.render("tasks", Revision(1), &initial).unwrap();
        let b = plan.render("tasks", Revision(2), &next).unwrap();
        assert_eq!(a.root.as_str(), "root");
        assert_eq!(
            a.nodes
                .iter()
                .find(|node| node.id.as_str() == "root")
                .unwrap()
                .children,
            vec!["root/k61".into(), "root/kc3a9".into()]
        );
        assert_eq!(
            a.diff(&b).unwrap().operations,
            vec![nulang_ui_protocol::PatchOperation::ReplaceChildren {
                node_id: "root".into(),
                children: vec!["root/kc3a9".into(), "root/k61".into()]
            }]
        );
    }

    #[test]
    fn compiler_rejects_unkeyed_loop_and_effectful_expression() {
        let unkeyed = parse(
            r#"
import stdlib::web::types
fn view() -> Html {
 <ul>{for item in rows { <li>{item.label}</li> }}</ul>
}
"#,
        );
        assert_eq!(
            UiTemplate::compile(&unkeyed, "view"),
            Err(TemplateError::MissingKeyOnLoop)
        );
        let effectful = parse(
            r#"
import stdlib::web::types
fn view() -> Html {
 <div>{perform Web.param("id")}</div>
}
"#,
        );
        assert_eq!(
            UiTemplate::compile(&effectful, "view"),
            Err(TemplateError::UnsupportedExpression)
        );
    }

    #[test]
    fn runtime_missing_values_and_duplicate_keys_fail_closed() {
        let module = parse(
            r#"import stdlib::web::types
fn view() -> Html {
          <ul>{for item in rows { <li key={item.id}>{item.label}</li> }}</ul>
        }"#,
        );
        let plan = UiTemplate::compile(&module, "view").unwrap();
        assert!(matches!(
            plan.render("tasks", Revision(1), &BTreeMap::new()),
            Err(TemplateError::MissingBinding(_))
        ));
        let dup = BTreeMap::from([(
            "rows".into(),
            WireValue::Array(vec![row("x", "A"), row("x", "B")]),
        )]);
        assert!(matches!(
            plan.render("tasks", Revision(1), &dup),
            Err(TemplateError::KeyedTree(_))
        ));
    }

    #[test]
    fn compiled_templates_never_execute_dynamic_functions_or_unsafe_urls() {
        let code = parse(
            r#"
import stdlib::web::types
fn view() -> Html { <a key={entry.id} href={entry.url}>{entry.title}</a> }
"#,
        );
        let plan = UiTemplate::compile(&code, "view").unwrap();
        let bad = BTreeMap::from([(
            "entry".into(),
            WireValue::Object(BTreeMap::from([
                ("id".into(), WireValue::from("x")),
                ("url".into(), WireValue::from("javascript:alert(1)")),
                ("title".into(), WireValue::from("Click")),
            ])),
        )]);
        assert!(matches!(
            plan.render("doc", Revision(1), &bad),
            Err(TemplateError::InvalidValue(_))
        ));
    }

    #[test]
    fn deserialized_plan_cannot_bypass_limits_or_dereference_empty_path() {
        let module = parse(
            r#"
import stdlib::web::types
fn view() -> Html { <ul>{for item in rows { <li key={item.id}>{item.label}</li> }}</ul> }
"#,
        );
        let plan = UiTemplate::compile(&module, "view").unwrap();
        let values = BTreeMap::from([(
            "rows".into(),
            WireValue::Array((0..10_001).map(|id| row(&id.to_string(), "x")).collect()),
        )]);
        assert!(matches!(
            plan.render("doc", Revision(1), &values),
            Err(TemplateError::InvalidValue(_))
        ));
        let mut tampered = plan.clone();
        let TemplateChild::Each { collection, .. } = &mut tampered.root.children[0] else {
            panic!("missing loop");
        };
        *collection = ValueExpr::Path(Vec::new());
        assert!(matches!(
            tampered.render("doc", Revision(1), &BTreeMap::new()),
            Err(TemplateError::UnsupportedExpression)
        ));
    }

    #[test]
    fn literal_dom_text_and_attributes_compile_consistently() {
        let module = parse(
            r#"import stdlib::web::types
fn view() -> Html {
          <section><p class="hello">{label}</p></section>
        }"#,
        );
        let plan = UiTemplate::compile(&module, "view").unwrap();
        let doc = plan
            .render(
                "doc",
                Revision(9),
                &BTreeMap::from([("label".into(), WireValue::from("Hello"))]),
            )
            .unwrap();
        let p = doc
            .nodes
            .iter()
            .find(|n| n.id.as_str() == "root/p0")
            .unwrap();
        assert_eq!(p.kind, "p");
        assert_eq!(p.properties["text"], WireValue::from("Hello"));
        assert_eq!(p.properties["class"], WireValue::from("hello"));
    }
}
