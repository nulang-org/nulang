---
title: Why Nulang?
description: The benefits of Nulang compared to other languages and the goal behind the project.
---

## The Goal: A Language for Software That Survives

Nulang is a **durable computation language**. Its core purpose is to let you describe software that keeps running across crashes, restarts, node migrations, and decades of change. The unit of thought is an **entity**: a named identity that carries state, responds to messages, evolves over time, and persists by default.

The goal is not to compete with every programming language. It's to fill a gap: there is no language today that gives you actors, algebraic effects, static types, and durable state in one coherent system.

---

## Nulang vs Erlang/Elixir

Both languages share the actor model, supervision trees, and "let it crash" philosophy. The differences:

| | Nulang | Erlang/Elixir |
|---|---|---|
| **Type system** | Static, HM-inferred, row-polymorphic | Dynamic (Erlang) / Gradual (Elixir) |
| **Effects** | Algebraic effects, compile-time checked | No effect tracking |
| **Performance** | JIT + native AOT, zero-copy | BEAM VM, garbage-collected |
| **Memory model** | Per-actor heaps, ORCA GC | Shared heap, per-process GC |
| **AI library** | Optional nulang-ai library with memory | Library-level (Nx, Bumblebee) |

**Takeaway**: If you want Erlang's fault tolerance with static types that catch bugs at compile time and native performance, Nulang is designed for you.

---

## Nulang vs Rust

Rust and Nulang share a focus on safety and performance, but their domains differ:

| | Nulang | Rust |
|---|---|---|
| **Concurrency model** | Actors + messages | async/await, channels, Arc&lt;Mutex&lt;T&gt;&gt; |
| **Distribution** | Built-in clustering, CRDTs | Manual (gRPC, custom protocols) |
| **Fault tolerance** | Supervision trees, cascading restart | Manual error handling, panic=abort |
| **Workflows** | Built-in durable workflows | Temporal/Sidekiq libraries |
| **Type safety** | HM inference + capabilities | Ownership + borrows + lifetimes |

**Takeaway**: Rust gives you fine-grained memory control. Nulang gives you fault-tolerant distribution out of the box. Use Rust for systems programming; use Nulang for distributed applications.

---

## Nulang vs Go

Go's strength is simplicity. Nulang's strength is correctness under failure:

| | Nulang | Go |
|---|---|---|
| **Concurrency** | Actors with supervision | Goroutines + channels |
| **Error handling** | Pattern matching, supervision | `if err != nil` |
| **Type system** | HM inference, row polymorphism, ADTs | Structural types, no generics (pre-1.18) |
| **Effects** | Compile-time effect tracking | No effect system |
| **Distribution** | Built into the language | Library-level |

**Takeaway**: Go is great for simple networked services. When those services become distributed systems with complex failure modes, Nulang's supervision, effects, and durable state reduce the operational burden.

---

## Nulang vs Python/TypeScript (for AI)

The AI ecosystem has converged on Python and TypeScript, but both languages were designed before LLMs existed:

| | Nulang | Python/TypeScript |
|---|---|---|
| **Agent declaration** | Declarative `agent` keyword | Library objects (LangChain, etc.) |
| **Memory** | 3 built-in subsystems | Manual vector DB integration |
| **Multi-agent** | Pipelines, debates, supervisors | Custom orchestration code |
| **Determinism** | Type-checked effect isolation | No effect guarantees |
| **Persistence** | Built-in checkpointing, event sourcing | External databases |

**Takeaway**: Python and TypeScript have vast AI library ecosystems. Nulang gives you declarative primitives that eliminate boilerplate for the common patterns: define an agent, give it memory, compose agents into teams. No LangChain required.

---

## The Bet: Primitives Over Frameworks

Every decade brings new AI models, new cloud providers, and new orchestration frameworks. The Nulang bet is that a small set of primitives — actors, effects, capabilities, state, identity, messages — will outlast all of them.

- **Actors** were meaningful in 1973 (Hewitt et al.) and will be meaningful in 2073.
- **Algebraic effects** generalize exceptions, async/await, generators, and state — all in one mechanism.
- **Reference capabilities** prevent data races without a GC or borrow checker.
- **Durable state** means your program's execution survives the machine it runs on.

Nulang freezes these primitives in a [Frozen Core](https://github.com/nulang-org/nulang/blob/main/GOVERNANCE.md) and builds everything else — AI, cloud services, billing, multi-tenancy — as evolvable layers.

---

## When to Use Nulang

- You're building a system that **must not lose state** across restarts.
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
