---
title: Standard Library Modules
description: Canonical Nulang-authored standard-library modules and their stability tiers.
---

> **Generated from `spec/stdlib/v0alpha1.json`.** Do not edit this page by hand.

These modules are written in Nulang and sit above the built-in effect operations.
The manifest is also used to validate official package mirrors so the package
registry cannot silently drift from the in-tree standard library.

| Module | Import | Stability | Source | Package mirror | Description |
|---|---|---|---|---|---|
| `core` | `stdlib::core` | experimental | `src/stdlib/core.nula` | — | General-purpose core combinators. Prelude-owned Option and Result types are available without importing this module. |
| `math` | `stdlib::math` | experimental | `src/stdlib/math.nula` | — | Numeric helpers including abs, min, max, clamp, pow, factorial, gcd, and sqrt. |
| `list` | `stdlib::list` | experimental | `src/stdlib/list.nula` | — | Functional array/list combinators including map, filter, fold, append, reverse, and sort. |
| `string` | `stdlib::string` | experimental | `src/stdlib/string.nula` | — | String helpers including trim, split, join, replace, case conversion, and search. |
| `map` | `stdlib::map` | experimental | `src/stdlib/map.nula` | — | Persistent map helpers implemented in Nulang over array-backed key/value records. |
| `set` | `stdlib::set` | experimental | `src/stdlib/set.nula` | — | Persistent set helpers implemented in Nulang over arrays. |
| `result` | `stdlib::result` | experimental | `src/stdlib/result.nula` | — | Operations on the prelude-owned Result type. |
| `option` | `stdlib::option` | experimental | `src/stdlib/option.nula` | — | Operations on the prelude-owned Option type. |
| `datetime` | `stdlib::datetime` | experimental | `src/stdlib/datetime.nula` | — | DateTime record helpers and validation. |
| `http` | `stdlib::http` | experimental | `src/stdlib/http.nula` | — | Higher-level HTTP client wrappers over the built-in Http effect. |
| `fs` | `stdlib::fs` | experimental | `src/stdlib/fs.nula` | — | Higher-level filesystem wrappers over the built-in FS effect. |
| `json` | `stdlib::json` | experimental | `src/stdlib/json.nula` | `packages/json/src/lib.nula` | JSON parsing, serialization, and field accessors. |
| `test` | `stdlib::test` | experimental | `src/stdlib/test.nula` | — | Testing assertions and helpers over the built-in Test effect. |

## Contract

- Module names are unique and imports must be exactly `stdlib::<name>`.
- Every declared source file must exist.
- Stability is one of `frozen`, `stable`, or `experimental`.
- Declared package mirrors are generated from their stdlib source and checked in CI.
- Built-in effect operations remain compiler/runtime registry data and are exposed alongside
  these module descriptors through `nulang::stdlib`; they are not independently reconstructed by docs tooling.
