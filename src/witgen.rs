//! WIT (WASM Interface Type) world generator.
//!
//! Maps Nulang effect signatures to WIT interfaces so compiled Nulang
//! actors are valid WASI 0.2+ components pluggable into any compliant host.
//!
//! Each Nulang effect module (IO, Timer, Signal, Provider, etc.) becomes
//! a WIT `import` interface. The host provides matching `export` functions.
//! The component's effect `perform` calls compile to WIT import calls.
//!
//! ## Example
//!
//! A Nulang actor with effects `{IO, Timer}` generates:
//! ```wit
//! package nulang:generated;
//! world actor {
//!     import io: interface {
//!         print: func(msg: string);
//!         read: func() -> string;
//!     }
//!     import timer: interface {
//!         sleep: func(ms: u64);
//!     }
//!     export init: func() -> s64;
//!     export handle-message: func(msg: list<u8>) -> s64;
//!     export checkpoint: func() -> list<u8>;
//! }
//! ```

use crate::types::{Effect, EffectRow};
use std::collections::BTreeSet;

/// A single operation in a WIT interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitOp {
    pub name: String,
    pub params: Vec<(String, String)>, // (name, WIT type)
    pub result: Option<String>,        // WIT return type
}

/// A WIT interface (e.g., `interface io { ... }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitInterface {
    pub name: String,
    pub ops: Vec<WitOp>,
}

/// A complete WIT world definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitWorld {
    pub package: String,
    pub world_name: String,
    pub imports: Vec<WitInterface>,
    pub exports: Vec<WitOp>,
}

