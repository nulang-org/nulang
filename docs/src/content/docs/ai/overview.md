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

## Next

- [Memory](/ai/memory/) — episodic, semantic, and procedural memory subsystems
- [Multi-Agent Patterns](/ai/multi-agent/) — pipelines, debates, and supervisor teams

> **Note**: The `agent` keyword is currently Experimental and is proposed for deprecation in favor of plain `actor` declarations that import `nlc.ai` (RFC 0004). The keyword remains functional and will continue to work through at least two major language versions.
