---
title: Use Cases
description: Discover where Nulang's typed actors, fault tolerance, and durable state fit best, with honest maturity guidance for AI, workflows, and distribution.
---

Nulang is designed for **stateful software that must continue making progress when individual components fail**. Its distinguishing combination is [actors and supervision](/actors/overview/), [static types and algebraic effects](/language/effects/), and opt-in persistent state. That combination is most valuable when failure recovery and coordination dominate the design—not for every app.

> **Before you adopt it:** Nulang is **alpha software**, not a production-proven platform. “Stable” below refers to language and actor API maturity; it does **not** imply a production SLA. Persistent actor recovery is implemented, while workflow syntax, AI integrations, HTTP services, and multi-node distribution have experimental boundaries. Test crash recovery and operational requirements against the exact runtime and backend you intend to use.

## The best fits, ranked

Ranked by **architectural fit**, not by benchmark results or production readiness.

| Use case | Why Nulang fits | Maturity |
| --- | --- | --- |
| **1. Fault-tolerant stateful services** | Isolated actors, message passing, and supervisor restarts | **Strongest current fit:** stable actor surface |
| **2. Durable business processes** | Persistent actors, checkpoints, events, and compensation patterns | **Promising:** persistence implemented; workflow syntax experimental |
| **3. Long-running AI agents** | Actor lifecycles, typed effect boundaries, tool use, and coordination | **Experimental:** optional AI runtime |
| **4. Event-driven and real-time backends** | Many independent state machines communicating by messages | **Mixed:** actors stable; web service surface experimental |
| **5. Distributed coordination and edge workloads** | Actor placement, distribution, and replicated-state patterns | **Experimental:** multi-node and source-level CRDT integration |

## 1. Fault-tolerant stateful services

**Examples:** session or connection coordinators, task supervisors, device gateways, game room state, and services with many independently failing jobs.

**Why it fits:** Model each unit of work as an actor with private state and a mailbox. Links, monitors, and supervision trees let you isolate a failure and restart the affected part instead of rebuilding failure handling around shared mutable state. Static types and effect tracking can expose mistakes before runtime.

**Start here:** Build a single-node service using the [actor model](/actors/overview/) and [supervision trees](/actors/supervision/). Add [persistent state](/getting-started/quick-start/) only to actors whose state must survive a process restart. Test message ordering, restart behavior, and replay under fault injection.

## 2. Durable business processes and background jobs

**Examples:** order fulfillment, approvals, onboarding, scheduled automations, data ingestion, and multi-step operations that wait on external systems.

**Why it fits:** Persistent actors can checkpoint state and hydrate it on restart. The reference runtime also implements workflow steps, signals, timers, journaling, and saga compensation. These primitives address the hard part of background processing: recovering progress after a crash rather than starting every job from zero.

**Maturity boundary:** The `workflow` declaration is experimental. The reference bytecode/runtime path implements workflow recovery, but the portable/cloud WASM backend currently **rejects workflow declarations** until durability parity is established. A successful local workflow is not evidence of cloud deployment compatibility. External effects still require idempotency, retry design, and integration tests.

**Explore:** [Durable workflows](/workflows/overview/) and [signals, timers, and queries](/workflows/signals-timers/).

## 3. Long-running AI agents and multi-agent coordination

**Examples:** research agents that hand work to specialists, tool-using support automations, evaluation pipelines, and human-in-the-loop agent processes.

**Why it fits:** An agent naturally has a lifecycle, state, messages, failure boundaries, and side effects. Nulang's actor model provides a foundation for coordination, while its optional AI runtime explores provider calls, tools, memory, pipelines, and supervisor teams. Typed effects can help make external interactions explicit.

**Maturity boundary:** AI support is **experimental**, provider integrations are limited, and recovery guarantees for an actor do not imply exactly-once LLM or tool execution. Prototype first; record tool results, enforce permissions, and budget for retries and observability.

**Explore:** [AI agents](/ai/overview/), [memory](/ai/memory/), and [multi-agent patterns](/ai/multi-agent/).

## 4. Event-driven services and real-time coordination

**Examples:** collaboration sessions, notification dispatchers, event processors, multiplayer lobbies, and streaming control planes.

**Why it fits:** Event-driven systems often consist of thousands of small state machines waiting for messages. Actors make the ownership of each state machine explicit, and supervisors give each one a recovery boundary. This is a good fit when coordination is harder than the underlying HTTP or database CRUD.

**Maturity boundary:** The actor surface is stable, but Nulang's [HTTP and web service tooling](/web/overview/) remains experimental. Validate backpressure, throughput, delivery semantics, and integration support against a Go, Elixir, or TypeScript implementation before committing a production backend.

## 5. Distributed systems and edge coordination

**Examples:** worker clusters, device fleets, distributed schedulers, and services that need to reconcile state across nodes.

**Why it fits:** Actors, monitors, process groups, and CRDT concepts provide useful building blocks for failure-prone networks. They make location, identity, state ownership, and supervision first-class architectural questions.

**Maturity boundary:** Multi-node transport and gossip membership are **experimental**, and source-level CRDT integration is incomplete. Treat this as a research or controlled-pilot use case—not as a claim of production-ready consensus, exactly-once delivery, or network partition tolerance.

**Explore:** [Distribution and clustering](/actors/distribution/) and [running a cluster](/actors/cluster-ops/).

## When Nulang is *not* the best choice

- **Simple CRUD APIs, marketing sites, or SPAs:** TypeScript, Go, and established web frameworks have faster development paths and broader integrations.
- **Low-level systems code or tight numerical kernels:** Rust or C/C++ offer a more mature ecosystem and more predictable optimization options.
- **Production-critical deployments that cannot tolerate alpha risk:** Elixir/Erlang, Go plus established workflow tooling, or another mature stack is the lower-risk choice today.
- **Workloads requiring proven cloud workflow or multi-node semantics:** Verify backend parity and fault recovery first; do not assume experimental features meet production guarantees.

## A practical adoption path

1. **Pick one stateful component, not a full rewrite.** Choose something with a clear mailbox, restart boundary, and measurable success criteria.
2. **Build locally.** Follow the [quick start](/getting-started/quick-start/) and [tutorial](/tutorial/); inspect [the examples on GitHub](https://github.com/nulang-org/nulang/tree/main/examples).
3. **Prove the failure behavior.** Kill actors and the process; check recovery, duplicate external side effects, state integrity, and observability.
4. **Expand only if the evidence supports it.** Compare implementation complexity, reliability, and measured performance with your existing stack.

**Want to test the language before installing it?** [Try the browser playground](/playground/), then read [installation instructions](/getting-started/installation/) for the full native runtime.
