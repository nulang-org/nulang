# Nulang 0.1.0: A distributed, actor-based language with algebraic effects

We're launching Nulang — a new open-source programming language that makes
distributed, fault-tolerant systems feel like ordinary application code.

## What is Nulang?

Nulang is an actor-based language with algebraic effects, Hindley-Milner type
inference, and capability-based types. Think Erlang's fault-tolerant actors
meeting Rust's compile-time safety, with a type system that understands
side-effects and resource ownership. Actors, effects, capabilities, and
distribution are all first-class — no frameworks, no ceremony.

## Why?

Distributed systems are hard because our languages weren't designed for them.
We bolt on frameworks, message queues, and retry logic — each making the
actual program logic harder to see. Nulang inverts this: the language *is* the
distributed runtime. `spawn` creates an actor. `send` delivers a message
across the network. `perform FS.read` performs a filesystem effect that the
type system tracks. The compiler checks that your effects are handled, your
messages are sendable, and your capabilities are sound — all at compile time.

## Quick example

```nulang
actor Greeter {
    behavior greet(name: String) {
        perform IO.print("Hello, " + name + "!")
    }
}

spawn Greeter {}
send Greeter.greet("World")
```

The type system infers everything. No `impl`, no `trait`, no
`#[derive(Serialize)]` — just write what you mean and the compiler figures
out the rest.

## Key features

- **Hindley-Milner type inference** with row-polymorphic records and variants
- **Algebraic effects** — `perform`/`handle` with resume semantics; effect rows
  in function signatures catch missing handlers at compile time
- **Actor model** — `spawn`, `send`/`!`, `ask`, selective `receive` with
  `after` timeouts, links, monitors, supervision trees
- **Capability-based types** — `iso`/`trn`/`ref`/`val`/`box`/`tag`/`lineariso`
  guarantee memory safety and data-race freedom without a borrow checker
- **Durable entities** — `entity` declarations with event sourcing, versioned
  migrations, and persistence that survives restarts
- **Package manager** — `nula new/build/run/test/add/remove`; dependencies
  with lockfiles, templates for CLI/lib/full projects
- **JIT compiler** — Cranelift-based tiered JIT with register-based bytecode VM
- **WASM backend** — compile Nulang to WebAssembly (experimental)
- **LSP server** — diagnostics, hover, goto-def, references, rename, completion
- **AI runtime** — `agent` declarations with LLM providers, pipelines, debates,
  memory subsystems (experimental, feature-gated)

## Getting started

```bash
git clone https://github.com/nulang-org/nulang.git
cd nulang
cargo build --release
./target/release/nulang --eval 'perform IO.print("Hello, Nulang!")'
# Or scaffold a new project:
./target/release/nulang nula new myapp
cd myapp && ../target/release/nulang nula run
```

Requires Rust 1.93+, Linux or macOS.

## Stability

Nulang is alpha software. The language version is `1.0.0-frozen`: the bytecode
format, wire protocol, and Nulang Core are frozen and will never break. The HM
type system, effect system, capability lattice, and actor surface are Stable
(breaking changes require an RFC and a deprecation cycle). Everything else is
Experimental. See `GOVERNANCE.md` for the full stability contract.

## What's next?

The roadmap (RFC 0003) targets a self-hosting bootstrap compiler, content-
addressed code deployment, a formal conformance suite, and Windows support.
The language was designed for a 200-year relevance horizon; the features
landing now are the ones that need to be right the first time.

## Links

- [GitHub](https://github.com/nulang-org/nulang)
- [Website](https://nulang.org)
- [Getting Started](https://github.com/nulang-org/nulang/blob/main/docs/GETTING_STARTED.md)
- [Tutorial](https://github.com/nulang-org/nulang/blob/main/docs/TUTORIAL.md)
- [Specification](https://github.com/nulang-org/nulang/blob/main/SPEC2.md)