/// Mapping from Nulang effect names to their WIT interface definitions.
///
/// This is the canonical registry of built-in effects that can cross the
/// WASM component boundary. Custom effects are not yet supported.
pub fn builtin_effect_wit_interfaces() -> Vec<WitInterface> {
    vec![
        WitInterface {
            name: "io".into(),
            ops: vec![
                WitOp {
                    name: "print".into(),
                    params: vec![("msg".into(), "string".into())],
                    result: None,
                },
                WitOp {
                    name: "read".into(),
                    params: vec![],
                    result: Some("string".into()),
                },
            ],
        },
        WitInterface {
            name: "timer".into(),
            ops: vec![WitOp {
                name: "sleep".into(),
                params: vec![("ms".into(), "u64".into())],
                result: None,
            }],
        },
        WitInterface {
            name: "random".into(),
            ops: vec![WitOp {
                name: "u64".into(),
                params: vec![],
                result: Some("u64".into()),
            }],
        },
        WitInterface {
            name: "signal".into(),
            ops: vec![
                WitOp {
                    name: "wait".into(),
                    params: vec![("name".into(), "string".into())],
                    result: None,
                },
                WitOp {
                    name: "notify".into(),
                    params: vec![("name".into(), "string".into())],
                    result: None,
                },
            ],
        },
        WitInterface {
            name: "provider".into(),
            ops: vec![WitOp {
                name: "ask".into(),
                params: vec![
                    ("provider".into(), "string".into()),
                    ("prompt".into(), "string".into()),
                ],
                result: Some("string".into()),
            }],
        },
        WitInterface {
            name: "string".into(),
            ops: vec![
                WitOp {
                    name: "length".into(),
                    params: vec![("s".into(), "string".into())],
                    result: Some("s64".into()),
                },
                WitOp {
                    name: "char-at".into(),
                    params: vec![("s".into(), "string".into()), ("idx".into(), "s64".into())],
                    result: Some("s64".into()),
                },
                WitOp {
                    name: "from-char".into(),
                    params: vec![("code".into(), "s64".into())],
                    result: Some("string".into()),
                },
                WitOp {
                    name: "concat".into(),
                    params: vec![("a".into(), "string".into()), ("b".into(), "string".into())],
                    result: Some("string".into()),
                },
                WitOp {
                    name: "substring".into(),
                    params: vec![
                        ("s".into(), "string".into()),
                        ("start".into(), "s64".into()),
                        ("len".into(), "s64".into()),
                    ],
                    result: Some("string".into()),
                },
                WitOp {
                    name: "to-int".into(),
                    params: vec![("s".into(), "string".into())],
                    result: Some("s64".into()),
                },
                WitOp {
                    name: "to-float".into(),
                    params: vec![("s".into(), "string".into())],
                    result: Some("f64".into()),
                },
            ],
        },
        WitInterface {
            name: "fs".into(),
            ops: vec![
                WitOp {
                    name: "read".into(),
                    params: vec![("path".into(), "string".into())],
                    result: Some("string".into()),
                },
                WitOp {
                    name: "write".into(),
                    params: vec![
                        ("path".into(), "string".into()),
                        ("content".into(), "string".into()),
                    ],
                    result: None,
                },
                WitOp {
                    name: "append".into(),
                    params: vec![
                        ("path".into(), "string".into()),
                        ("content".into(), "string".into()),
                    ],
                    result: None,
                },
                WitOp {
                    name: "exists".into(),
                    params: vec![("path".into(), "string".into())],
                    result: Some("bool".into()),
                },
            ],
        },
        WitInterface {
            name: "array".into(),
            ops: vec![
                WitOp {
                    name: "length".into(),
                    params: vec![("arr".into(), "list<s64>".into())],
                    result: Some("s64".into()),
                },
                WitOp {
                    name: "push".into(),
                    params: vec![
                        ("arr".into(), "list<s64>".into()),
                        ("elem".into(), "s64".into()),
                    ],
                    result: Some("list<s64>".into()),
                },
                WitOp {
                    name: "new".into(),
                    params: vec![("len".into(), "s64".into()), ("init".into(), "s64".into())],
                    result: Some("list<s64>".into()),
                },
                WitOp {
                    name: "set".into(),
                    params: vec![
                        ("arr".into(), "list<s64>".into()),
                        ("idx".into(), "s64".into()),
                        ("val".into(), "s64".into()),
                    ],
                    result: Some("list<s64>".into()),
                },
                WitOp {
                    name: "slice".into(),
                    params: vec![
                        ("arr".into(), "list<s64>".into()),
                        ("start".into(), "s64".into()),
                        ("end".into(), "s64".into()),
                    ],
                    result: Some("list<s64>".into()),
                },
                WitOp {
                    name: "range".into(),
                    params: vec![("start".into(), "s64".into()), ("end".into(), "s64".into())],
                    result: Some("list<s64>".into()),
                },
            ],
        },
        WitInterface {
            name: "http".into(),
            ops: vec![
                WitOp {
                    name: "get".into(),
                    params: vec![("url".into(), "string".into())],
                    result: Some("string".into()),
                },
                WitOp {
                    name: "post".into(),
                    params: vec![
                        ("url".into(), "string".into()),
                        ("body".into(), "string".into()),
                    ],
                    result: Some("string".into()),
                },
            ],
        },
        WitInterface {
            name: "debug".into(),
            ops: vec![WitOp {
                name: "inspect".into(),
                params: vec![
                    ("label".into(), "string".into()),
                    ("value".into(), "s64".into()),
                ],
                result: Some("s64".into()),
            }],
        },
        WitInterface {
            name: "int".into(),
            ops: vec![
                WitOp {
                    name: "to-string".into(),
                    params: vec![("n".into(), "s64".into())],
                    result: Some("string".into()),
                },
                WitOp {
                    name: "to-float".into(),
                    params: vec![("n".into(), "s64".into())],
                    result: Some("f64".into()),
                },
                WitOp {
                    name: "to-hex".into(),
                    params: vec![("n".into(), "s64".into())],
                    result: Some("string".into()),
                },
                WitOp {
                    name: "to-binary".into(),
                    params: vec![("n".into(), "s64".into())],
                    result: Some("string".into()),
                },
            ],
        },
        WitInterface {
            name: "float".into(),
            ops: vec![
                WitOp {
                    name: "to-int".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("s64".into()),
                },
                WitOp {
                    name: "to-string".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("string".into()),
                },
                WitOp {
                    name: "sin".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("f64".into()),
                },
                WitOp {
                    name: "cos".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("f64".into()),
                },
                WitOp {
                    name: "tan".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("f64".into()),
                },
                WitOp {
                    name: "sqrt".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("f64".into()),
                },
                WitOp {
                    name: "log".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("f64".into()),
                },
                WitOp {
                    name: "exp".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("f64".into()),
                },
                WitOp {
                    name: "log2".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("f64".into()),
                },
                WitOp {
                    name: "log10".into(),
                    params: vec![("x".into(), "f64".into())],
                    result: Some("f64".into()),
                },
                WitOp {
                    name: "pow".into(),
                    params: vec![("base".into(), "f64".into()), ("exp".into(), "f64".into())],
                    result: Some("f64".into()),
                },
            ],
        },
    ]
}

/// Error returned when a typed effect row cannot be represented as a closed WIT capability set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WitGenError {
    /// Open rows may acquire additional effects through their row variable, so
    /// emitting a closed capability manifest would be unsound.
    OpenEffectRow,
    /// Effects with no canonical WIT host interface must be mapped explicitly
    /// before they can cross a component boundary.
    UnsupportedEffects(Vec<String>),
}

impl std::fmt::Display for WitGenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WitGenError::OpenEffectRow => write!(
                f,
                "cannot emit a closed WIT capability manifest from an open effect row"
            ),
            WitGenError::UnsupportedEffects(effects) => write!(
                f,
                "no WIT interface mapping for effect(s): {}",
                effects.join(", ")
            ),
        }
    }
}

