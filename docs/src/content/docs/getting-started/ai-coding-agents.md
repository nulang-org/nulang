---
title: Nulang for AI Coding Agents
description: "A practical, verifiable guide for coding agents using Nulang: authoritative references, supported execution targets, CLI checks, testing, and feature maturity."
---

This page is for **AI coding assistants that write or modify Nulang programs**. If you are building LLM-powered agents *in* Nulang, start with the [experimental AI runtime](/ai/overview/) instead.

Nulang is **alpha**. Do not assume that a proposed RFC, roadmap description, historical design note, or open pull request represents supported behavior.

## Machine-readable entry points

Read only what the task requires:

| Need | Source |
| --- | --- |
| Compact documentation map | [llms.txt](/llms.txt) |
| All documentation, plain text | [llms-full.txt](/llms-full.txt) (large; prefer targeted pages) |
| Individual pages as Markdown | Append `.md` to the page's path, without the trailing slash: for example [quick-start.md](/getting-started/quick-start.md) |
| Installation and native execution | [Installation](/getting-started/installation/) |
| Supported syntax and types | [Syntax](/language/syntax/), [Types](/language/types/), [Effects](/language/effects/) |
| Actor programming | [Actor Model](/actors/overview/), [Supervision](/actors/supervision/) |
| CLI and editor tooling | [Language Server](/tooling/language-server/), [Testing](/tooling/testing/) |
| Implementation vs. proposals | [Implementation Status](https://github.com/nulang-org/nulang/blob/main/docs/IMPLEMENTATION_STATUS.md), [Changelog](https://github.com/nulang-org/nulang/blob/main/CHANGELOG.md) |
| Exact language contracts | [SPEC2](https://github.com/nulang-org/nulang/blob/main/SPEC2.md), [Governance](https://github.com/nulang-org/nulang/blob/main/GOVERNANCE.md) |
| Compiler behavior and tests | [Source repository](https://github.com/nulang-org/nulang), [Conformance cases](https://github.com/nulang-org/nulang/tree/main/conformance) |

**Evidence priority:** the compiler/runtime and passing executable tests on the version being used take precedence for actual behavior. Use SPEC2 and accepted RFCs for intended semantics; check the changelog and implementation-status notes for what has landed. Treat the website as guidance that can lag the current source.

## Minimal create → check → run loop

Use the [tagged release and checksum instructions](/getting-started/installation/) for a supported platform, or build from source when an appropriate artifact is not available.

Save this as `hello.nula`:

```nulang
fn main() {
    perform Test.assert_eq(6 * 7, 42)
    perform IO.print("Hello from Nulang")
}
```

Then run:

```bash
nulang --check hello.nula
nulang hello.nula
```

`--check` checks the source without executing it. Do **not** claim a program works merely because a code sample looks syntactically plausible: execute it against the user's installed Nulang version and report any errors.

For package projects:

```bash
nulang nula new hello-app
cd hello-app
nulang nula test --json
```

The package test runner reports test failures and compiler errors. See [Testing](/tooling/testing/) for test discovery, assertions, and flags. For the Nulang compiler repository itself, use its [documented contributor checks](https://github.com/nulang-org/nulang/blob/main/AGENTS.md), including `cargo test --locked`, rather than a package-level test alone.

## Choose the right runtime

| Capability | Current boundary |
| --- | --- |
| Functions, types, records, algebraic effects | Core language; start with the [Quick Start](/getting-started/quick-start/) |
| Actors and OTP-style supervision | Native runtime; see [Actor Model](/actors/overview/) |
| Persistent actors and crash recovery | Implemented in the reference runtime; verify store configuration and recovery for your workload |
| Multi-node distribution and CRDT integration | Experimental; do not assume transparent cluster failover or complete source-level CRDT state support |
| LLM providers, tools, memory, multi-agent coordination | Optional, experimental AI runtime; inspect the installed feature set |
| Portable WASM, WasmFX, native AOT | Experimental or subset-dependent; never assume parity with the bytecode semantic reference |

**The browser playground is Core-only.** It is useful for small language examples but cannot verify native actors, persistence, networking, FFI, or JIT behavior. Use a local binary for those workloads.

## Rules for reliable generated code

1. **Ground syntax in current examples or documentation.** Prefer the [Quick Start](/getting-started/quick-start/) and [conformance suite](https://github.com/nulang-org/nulang/tree/main/conformance) over guessed APIs.
2. **Check the feature tier before choosing an API.** Make experimental dependencies explicit, including build flags and external services.
3. **Keep effects and capabilities visible.** Do not silently strip effect annotations or bypass capability constraints to make a program compile.
4. **Verify both success and failure paths.** For durable work, test restart/recovery, retries, and external side-effect boundaries; do not infer exactly-once delivery from checkpoints alone.
5. **Never invent benchmark results or compatibility guarantees.** Use [Performance](/language/performance/) and reproducible benchmark sources, with matching hardware and workload.
6. **Report verification precisely.** Distinguish “written”, “type-checked”, “executed”, and “tested”; if you cannot run the compiler, say so.

## Working on the Nulang compiler itself

Read the repository's [AGENTS.md](https://github.com/nulang-org/nulang/blob/main/AGENTS.md) before modifying its Rust sources. It describes the frontend, bytecode semantic reference, alternative backends, runtime lifecycle, and required validation. Avoid broad changes based solely on an architectural sketch or an AI-generated snippet.

## Next steps

- [Quick Start](/getting-started/quick-start/) — first runnable programs
- [Testing](/tooling/testing/) — assertions and package test runner
- [AI Agents Runtime](/ai/overview/) — using language-level AI features (experimental)
- [Durable Workflows](/workflows/overview/) — recovery-oriented workflows (experimental)
