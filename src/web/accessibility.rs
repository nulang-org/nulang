//! Conservative, local HTML diagnostics over parser-owned markup only.
//! Dynamic keys/attributes, function-valued helpers, and rendered DOM
//! relationships are intentionally not inferred here.

use crate::ast::{Expr, Literal};
use crate::types::{NuWarning, Span};

fn literal_string(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Literal(Literal::String(value), _) => Some(value),
        _ => None,
    }
}

fn literal_text(expr: &Expr, sugar: bool) -> Option<&str> {
    match expr {
        Expr::Perform {
            effect, op, args, ..
        } if effect == "Web" && op == "text" => match args.as_slice() {
            [value] => literal_string(value),
            _ => None,
        },
        Expr::App { func, args, .. }
            if sugar && matches!(func.as_ref(), Expr::Var(name, _) if name == "text") =>
        {
            match args.as_slice() {
                [value] => literal_string(value),
                _ => None,
            }
        }
        _ => literal_string(expr),
    }
}

fn array(expr: &Expr) -> Option<&[Expr]> {
    match expr {
        Expr::Array(items, _) => Some(items),
        _ => None,
    }
}

fn warning(code: &'static str, msg: String, help: &str, span: Span) -> NuWarning {
    NuWarning {
        code,
        msg,
        help: Some(help.to_owned()),
        span,
    }
}

/// Check explicit Web.html, not arbitrary functions named el/document.
pub(crate) fn web_html_warnings(args: &[Expr], span: Span) -> Vec<NuWarning> {
    match args {
        [tag, attrs, children] => match literal_string(tag) {
            Some(tag) => element_warnings(tag, array(attrs), array(children), span, false),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

pub(crate) fn element_warnings(
    tag: &str,
    attrs: Option<&[Expr]>,
    children: Option<&[Expr]>,
    span: Span,
    sugar: bool,
) -> Vec<NuWarning> {
    let tag = tag.to_ascii_lowercase();
    let mut warnings = Vec::new();
    if crate::web::html_safety::is_void_element(&tag)
        && children.is_some_and(|children| !children.is_empty())
    {
        warnings.push(warning(
            "W0204",
            format!("void HTML element <{tag}> cannot have children"),
            "Use an empty children array or self-closing HTML syntax.",
            span,
        ));
    }

    if !matches!(tag.as_str(), "html" | "img" | "input" | "button" | "a") {
        return warnings;
    }

    // A dynamic attribute list or name may supply the missing attribute.
    // Do not infer its absence. Attribute values may remain dynamic.
    let Some(attrs) = attrs else {
        return warnings;
    };
    let Some(attrs) = attrs
        .iter()
        .map(|attr| match attr {
            Expr::Tuple(items, _) => match items.as_slice() {
                [name, value] => literal_string(name).map(|name| (name, value)),
                _ => None,
            },
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
    else {
        return warnings;
    };
    let attr = |name: &str| {
        attrs
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    };
    let known_text = |name: &str| attr(name).and_then(|value| literal_text(value, sugar));
    if tag == "html"
        && (attr("lang").is_none()
            || known_text("lang").is_some_and(|value| value.trim().is_empty()))
    {
        warnings.push(warning(
            "W0201",
            "HTML document has no known nonempty lang attribute".into(),
            "Set lang to the actual BCP 47 language of the page; use document_lang for complete pages.",
            span,
        ));
    }

    let image_input = tag == "input"
        && known_text("type").is_some_and(|value| value.eq_ignore_ascii_case("image"));
    if ((tag == "img" || image_input) && attr("alt").is_none())
        || (image_input && known_text("alt").is_some_and(|value| value.trim().is_empty()))
    {
        warnings.push(warning(
            "W0202",
            if image_input {
                "HTML image button has no known nonempty alt attribute".into()
            } else {
                "HTML <img> image has no alt attribute".into()
            },
            if image_input {
                "Provide alt text describing the action of the image button."
            } else {
                "Provide descriptive alt text, or alt=\"\" for a purely decorative image."
            },
            span,
        ));
    }

    // Do not infer absence of an external label for an element with an id.
    // Dynamic children, aria references, and hidden controls are also unknown.
    if (tag == "button" || (tag == "a" && attr("href").is_some()))
        && children.is_some_and(|children| children.is_empty())
        && ["aria-label", "aria-labelledby", "title", "id", "hidden"]
            .iter()
            .all(|name| attr(name).is_none())
        && !known_text("aria-hidden").is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        warnings.push(warning(
            "W0203",
            format!("empty HTML <{tag}> has no local accessible name"),
            "Provide visible text or an appropriate accessible name; verify any external label.",
            span,
        ));
    }
    warnings
}