impl std::error::Error for WitGenError {}

/// Map a compiler-level effect to the canonical WIT host interface.
///
/// This is intentionally fail-closed: effects not listed here are not silently
/// erased from the component's authority contract. A few legacy stdlib modules
/// are represented as user-defined effects until they receive dedicated
/// `Effect` variants, so those names are mapped explicitly as well.
pub fn effect_to_wit_interface_name(effect: &Effect) -> Option<&'static str> {
    match effect {
        Effect::IO => Some("io"),
        Effect::Net => Some("http"),
        Effect::String => Some("string"),
        Effect::FS => Some("fs"),
        Effect::Rand => Some("random"),
        Effect::Time => Some("timer"),
        Effect::Inference => Some("provider"),
        Effect::Array => Some("array"),
        Effect::UserDefined(name) => match name.as_str() {
            "Debug" => Some("debug"),
            "Int" => Some("int"),
            "Float" => Some("float"),
            "Signal" => Some("signal"),
            "Provider" => Some("provider"),
            "Random" => Some("random"),
            "Timer" => Some("timer"),
            _ => None,
        },
        _ => None,
    }
}

/// Convert a closed compiler effect row into the exact WIT interfaces it
/// requires. Open rows are rejected because their authority is not statically
/// closed; unmapped effects are rejected rather than silently under-granting.
pub fn effect_row_to_wit_imports(row: &EffectRow) -> Result<BTreeSet<String>, WitGenError> {
    if matches!(row, EffectRow::Open(_, _)) {
        return Err(WitGenError::OpenEffectRow);
    }

    let mut imports = BTreeSet::new();
    let mut unsupported = BTreeSet::new();
    for effect in row.effects() {
        if let Some(name) = effect_to_wit_interface_name(effect) {
            imports.insert(name.to_string());
        } else {
            unsupported.insert(effect.to_string());
        }
    }

    if unsupported.is_empty() {
        Ok(imports)
    } else {
        Err(WitGenError::UnsupportedEffects(
            unsupported.into_iter().collect(),
        ))
    }
}

/// Generate a WIT world from one or more compiler-checked function effect
/// rows. Imports are deduplicated across functions.
pub fn generate_wit_world_from_effect_rows<'a>(
    rows: impl IntoIterator<Item = &'a EffectRow>,
) -> Result<WitWorld, WitGenError> {
    let mut effects = BTreeSet::new();
    for row in rows {
        effects.extend(effect_row_to_wit_imports(row)?);
    }
    Ok(generate_wit_world(&effects))
}

/// Generate a WIT world for the given set of Nulang effect names.
///
/// `effects` is a set of effect module names (e.g., `{"io", "timer"}`).
/// Only built-in effects with known WIT mappings are included; unknown
/// effects are silently skipped.
pub fn generate_wit_world(effects: &BTreeSet<String>) -> WitWorld {
    let all_interfaces = builtin_effect_wit_interfaces();
    let imports: Vec<WitInterface> = all_interfaces
        .into_iter()
        .filter(|iface| effects.contains(&iface.name))
        .collect();

    WitWorld {
        package: "nulang:generated".into(),
        world_name: "actor".into(),
        imports,
        exports: vec![
            WitOp {
                name: "init".into(),
                params: vec![],
                result: Some("s64".into()),
            },
            WitOp {
                name: "handle-message".into(),
                params: vec![("msg".into(), "list<u8>".into())],
                result: Some("s64".into()),
            },
            WitOp {
                name: "checkpoint".into(),
                params: vec![],
                result: Some("list<u8>".into()),
            },
        ],
    }
}

/// Render a WIT world to its text representation.
pub fn render_wit(world: &WitWorld) -> String {
    let mut out = String::new();

    // Package header
    out.push_str(&format!("package {};\n", world.package));
    out.push_str(&format!("world {} {{\n", world.world_name));

    // Imports
    for iface in &world.imports {
        out.push_str(&format!("    import {}: interface {{\n", iface.name));
        for op in &iface.ops {
            let params: Vec<String> = op
                .params
                .iter()
                .map(|(n, t)| format!("{}: {}", n, t))
                .collect();
            let params_str = params.join(", ");
            match &op.result {
                Some(ret) => out.push_str(&format!(
                    "        {}: func({}) -> {};\n",
                    op.name, params_str, ret
                )),
                None => out.push_str(&format!("        {}: func({});\n", op.name, params_str)),
            }
        }
        out.push_str("    }\n");
    }

    // Exports
    for op in &world.exports {
        let params: Vec<String> = op
            .params
            .iter()
            .map(|(n, t)| format!("{}: {}", n, t))
            .collect();
        let params_str = params.join(", ");
        match &op.result {
            Some(ret) => out.push_str(&format!(
                "    export {}: func({}) -> {};\n",
                op.name, params_str, ret
            )),
            None => out.push_str(&format!("    export {}: func({});\n", op.name, params_str)),
        }
    }

    out.push_str("}\n");
    out
}

