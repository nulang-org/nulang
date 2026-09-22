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
  migrations, snapshot/journal recovery, and selectable durable stores
- **Package manager** — `nula new/build/run/test/add/remove`; dependencies
  with lockfiles, templates for CLI/lib/full projects
- **JIT compiler** — Cranelift-based tiered JIT with register-based bytecode VM
- **WASM backend** — compile Nulang to WebAssembly (experimental)
- **LSP server** — diagnostics, hover, goto-def, references, rename, completion
- **AI runtime** — optional LLM providers, pipelines, debates, and memory
  subsystems (experimental, feature-gated). Under accepted RFC 0017, `agent`
  remains Experimental ergonomic syntax that lowers to the canonical
  actor/effect runtime model

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

Source builds use Rust 1.95.0, pinned by `rust-toolchain.toml`. Tagged
release CI currently validates Linux x86_64/aarch64, macOS aarch64, and
Windows x86_64.

## Stability

Nulang is alpha software. Existing artifacts still carry the historical
`1.0.0-frozen` language metadata, but RFC 0021 no longer treats all
pre-adoption source semantics as permanently Frozen. Published compatibility
contracts remain versioned obligations; source semantics are Stable or
Experimental according to `GOVERNANCE.md`. Expect breaking source-language
changes before the external-adoption freeze gate closes.

## What's next?

The current priority is semantic stabilization rather than adding more
language surface: exact-head CI, authority/capability enforcement, durable
identity and migration correctness, replay-safe external effects, backend
conformance, and destructive recovery testing. See
`docs/SEMANTIC_STABILIZATION_CONTRACT.md` for the active gate.

## Links

- [GitHub](https://github.com/nulang-org/nulang)
- [Website](https://nulang.org)
- [Getting Started](https://github.com/nulang-org/nulang/blob/main/docs/GETTING_STARTED.md)
- [Tutorial](https://github.com/nulang-org/nulang/blob/main/docs/TUTORIAL.md)
- [Specification](https://github.com/nulang-org/nulang/blob/main/SPEC2.md)
