---
title: Why Nulang?
description: Where Nulang's typed actor model helps, how it differs from Erlang, Rust, Go, and TypeScript, and when to choose another language.
---

Nulang explores a particular design: **typed effects + isolated actors + supervision + opt-in durability** in one language and runtime. The objective is to make failure handling visible in application code rather than spreading it across unrelated libraries.

**Current status: alpha.** Some language and actor surfaces are designated stable under the project's change-control policy. That is **not** a promise of production readiness, performance leadership, compatibility with existing ecosystems, or proven distributed failover. Multi-node distribution, AI integrations, and alternate compilation backends remain experimental.

## What makes the design interesting?

- **Types and effects.** Hindley-Milner inference, row-polymorphic records and effects, and reference capabilities express more constraints before execution.
- **Actors and supervision.** Addressable actors isolate mutable state, exchange messages, and can be linked, monitored, and restarted through supervisors.
- **Explicit durability.** Ordinary actors are in-memory. Persistent actors opt into snapshots, journals, and restart hydration using configured storage. Durability still requires failure-injection testing and an appropriate store for the intended guarantees.
- **One compiler pipeline.** The project has a bytecode virtual machine with optional Cranelift JIT tiering, plus experimental WASM and native/AOT routes.

These are architectural properties, not comparative throughput or reliability results. Evaluate both correctness and performance on **your actual workload**.

## Nulang versus established alternatives

| If you need... | Consider first | What Nulang changes | Important trade-off |
| --- | --- | --- | --- |
| Proven fault-tolerant messaging and supervision | **Erlang / Elixir** | Adds inferred static types, capability analysis, and algebraic effects to an actor-oriented language | BEAM has much more deployment experience, libraries, and operational tooling |
| Predictable native systems performance and low-level memory control | **Rust** | Makes supervised actors and optional persistent state first-class programming concepts | Rust has a mature ecosystem and stronger evidence for production low-level workloads |
| Simple services with fast onboarding | **Go** | Provides algebraic effects, actor identities, and supervision rather than assembling conventions with libraries | Go offers a much more mature toolchain and ecosystem; it also has generics |
| Web applications and established AI integrations | **TypeScript / Python** | Experiments with language-level effects and optional AI-runtime patterns | TypeScript and Python have far broader web, data, model, and SDK support |

### Erlang / Elixir

The strongest reason to prefer Nulang is an interest in **static, inferred type-and-effect checking alongside actor primitives**. Erlang/OTP remains the more defensible choice for production supervision, distribution, observability, and decades of operational precedent. Nulang is not a drop-in BEAM replacement.

### Rust

Choose Rust when native performance, memory layout, platform integration, or mature libraries are the primary requirement. Nulang experiments with higher-level actor and recovery semantics that would otherwise be built from Rust crates or services. No general-purpose performance advantage is established by the language design alone.

### Go

Go already supports generics, concurrency primitives, and a substantial production ecosystem. Nulang is worth exploring when typed effects, actor isolation, or supervisor-managed state matter more than Go's simplicity and deployment maturity.

### TypeScript / Python

Stay with TypeScript for mainstream browser apps and with Python or TypeScript when you depend on broad AI frameworks and SDKs. Nulang's AI providers, pipelines, memory, and multi-agent primitives are **experimental and optional**. They do not eliminate the need for model evaluation, credentials management, observability, and durable infrastructure.

## When to experiment with Nulang

- You are researching type-and-effect systems or runtime architecture.
- You want to prototype supervised actor workloads and compare semantics against OTP or a Rust/Go service.
- You can test persistence, recovery, and operational behavior without placing critical production data at risk.
- You are comfortable with an evolving language, limited packages, and building from source.

## When to choose something else

- **Revenue-critical production service:** use a proven runtime unless you have explicitly validated the failure, security, deployment, and recovery behavior you need.
- **Hard multi-node availability guarantees:** prefer a mature distributed platform; Nulang's clustering and transport are still experimental.
- **Conventional website or SPA:** TypeScript and established web frameworks are substantially more practical.
- **Quick scripting or broad third-party API access:** Python, Go, or TypeScript usually reduce delivery time.
- **Lowest possible latency:** benchmark a representative workload before considering a migration.

## A low-risk way to evaluate it

1. [Try the browser playground](/playground/) for pure Core functions and effects. It does **not** run native actors, networking, JIT, or FFI.
2. [Build the native release binary](/getting-started/installation/) and run the [guided tutorial](/tutorial/).
3. [Exercise actor supervision](/actors/supervision/) and [distribution](/actors/distribution/) separately; do not infer network guarantees from local tests.
4. Read the [durability contract](https://github.com/nulang-org/nulang/blob/main/docs/DURABILITY_GUARANTEES.md) and write crash/restart tests against your selected store.
5. Inspect the [conformance cases](https://github.com/nulang-org/nulang/tree/main/conformance) and [benchmark sources](https://github.com/nulang-org/nulang/tree/main/benchmarks) rather than relying on performance claims.

Nulang is [open source under Apache 2.0](https://github.com/nulang-org/nulang/blob/main/LICENSE). Feedback, reproducible cases, and narrow benchmark comparisons are especially useful at this stage.
