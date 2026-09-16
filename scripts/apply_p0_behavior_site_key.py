#!/usr/bin/env python3
from pathlib import Path


def replace_exact(text: str, old: str, new: str, count: int, label: str) -> str:
    actual = text.count(old)
    if actual != count:
        raise SystemExit(f"{label}: expected {count} matches, found {actual}")
    return text.replace(old, new)


def main() -> None:
    tc_path = Path("src/typechecker.rs")
    tc = tc_path.read_text()
    tc = replace_exact(
        tc,
        "FxHashMap<(u32, u32), crate::semantic_schema::BehaviorKey>",
        "FxHashMap<(u32, u32, String), crate::semantic_schema::BehaviorKey>",
        1,
        "typechecker behavior-site key type",
    )
    tc = replace_exact(
        tc,
        ".insert((span.start, span.end), key);",
        ".insert((span.start, span.end, behavior.to_string()), key);",
        2,
        "typechecker behavior-site key writes",
    )
    tc_path.write_text(tc)

    hir_path = Path("src/hir_lower.rs")
    hir = hir_path.read_text()
    hir = replace_exact(
        hir,
        "(u32, u32),\n        crate::semantic_schema::BehaviorKey,",
        "(u32, u32, String),\n        crate::semantic_schema::BehaviorKey,",
        1,
        "HIR behavior-site argument type",
    )
    hir = replace_exact(
        hir,
        "behavior_key: resolved_behavior_key(*span),",
        "behavior_key: resolved_behavior_key(*span, behavior),",
        2,
        "HIR behavior key lookup calls",
    )
    hir = replace_exact(
        hir,
        "fn resolved_behavior_key(span: Span) -> Option<crate::semantic_schema::BehaviorKey> {",
        "fn resolved_behavior_key(\n    span: Span,\n    behavior: &str,\n) -> Option<crate::semantic_schema::BehaviorKey> {",
        1,
        "HIR behavior key lookup signature",
    )
    hir = replace_exact(
        hir,
        ".and_then(|map| map.get(&(span.start, span.end)).cloned())",
        ".and_then(|map| map.get(&(span.start, span.end, behavior.to_string())).cloned())",
        1,
        "HIR behavior key lookup tuple",
    )
    hir = replace_exact(
        hir,
        "FxHashMap<(u32, u32), crate::semantic_schema::BehaviorKey>",
        "FxHashMap<(u32, u32, String), crate::semantic_schema::BehaviorKey>",
        1,
        "HIR behavior-site thread-local type",
    )
    hir_path.write_text(hir)


if __name__ == "__main__":
    main()
