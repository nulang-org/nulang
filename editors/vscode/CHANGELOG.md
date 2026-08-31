# Changelog

All notable changes to the Nulang VS Code extension are documented here.

## [0.2.0] - Unreleased

### Added

- Language server client (`nulang --lsp` over stdio): diagnostics, hover,
  go-to-definition, references, document symbols, rename, signature help,
  formatting, semantic tokens, code actions, inlay hints, completion, code
  lens, and document links.
- Commands: **Nulang: Compile** (`--emit-nbc`), **Nulang: Run**, **Nulang:
  Type Check** (`--check`), **Nulang: Restart Language Server**.
- `nulang.path` configuration setting (explicit setting > `NULANG_PATH` >
  `PATH`).
- Integration test suite (VS Code + real language server).

### Changed

- The TextMate grammar is now sourced from the
  [nulang-syntax](https://github.com/nulang-org/nulang-syntax) package
  (single source of truth) instead of a local copy.

## [0.1.0] - Unreleased

### Added

- TextMate grammar for Nulang (`.nula`) covering the Frozen Core and Stable
  tiers of `spec/grammar.ebnf`: keywords, reference capabilities, types,
  effects, actors/behaviors, strings, chars, comments, numbers, operators,
  and `@annotations`.
- Language configuration: line/block comments, brackets, auto-closing and
  surrounding pairs, indentation rules, and region folding.
- Snippets for common forms: `fn`, `actor`, `behavior`, `spawn`, `send`,
  `handle`, `match`, `effect`, control flow, and `IO.print`.
