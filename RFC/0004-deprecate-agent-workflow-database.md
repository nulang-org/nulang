# RFC 0004: Deprecate `agent`, `workflow`, and `database` as Language Keywords

- **Status:** Historical draft — not accepted; architectural direction superseded by RFC 0024
- **Tier:** Stable
- **Author:** AI assistant review
- **Created:** 2026-07-21
- **Resolved:** (pending)
- **Language-version at effect:** 1.0.0-frozen
- **Supersedes:** none
- **Superseded by:** RFC 0024 (architectural direction)

## Summary

> **Historical note (2026-09-25):** This proposal was never accepted and is not current migration guidance. RFC 0024 established the current orthogonal execution model: local computation, scoped tasks, and actors are distinct execution forms, while durability and identity compose with them. Current `workflow` support remains an Experimental first-class ergonomic surface while its semantics are stabilized; `agent` is increasingly treated as composition over ordinary runtime primitives. Consult `SPEC2.md`, `docs/IMPLEMENTATION_STATUS.md`, and accepted RFCs for current behavior.

Move the `agent`, `workflow`, and `database` top-level declarations out of the Stable language surface and reclassify them as Experimental Cloud SDK / library concerns. The declarations remain functional for the deprecation cycle (≥2 major language versions) but emit warnings. The breaking phase removes the AST nodes, parser productions, typechecker cases, and bytecode opcodes, replacing them with ordinary actor declarations that import `nlc.ai`, `nlc.workflow`, and `nlc.storage` packages.

## Motivation

Nulang is repositioning as a durable computation language for long-lived, distributed, stateful software entities. AI agents, durable workflows, and database schemas are important applications of durable entities, but they are not timeless language primitives. Today's LLM providers, workflow patterns, and database APIs will change repeatedly over a 50–100+ year horizon. Baking them into the language surface (`Decl::Agent`, `Decl::Workflow`, `Decl::Database`, a dedicated `LlmAsk` opcode) creates a coupling that the Frozen/Stable tiers cannot afford.

The repository already shows the right replacement path:

- `actor` and `persistent actor` are the Stable concurrency/durability primitives (`src/ast.rs`, `src/runtime/`).
- `Provider.ask("llm", prompt)` replaces `perform LLM.ask(prompt)` as the general, non-transient effect vocabulary (RFC 0003, Item 5; `src/mir_lower.rs`, `src/runtime/mod.rs`).
- Cloud SDK crates (`nlc.ai`, `nlc.workflow`, `nlc.storage`) are the natural homes for the former language-level features.

Current evidence of the problem:

- `src/ast.rs` contains `Decl::Agent`, `Decl::Workflow`, `Decl::Database`, and agent-specific configuration structs (model, system prompt, memory, pricing, retry, fallback).
- `src/bytecode.rs` reserves opcode `0x9C` for `LlmAsk`.
- `src/stdlib.rs` registers `LLM.ask` as a built-in effect.
- `src/typechecker.rs` and `src/effect_checker.rs` have special cases for agent and workflow declarations.
- `src/mir_lower.rs` returns `not yet implemented` for `hir::Decl::Workflow` and `hir::Decl::Agent`, indicating these language features are not yet on the MIR-exclusive path.

## Design

### Non-breaking phase (this RFC)

1. **Reclassify `agent`, `workflow`, `database` to Experimental tier.** They become language-level features with no stability promise; new code should use Cloud SDK libraries.
2. **Emit deprecation warnings.** When the parser or effect checker encounters `Decl::Agent`, `Decl::Workflow`, or `Decl::Database`, it records a warning. The warning is printed by the CLI and surfaced by the LSP diagnostics provider.
3. **Document migration path.** Provide source examples showing how to rewrite the deprecated declarations as ordinary actors that import Cloud SDK packages.
4. **Update `CHANGELOG.md`** under the Stable tier to record the deprecation and the planned removal version.
5. **Update `SPEC2.md`** Chapter 7 (Declarations) and the language tour examples to note that `agent`, `workflow`, and `database` are deprecated and will become library constructs.

### Breaking phase (separate RFC, ≥2 major versions later)

