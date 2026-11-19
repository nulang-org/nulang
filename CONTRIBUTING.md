# Contributing to Nulang

Nulang is a **pre-1.0 alpha** typed language and runtime. Contributions of any
size are welcome: documentation corrections, runnable examples, reproductions,
tests, compiler/runtime fixes, editor tooling, and carefully scoped design
proposals. This guide describes how to get a contribution accepted with
minimal back-and-forth.

## Start here

- [Roadmap](ROADMAP.md): priorities, blockers, and live issue links.
- [Open issues](https://github.com/nulang-org/nulang/issues): check for an
  existing report before creating a new one.
- [Issue forms](https://github.com/nulang-org/nulang/issues/new/choose):
  report a reproducible bug, request an improvement, or submit a
  documentation correction (no Rust setup required for typo reports).
- [Discussions](https://github.com/nulang-org/nulang/discussions): questions,
  help getting started, or design exploration before a large PR.
- [Architecture](ARCHITECTURE.md),
  [implementation status](docs/IMPLEMENTATION_STATUS.md),
  [governance](GOVERNANCE.md) and [specification](SPEC2.md) are the main
  references for implementation and language behavior.

**For your first PR:** choose one documentation/example improvement, one
regression test, or one small bug fix. Browse the [curated first-issue queue](https://github.com/nulang-org/nulang/issues?q=is%3Aissue+is%3Aopen+label%3Agood-first-issue)
for small, scoped tasks. The queue may be empty at times; a label is not
a promise of mentorship or approval. Comment before starting substantive work
to confirm ownership and intended scope. If none fit, ask in Discussions.

## Local setup

Use the pinned Rust toolchain in [rust-toolchain.toml](rust-toolchain.toml)
(currently Rust 1.95.0), Git, Cargo, and Python 3. The most thoroughly
exercised source-development path is Linux; macOS can work; on Windows, WSL
is the safer choice for repo tooling even though release archives include a
Windows binary. Some features need additional system libraries. Docs-site
work also uses Node.js 22 and pnpm (see [docs/package.json](docs/package.json)).

```sh
git clone https://github.com/nulang-org/nulang.git
cd nulang
cargo build --locked
cargo test --locked
cargo run --locked -- examples/01_hello.nula
```

If the default-feature build has a platform-specific prerequisite failure,
share the **first error**, OS, Rust version, selected features, and commit SHA.
Do not modify the language semantics just to bypass a local environment issue.

## Find the right area

| Contribution | Starting point |
| --- | --- |
| Parser, syntax, diagnostics | `src/lexer.rs`, `src/parser.rs`, `src/typechecker.rs`; `SPEC2.md` |
| Compiler passes and backends | `src/hir_lower.rs`, `src/mir_lower.rs`, `src/mir_codegen.rs`, `src/mir_wasm.rs` |
| Actors, persistence, distribution | `src/runtime/`; semantic stabilization contract |
| Standard library, examples | `src/stdlib/`, `examples/`, `examples/README.md` |
| Editor tools | `editors/`; CLI LSP implementation |
| Docs site | `docs/src/content/docs/`, `docs/astro.config.mjs` |
| Conformance, formal methods | `conformance/`, `spec/formal/`, `tests/` |

Check the file's existing tests and the relevant [RFC](RFC/) before editing.
Small focused commits are easier to review than unrelated cleanups.

## Development and verification

Run the smallest relevant check while iterating, then expand before marking
the PR ready. Commands below reflect the current
[Justfile](Justfile) and [CI workflow](.github/workflows/ci.yml):

```sh
cargo fmt --check
cargo check --locked --tests
cargo test --locked                           # includes default-feature tests
cargo test --locked --features wasm-backend   # if changing WASM feature paths
cargo clippy --locked --all-targets -- -D clippy::correctness
```

If you have [just](https://github.com/casey/just) and
[cargo-nextest](https://nexte.st/), `just fast-check` runs format,
compile and default-feature tests; `just check` adds local lint, doctests,
and invariant verification. Otherwise the Cargo commands above are valid
starting points. The CI matrix contains additional no-default-features,
release, formal, audit, documentation and backend checks. Don't claim you
ran checks you did not run.

### If CI is blocked

A check that is **queued, cancelled, or skipped** is not a passing test.
First verify the PR's current head SHA and distinguish infrastructure problems
from a completed failed test. Do not repeatedly rerun queued workflows, disable
branch protection, or merge based on a separate deployment preview. If a job
actually fails, link its exact run and first relevant error in your PR; if
independent PRs are all queued, notify maintainers with example run URLs.
See [CI queue investigation #1431](https://github.com/nulang-org/nulang/issues/1431)
for the current incident, rather than opening duplicate reports.

For docs-site content:

```sh
cd docs
corepack enable
pnpm install --frozen-lockfile
pnpm run check
pnpm run build
```

The site build also prepares the browser playground and can require the
pinned Rust toolchain with `wasm32-unknown-unknown` plus build tooling.
For just a Markdown change, explain if you could only validate links and
structure locally; CI is the final integration check.

For behavior changes, add a failing regression first, verify the new behavior,
and run adjacent tests. For semantics, actors, durability, distribution, FFI,
and backend differences, include boundary/failure-mode tests and preserve the
bytecode reference semantics. Performance PRs need matched-environment
baseline/candidate evidence, not cross-machine rankings.

## Before opening a PR

1. Search issues/PRs. For broad work, open or discuss the problem before
   implementing it; use a draft PR for work needing early design feedback.
2. Fork or branch from `main`, make one coherent change, and use a
   [Conventional Commit](https://www.conventionalcommits.org/) style PR
   title such as `fix(parser): reject malformed tuple` or
   `docs: clarify effect examples`. The PR title is checked by CI.
3. Link the related issue, explain **why**, what changed, how it was tested,
   compatibility/migration risk, and what remains unverified. Fill in the
   [PR template](.github/PULL_REQUEST_TEMPLATE.md).
4. Add or update tests/examples/docs for behavior changes. Add a
   `CHANGELOG.md` entry under the correct stability tier for each
   user-visible change. The CI changelog gate applies to `feat`, `fix`,
   and `perf` PR titles unless maintainers apply `skip-changelog`.
5. Do not bundle formatting unrelated to your change, unrequested dependency
   upgrades, generated artifacts, or other contributors' work.
6. Respond to review feedback on the same PR; only mark resolved when the
   requested change is addressed. Maintainers decide when exact-head CI and
   review requirements have been met; a green local run does not replace CI.

## Compatibility and design gate

The [governance policy](GOVERNANCE.md) defines **Frozen**, **Stable**, and
**Experimental** surfaces. Bug fixes and Experimental additions may not need
an RFC; changes to Frozen or Stable language surfaces **do**. Start from
[RFC/0000-template.md](RFC/0000-template.md), discuss the proposal in an issue,
and obtain acceptance before implementation. Accepted RFCs are immutable.

Before proposing new language-core syntax or primitives, explain why the
problem cannot be solved in a library, capability package, compiler plugin,
WASM component, or cloud service. See the active
[semantic stabilization contract](docs/SEMANTIC_STABILIZATION_CONTRACT.md):
correctness and preserving existing guarantees outrank surface expansion.

## Communication and safety

Be constructive, stay focused on evidence, and follow our
[Code of Conduct](CODE_OF_CONDUCT.md).
Never disclose a vulnerability or exploit in a public issue or PR;
follow [SECURITY.md](SECURITY.md) for private reporting. Don't include API
keys, personal data, proprietary corpora, or unredacted logs in reproductions.
You keep ownership of your contribution; submitted code is contributed under
the repository's [Apache-2.0 license](LICENSE).

**Questions?** Use [Discussions](https://github.com/nulang-org/nulang/discussions).
**Ready to report?** Use [Issues](https://github.com/nulang-org/nulang/issues/new/choose).
