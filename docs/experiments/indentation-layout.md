# Experimental indentation layout for Nulang

**Status:** prototype; opt-in compiler library API only. The normal `Lexer::lex`,
`Parser`, CLI, REPL, formatter, and LSP keep their existing brace syntax.
This is not a language-version change or a promise of a second permanent syntax.

## Goal

Evaluate Python/Nim-style significant indentation without changing AST types,
type inference, actor semantics, effects, runtime behavior, or artifact formats.
`nulang::layout::lex_layout(source)` converts supported layout headers to
synthetic `{` and `}` tokens while preserving original byte-offset spans.
`nulang::layout::parse_layout_module(source)` then uses the ordinary parser.

### Prototype examples

```nulang
fn add(x: Int, y: Int) -> Int:
    let total = x + y
    total

actor Counter:
    state count: Int = 0

    behavior get() -> Int:
        self.count
```

The first scope supports function, actor, entity, organization, and behavior
headers ending in a colon. Each indented body must contain at least one
nonblank line. Blank lines and regular comments do not affect layout. Tabs in
significant indentation are rejected rather than assigned a platform-dependent
width. The existing brace form remains accepted by the layout lexer unchanged.

## Current limits (deliberate)

- This is a **library-level parsing experiment**, not an executable CLI flag.
  It does not yet integrate source formatting, LSP, full-module resolution,
  incremental parsing, or imported layout modules.
- No layout blocks for `if`, `match`, `handle`, `workflow`,
  `receive`, or nested `fn` expressions. Their conventional brace syntax
  remains unchanged.
- Header signatures must be on one logical line. Delimiter-nested multiline
  expressions remain explicit; implicit continuation without delimiters is not
  part of this experiment.
- This initial token lowering is not a replacement for grammar-aware lexing.
  In particular, mixing explicit and layout blocks should be treated as
  unsupported unless covered by conformance tests.
- Source syntax is **not** changed in `SPEC2.md` or `spec/grammar.ebnf`.
  That requires an RFC, complete conformance coverage, and a compatibility plan.

## Validation

```sh
cargo test --no-default-features --test layout_syntax
cargo test --no-default-features test_grammar_conformance
cargo fmt --check
```

Promotion criteria: complete negative tests (missing body, inconsistent
dedentation, mixed tabs/spaces, malformed nesting), differential AST and
typechecking tests against brace-form equivalents, fixture coverage for
actors/effects, source-location accuracy, formatter/LSP round-trip, and a
measured usability benefit that outweighs permanent syntax maintenance.

Do not enable this by default or rewrite existing source files until the RFC
has been accepted and the above tests pass on a clean exact-head build.