1. Remove `Decl::Agent`, `Decl::Workflow`, `Decl::Database`, and `Decl::StateMachine` from `src/ast.rs`.
2. Remove the corresponding parser productions in `src/parser.rs`.
3. Remove the corresponding typechecker and effect-checker cases.
4. Remove `OpCode::LlmAsk` and `Effect::LLM` per RFC 0003, Item 5 (breaking phase).
5. Remove `PipelineNew`, `PipelineStage`, `PipelineRun`, and agent-orchestration opcodes if they remain.
6. Provide a source-to-source migration tool (or LSP code action) that rewrites:
   - `agent Name = { ... }` → `actor Name { ... }` with an `nlc.ai` import and effect wiring.
   - `workflow Name { ... }` → `actor Name { ... }` with `nlc.workflow` step/effect wiring.
   - `database Name { ... }` → `actor Name { ... }` with `nlc.storage` schema wiring.
7. Bump the language major version and provide a bytecode migration if needed.

### Migration examples

Before (deprecated):

```nulang
agent Assistant = {
    model: "gpt-4o",
    system_prompt: "You are helpful.",
    memory: { max_turns: 10 }
}
let a = spawn Assistant {}
```

After (target):

```nulang
import nlc.ai

actor Assistant {
    state durable config: { model: String, system_prompt: String, max_turns: Int } = {
        model: "gpt-4o",
        system_prompt: "You are helpful.",
        max_turns: 10
    }

    behavior ask(question: String) ! {Provider} {
        perform Provider.ask("llm", question)
    }
}

let a = spawn Assistant {}
```

Before (deprecated):

```nulang
workflow OrderSaga {
    step reserve { /* ... */ }
    step charge { /* ... */ }
    compensate { /* ... */ }
}
```

After (target):

```nulang
import nlc.workflow

actor OrderSaga {
    behavior run() ! {Workflow, Storage} {
        perform Workflow.step("reserve")
        perform Workflow.step("charge")
    }
}
```

## Tier Classification

- **Stable tier affected:** Yes. `agent`, `workflow`, and `database` declarations are currently treated as Stable or implemented language surface.
- **New tier:** Experimental.
- **Deprecation cycle:** The declarations remain functional for at least two major language versions after this RFC is accepted and implemented. They emit warnings in version 1.x, remain functional through version 2.x, and are removed in version 3.0 by a separate breaking-phase RFC.
- **Language version:** 1.0.0-frozen marks the deprecation; removal requires a major-version bump.

## Backwards Compatibility

Existing programs using `agent`, `workflow`, and `database` continue to compile and run during the deprecation cycle. The compiler emits a warning pointing to this RFC and to the Cloud SDK migration examples. The breaking-phase RFC will provide:

- A source-to-source rewrite tool or LSP code action.
- Updated bytecode migration in `src/format/migrate.rs` if the removed opcodes were ever emitted (they are currently not on the MIR path for `agent`/`workflow`, but `LlmAsk` is).

## Alternatives Considered

1. **Keep `agent` and `workflow` as Stable sugar over actors.** Rejected because even syntactic sugar in the Stable tier creates a long-term maintenance burden and sends a market signal that Nulang is an "AI language" rather than a durable computation language.
2. **Move only `agent` and keep `workflow`/`database`.** Rejected because the same longevity argument applies to all three: workflows and database schemas are no more timeless than AI model APIs.
3. **Remove immediately without deprecation cycle.** Rejected because it would break existing alpha users and violates GOVERNANCE.md §6.
4. **Promote to Frozen Core.** Rejected because the Frozen Core must remain small and sequential; actors, effects, and durability are already Stable, not Core, and AI/workflow/DB are clearly higher-layer concerns.

## Open Questions

1. Should the Cloud SDK packages (`nlc.ai`, `nlc.workflow`, `nlc.storage`) be created in the language repo as `examples/` or in the `nulang-cloud` repo? The PRD places them in the Cloud SDK, which may live in either repo.
2. Is `StateMachine` also a candidate for deprecation, or is it a general enough construct to remain as Stable actor sugar?
3. Should the migration tool be a CLI subcommand (`nulang migrate`) or an LSP code action? Both are acceptable; the RFC does not decide.

## Resolution

(To be filled on accept/reject.)
