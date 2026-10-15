---
title: Why Nulang? Typed Actors and Durable Execution
description: Why Nulang combines typed effects, actor supervision, persistent state, and experimental distribution in one language and runtime.
---


Nulang explores a particular software architecture: **typed actors, explicit effects, supervision and opt-in durable state** as parts of the same programming language and runtime. Instead of bolting failure recovery and actor state onto ordinary service code, the aim is to make their contracts inspectable and testable.

Nulang is currently **alpha software**. The reference bytecode runtime implements durable actor state and recovery, while distributed operation, AI features, WASM and alternate compilation backends are experimental. This is a research and adoption proposition, not proof of production superiority.

## The design choices

- **Actors and supervision.** Isolate state and failures, communicate through messages, and restart supervised work according to explicit policies.
- **Static types and algebraic effects.** Hindley–Milner-style inference, row-polymorphic effect tracking and capability checks make classes of operations visible to the compiler.
- **Persistence as an explicit choice.** Regular actors are in memory. Persistent actors can journal and recover state through configured stores; external effects still require real idempotency/replay contracts.
- **One runtime direction.** Fabric messaging, durable streams, workflows and NuDB research aim to reduce the number of independently operated components for certain applications.

## Is Nulang better than Erlang, Go or Rust?

Not categorically. **Erlang/Elixir** has a far more mature actor and supervision runtime; **Go** offers simple production services; **Rust** offers strong native control. Nulang's architectural proposition is the specific combination of actors, types, effects, capabilities and durable state. Its production maturity and ecosystem are far behind those established choices.

See the detailed [Nulang vs Erlang, Elixir, Go, Rust and Python comparison](/comparisons/).

## Is Nulang better than Akka, Orleans or Ray?

Each solves a different problem. **Akka** already provides persistent actors and cluster sharding. **Orleans** provides virtual actors and pluggable grain persistence. **Ray** excels at distributed Python/AI computation. Nulang emphasizes a cohesive language-level model, but it has not matched those projects' deployment maturity or all their semantics.

See [the actor framework comparison](/comparisons/#nulang-vs-actor-frameworks).

## Could one runtime replace Redis, NATS, Kafka or Temporal?

That is a **long-term goal, not current drop-in compatibility**. Fabric already has experimental topic routing and durable-stream components; the reference runtime has workflow/recovery primitives; NuDB is early database-engine work. Consumer guarantees, compatibility gateways, operational reliability and database functionality remain incomplete.

Read [how Nulang aims to build durable distributed applications with fewer services](/infrastructure/).

## When to try Nulang

Experiment when you want to prototype supervised actors, typed effects, or recovery semantics and can validate behavior through tests. For revenue-critical production workloads, mature languages and services remain the safer default.

Start with the [installation guide](/getting-started/installation/), [actor model](/actors/overview/) and [durable workflow docs](/workflows/overview/). Source and implementation status are available on [GitHub](https://github.com/nulang-org/nulang) under Apache 2.0.
