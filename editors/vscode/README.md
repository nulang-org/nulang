# Nulang for VS Code

Language support for [Nulang](https://github.com/nulang-org/nulang) in Visual Studio Code: syntax highlighting, language server integration, language essentials (comments, brackets, indentation, folding), and snippets.

## Features

- **Syntax highlighting** for `.nula` files via a TextMate grammar
  (`source.nulang`, maintained in
  [nulang-org/nulang-syntax](https://github.com/nulang-org/nulang-syntax)):
  - Keywords: `fn`, `let`, `const`, `type`, `alias`, `effect`, `actor`, `behavior`, `state`, `spawn`, `send`, `receive`, `handle`, `perform`, `resume`, `match`, `case`, `if`/`else`, `for`, `while`, `loop`, `return`, `import`, `pub`, `extern`, and more
  - Reference capabilities: `iso`, `trn`, `ref`, `val`, `box`, `tag`, `lineariso` (including `@cap` annotations)
  - Primitive and standard types, effect names (`IO`, `Http`, `Json`, `LLM`, ...), user-defined types
  - Strings with escape sequences, character literals, comments (`//` and `/* */`), numbers (int, float, hex, binary, octal), and operators (`->`, `=>`, `|>`, `!`, `<-`, `..`, ...)
  - Declaration highlighting: function, actor, behavior, type, and effect names
- **Language server** (`nulang --lsp`): diagnostics (parse/type/effect/capability), hover, go-to-definition, references, document symbols, rename, signature help, formatting, semantic tokens, code actions, inlay hints, completion, code lens, and document links
- **Language configuration**: comment toggling (line + block), bracket matching, auto-closing pairs, indentation rules, and region folding (`// #region` / `// #endregion`)
- **Snippets** for common forms: `fn`, `actor`, `behavior`, `spawn`, `send`, `handle`, `match`, `effect`, loops, and more
- **Commands** (Command Palette → "Nulang: ..."):
  - **Compile** — compile the active file to a `.nbc` artifact (`nulang --emit-nbc`)
  - **Run** — compile and run the active file
  - **Type Check** — type/effect/capability check only (`nulang --check`)
  - **Restart Language Server** — restart the LSP client

## Requirements

The `nulang` binary must be on your `PATH`, or point the `NULANG_PATH`
environment variable at it, or set the `nulang.path` setting explicitly.

## Configuration

| Setting | Default | Description |
|---------|---------|-------------|
| `nulang.path` | `nulang` | Path to the `nulang` binary. When not explicitly set, `NULANG_PATH` is consulted, then `PATH`. |

## Installation

### From a `.vsix`

```sh
cd editors/vscode
npm install
npx @vscode/vsce package
code --install-extension nulang-0.2.0.vsix
```

### Manual (development)

Symlink or copy this directory into your VS Code extensions folder:

```sh
ln -s "$PWD/editors/vscode" ~/.vscode/extensions/nulang
```

Then reload VS Code.

## Usage

Open any `.nula` file — the grammar activates automatically and the language
server starts on first open. Try the
[examples](https://github.com/nulang-org/nulang/tree/main/examples) in the
main repository.

## Other editors

Any editor with LSP client support can talk to `nulang --lsp` over stdio
(Neovim `nvim-lspconfig`, Emacs `lsp-mode`/`eglot`, Helix, Zed). Point your
LSP client at `nulang --lsp` for `.nula` files.

## License

Apache-2.0, same as the Nulang repository. See [LICENSE](./LICENSE).