/// Legacy textual effect extraction helper.
///
/// New compiler paths should use [`effect_row_to_wit_imports`] or
/// [`generate_wit_world_from_effect_rows`], which consume checked `EffectRow`
/// values and therefore include transitive callee effects and reject open rows.
pub fn extract_effects_from_source(source: &str) -> BTreeSet<String> {
    let mut effects = BTreeSet::new();

    // Look for `perform <Effect>.<op>(...)` patterns
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("perform ") {
            if let Some(dot_pos) = rest.find('.') {
                let effect_name = rest[..dot_pos].trim().to_lowercase();
                // Map Nulang effect names to WIT interface names
                let wit_name = match effect_name.as_str() {
                    "io" => "io",
                    "timer" => "timer",
                    "random" => "random",
                    "signal" => "signal",
                    "provider" | "inference" => "provider",
                    "string" => "string",
                    "fs" => "fs",
                    "array" => "array",
                    "http" => "http",
                    "debug" => "debug",
                    "int" => "int",
                    "float" => "float",
                    _ => continue, // unknown effect, skip
                };
                effects.insert(wit_name.to_string());
            }
        }
    }

    effects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_wit_empty() {
        let effects = BTreeSet::new();
        let world = generate_wit_world(&effects);
        assert!(world.imports.is_empty());
        assert_eq!(world.exports.len(), 3);
    }

    #[test]
    fn test_generate_wit_io_only() {
        let effects: BTreeSet<String> = ["io".into()].into();
        let world = generate_wit_world(&effects);
        assert_eq!(world.imports.len(), 1);
        assert_eq!(world.imports[0].name, "io");
    }

    #[test]
    fn test_generate_wit_io_and_timer() {
        let effects: BTreeSet<String> = ["io".into(), "timer".into()].into();
        let world = generate_wit_world(&effects);
        assert_eq!(world.imports.len(), 2);
    }

    #[test]
    fn test_render_wit_io_only() {
        let effects: BTreeSet<String> = ["io".into()].into();
        let world = generate_wit_world(&effects);
        let wit = render_wit(&world);
        assert!(wit.contains("package nulang:generated;"));
        assert!(wit.contains("import io: interface {"));
        assert!(wit.contains("print: func(msg: string);"));
        assert!(wit.contains("export init: func() -> s64;"));
    }

    #[test]
    fn typed_effect_row_maps_to_wit_imports() {
        let row = EffectRow::Closed(vec![
            Effect::IO,
            Effect::FS,
            Effect::Inference,
            Effect::UserDefined("Int".into()),
        ]);
        let imports = effect_row_to_wit_imports(&row).unwrap();
        let expected: BTreeSet<String> =
            ["fs", "int", "io", "provider"].into_iter().map(str::to_string).collect();
        assert_eq!(imports, expected);
    }

    #[test]
    fn typed_effect_rows_deduplicate_imports() {
        let a = EffectRow::Closed(vec![Effect::IO, Effect::FS]);
        let b = EffectRow::Closed(vec![Effect::IO, Effect::Rand]);
        let world = generate_wit_world_from_effect_rows([&a, &b]).unwrap();
        let names: Vec<_> = world.imports.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["io", "random", "fs"]);
    }

    #[test]
    fn open_effect_row_fails_closed() {
        let row = EffectRow::Open(vec![Effect::IO], crate::types::Region(1));
        assert_eq!(
            effect_row_to_wit_imports(&row),
            Err(WitGenError::OpenEffectRow)
        );
    }

    #[test]
    fn unmapped_effect_fails_closed() {
        let row = EffectRow::Closed(vec![Effect::DB, Effect::IO]);
        assert_eq!(
            effect_row_to_wit_imports(&row),
            Err(WitGenError::UnsupportedEffects(vec!["DB".into()]))
        );
    }

    #[test]
    fn test_extract_effects() {
        let source = r#"
            fn main() {
                perform IO.print("hello");
                perform Timer.sleep(100);
            }
        "#;
        let effects = extract_effects_from_source(source);
        assert!(effects.contains("io"));
        assert!(effects.contains("timer"));
        assert_eq!(effects.len(), 2);
    }

    #[test]
    fn test_extract_effects_empty() {
        let effects = extract_effects_from_source("fn main() { 42 }");
        assert!(effects.is_empty());
    }
}
