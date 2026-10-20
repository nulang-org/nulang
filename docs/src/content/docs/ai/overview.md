---
title: AI Agents Overview
description: Build LLM-powered agents using the nulang-ai library — spawning, asking, and tool binding.
---

## AI Agents
Nulang's AI capabilities live in the optional nulang-ai library crate. An `agent` is a named record of configuration — model, system prompt, tools, memory, pricing — that the runtime spawns like an actor through the generic PerformAsync effect mechanism. No special AI bytecodes or language extensions are needed. You interact with an agent through the `ask` operator, which is a synchronous request/reply call.

## Declaring an Agent

```nulang
agent Assistant = {
    model: "gpt-4o",
    system_prompt: "You are a helpful assistant.",
    memory: { max_turns: 10 }
}
```

The full set of agent configuration fields:

| Field | Type | Description |
|-------|------|-------------|
| `model` | `String` | LLM model identifier (e.g. `"gpt-4o"`, `"llama3.1"`) |
| `system_prompt` | `String` | System prompt prepended to every conversation |
| `tools` | `[String]` | List of function names exposed as tools (see [Tools](#tools)) |
| `memory` | `{ max_turns: Int }` | Episodic memory — conversation history window |
| `semantic_memory` | `{ dimensions: Int }` | Vector embeddings for fact recall |
| `procedural_memory` | `{ namespace: String }` | Learned patterns/skills |
| `pricing` | `{ input: Float, output: Float }` | Per-token pricing for cost tracking |
| `fallback` | `[{ model: String, ... }]` | Fallback models on failure |
| `retry` | `{ max_attempts: Int, ... }` | Retry configuration |

All fields except `model` and `system_prompt` are optional.

## Spawning and Asking

Spawn an agent like an actor, then call it with `ask`:

```nulang
agent Assistant = {
    model: "gpt-4o",
    system_prompt: "You are helpful.",
    memory: { max_turns: 10 }
}

let a = spawn Assistant {} in
ask a ask("What is an actor model?")
```

`spawn Assistant {} in ...` creates a running agent instance and returns its reference. The `ask a ask("prompt")` form is a synchronous request/reply — it blocks the caller until the agent responds. Inside a scheduler-driven actor or workflow, `perform Inference.ask(prompt)` suspends non-blockingly instead (see [Signals, Timers & Queries](/workflows/signals-timers/)).

## Tools

Expose Nulang functions as agent tools with the `@tool` annotation:

```nulang
@tool(description: "Adds two integers.")
fn add(x: Int, y: Int) -> Int { x + y }

agent Calculator = {
    model: "gpt-4o",
    system_prompt: "You are a calculator.",
    tools: [add]
}

let calc = spawn Calculator {} in
ask calc ask("What is 2 + 2?")
```

The `@tool(description: "...")` annotation attaches a human-readable description. The agent's LLM can invoke the tool during its response; the runtime executes the Nulang function and feeds the result back.

## Providers

Nulang's LLM client is provider-agnostic. The `model` field selects the provider:

| Provider | Example model | Configuration |
|----------|---------------|---------------|
| OpenAI | `gpt-4o` | `OPENAI_API_KEY` env var |
| Ollama | `llama3.1` | Local Ollama server on `localhost:11434` |

Pipeline orchestration is available via the Rust `nulang-ai` crate (`Pipeline::new()`, `Pipeline::stage()`, `Pipeline::run()`) and can be accessed through the runtime API. A language-level pipeline expression is pending.

## Local Agent Execution Safety

The `nulang agent run` CLI currently initializes a local orchestration scaffold.
Its default `LocalWorker` does **not** run shell commands, modify files, or
execute acceptance checks. It reports the task as blocked instead of claiming
that work was completed. A blocked goal returns a non-success CLI exit.

Hosts embedding `nulang-ai-local` can use `LocalRuntime::open_with_execution`
to provide a `Worker` and an independent `EvidenceVerifier`. A worker returns
`WorkerExecution::Executed` with acceptance-criterion artifact references.
The local runtime checks that every required criterion has a nonempty
reference and requires the host verifier to validate the real artifacts.
Only then does it atomically persist the evidence and promote the task to
`Completed` in SQLite. Unconfigured or rejected verification blocks completion.

This is a **verification boundary**, not a shipped sandbox or a trusted
test runner. The host is responsible for actual execution, sandboxing,
artifact verification, and minimizing tool permissions. NLAP now emits
`task_verified`, `task_blocked`, `task_failed`, and `goal_blocked`
events; existing `task_completed` and `goal_completed` events are
emitted only after successful verification.

## Opt-in Container Test Executor

The `nulang-ai-worker::sandbox` module provides an opt-in, local test runner.
It does not change the default `nulang agent run` behavior: without a host
executor and verifier, the default agent remains blocked.

A trusted application host supplies:

1. A locally cached OCI image **pinned by SHA-256 digest**.
2. A fixed command as an argument vector, such as `["cargo", "test", "--workspace"]`.
3. An existing workspace and a separate private receipt directory.
4. A task manager that creates **test-only** tasks with `required_capabilities:
   ["test"]` and exactly the configured acceptance criterion.

The host constructs a `SandboxConfig`, creates
`SandboxHarness::docker(config).into_parts()`, and passes the worker and
verifier into `LocalRuntime::open_with_manager` alongside its trusted
manager implementation. A host can instead use
`SandboxHarness::with_driver(config, Arc::new(DockerDriver::new("podman")))`
to select a compatible local container CLI.

The runner invokes the engine **without a shell**, with networking disabled,
all Linux capabilities dropped, a non-root UID/GID, a read-only root
filesystem, read-only source binding, private temporary storage, resource
limits and a timeout. It refuses test-only requests that require `code`,
`shell` or other capabilities. No model-supplied task text enters the
command or image arguments.

A successful exit creates a host-owned transcript and an ephemeral sealed
receipt bound to the exact task and criterion. The independent verifier
checks that receipt and its transcript digest before a task can be marked
completed. A missing or modified receipt blocks completion. Transcripts can
contain sensitive test output; the configured host is responsible for their
retention and access controls.

**Scope and limitations:** This is a Docker/Podman-based local development
runner, not a security boundary for mutually untrusted Cloud tenants.
A malicious container engine, privileged Docker daemon, kernel exploit, or
compromised host falls outside its threat model. Nulang Cloud should use
isolated workers or microVMs with independent identities and enforcement.
Receipts are held in memory until shutdown; the next step is a durable,
reconcilable execution journal and outbox. The host must also ensure a test
result genuinely satisfies the user's *goal*, not just a narrower task.

## Next

- [Memory](/ai/memory/) — episodic, semantic, and procedural memory subsystems
- [Multi-Agent Patterns](/ai/multi-agent/) — pipelines, debates, and supervisor teams

> **Note**: The `agent` keyword is currently Experimental and is proposed for deprecation in favor of plain `actor` declarations that import `nlc.ai` (RFC 0004). The keyword remains functional and will continue to work through at least two major language versions.
