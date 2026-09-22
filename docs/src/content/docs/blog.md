---
title: Why Nulang?
description: The benefits of Nulang compared to other languages and the goal behind the project.
---

## The Goal: A Language for Software That Survives

Nulang is a **durable computation language**. Its core purpose is to let you describe software that keeps running across crashes, restarts, node migrations, and decades of change. The unit of thought is an **entity**: a named identity that carries state, responds to messages, evolves over time, and persists by default.

The goal is not to compete with every programming language. Nulang explores a particular combination of actors, algebraic effects, static typing, reference capabilities, and durable state in one runtime.

---

## Nulang vs Erlang/Elixir

Both languages share the actor model, supervision trees, and "let it crash" philosophy. The differences:

| | Nulang | Erlang/Elixir |
|---|---|---|
| **Type system** | Static, HM-inferred, row-polymorphic | Dynamic (Erlang) / Gradual (Elixir) |
| **Effects** | Algebraic effects, compile-time checked | No effect tracking |
| **Execution** | Semantic-reference bytecode + tiered Cranelift JIT; experimental WASM and secondary native AOT | BEAM VM + JIT, garbage-collected |
| **Memory model** | Per-actor heaps, ORCA GC | Per-process heaps with BEAM-managed garbage collection |
| **AI library** | Optional nulang-ai library with memory | Library-level (Nx, Bumblebee) |

**Takeaway**: Nulang is aimed at developers who want Erlang-style fault-tolerance concepts with static typing, capabilities, effects, and durable state. The BEAM remains far more mature operationally, and Nulang does not currently claim a universal performance advantage.

---

## Nulang vs Rust

Rust and Nulang share a focus on safety and performance, but their domains differ:

| | Nulang | Rust |
|---|---|---|
| **Concurrency model** | Actors + messages | async/await, channels, Arc&lt;Mutex&lt;T&gt;&gt; |
| **Distribution** | Built-in clustering, CRDTs | Manual (gRPC, custom protocols) |
| **Fault tolerance** | Supervision trees, cascading restart | Manual error handling, panic=abort |
| **Durable orchestration** | Durable actor/entity primitives; `workflow` is Experimental ergonomic sugar under RFC 0017 | External workflow engines/libraries such as Temporal |
| **Type safety** | HM inference + capabilities | Ownership + borrows + lifetimes |

**Takeaway**: Rust gives you fine-grained memory control. Nulang gives you fault-tolerant distribution out of the box. Use Rust for systems programming; use Nulang for distributed applications.

---

## Nulang vs Go

Go's strength is simplicity. Nulang's strength is correctness under failure:

| | Nulang | Go |
|---|---|---|
| **Concurrency** | Actors with supervision | Goroutines + channels |
| **Error handling** | Pattern matching, supervision | `if err != nil` |
| **Type system** | HM inference, row polymorphism, ADTs | Static structural typing with interfaces and generics |
| **Effects** | Compile-time effect tracking | No effect system |
| **Distribution** | Built into the language | Library-level |

**Takeaway**: Go is great for simple networked services. When those services become distributed systems with complex failure modes, Nulang's supervision, effects, and durable state reduce the operational burden.

---

## Nulang vs Python/TypeScript (for AI)

The AI ecosystem has converged on Python and TypeScript, but both languages were designed before LLMs existed:

| | Nulang | Python/TypeScript |
|---|---|---|
| **AI integration** | Optional `nulang-ai` runtime/library; `agent` is Experimental ergonomic sugar under RFC 0017 | Large library ecosystems and SDKs |
| **Memory** | Episodic, semantic, and procedural memory support | Commonly library/vector-store based |
| **Multi-agent** | Pipeline, debate, and supervisor abstractions in `nulang-ai` | Framework-dependent orchestration |
| **Effects** | AI calls can participate in the language effect model | Side effects are library/runtime conventions |
| **Persistence** | Can compose AI work with durable actors/entities | Commonly external databases/workflow engines |

**Takeaway**: Python and TypeScript currently have much larger AI ecosystems. Nulang's design goal is to compose AI libraries with the same actor, effect, capability, and durability primitives used by the rest of an application rather than making AI syntax the language's foundation.

---

## The Bet: Primitives Over Frameworks

Every decade brings new AI models, new cloud providers, and new orchestration frameworks. The Nulang bet is that a small set of primitives — actors, effects, capabilities, state, identity, messages — will outlast all of them.

- **Actors** were meaningful in 1973 (Hewitt et al.) and will be meaningful in 2073.
- **Algebraic effects** generalize exceptions, async/await, generators, and state — all in one mechanism.
- **Reference capabilities** provide compile-time aliasing/sendability constraints without Rust-style borrowing; the runtime still uses ORCA garbage collection.
- **Durable state** gives actors/entities runtime-managed persistence and recovery mechanisms across failures, subject to the configured store and recovery semantics.

Nulang keeps a small portability/core layer and evolves higher-level surfaces separately. Under [RFC 0021](https://github.com/nulang-org/nulang/blob/main/RFC/0021-compatibility-before-freeze.md), pre-adoption source semantics are currently Stable rather than permanently Frozen; published artifact/wire/ABI versions remain explicit compatibility obligations.

---

## When to Use Nulang

- You're building a system where **durable state and recovery semantics** should be part of the runtime model rather than entirely application-managed.
- You need **fault tolerance** but don't want to learn OTP from scratch.
- You want **static types** that catch bugs before they reach production.
- You're building **AI agents** that need memory, tool use, and multi-agent coordination.
- You want to **start local** and deploy to the cloud without rewriting.

## When Not to Use Nulang

- You need a mature ecosystem with thousands of libraries. (Nulang is alpha.)
- You're building a CLI tool or a simple script. (Use Rust, Go, or Python.)
- You need Web/React/SPA frontend support. (Use TypeScript.)
- You're under a tight deadline with no tolerance for alpha software.

---

## Getting Started

[Install Nulang](/getting-started/installation/) and follow the [Quick Start](/getting-started/quick-start/) guide to write your first actor.

The [source code is on GitHub](https://github.com/nulang-org/nulang) under the Apache 2.0 license.
