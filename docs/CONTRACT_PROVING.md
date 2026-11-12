# Optional contract proof diagnostics

Nulang can check a **small subset** of the existing `requires` and
`ensures` contracts without changing source syntax or compilation.

```sh
nulang prove example.nula
nulang prove example.nula --json
nulang prove example.nula --strict
# The nested package-manager CLI accepts the same operation:
nulang nula prove example.nula
```

Example:

```nulang
fn add_one(x: Int) -> Int
    requires x >= 0 && x <= 10
    ensures result >= 1
{
    x + 1
}
```

The analyzer first runs Nulang's regular frontend and type/effect/capability
checks. It can establish simple integer postconditions from conjunctions of
literal bounds, straight-line pure integer expressions, and safe 48-bit signed
interval arithmetic. It currently handles +, -, unary negation, and immutable
integer lets. Calls, effects, loops, branches, floats, and non-integer
parameters are intentionally reported as `unknown`, not certified.

Outcomes:

- `proved`: the postcondition holds **within the documented, restricted
  interval model**, and the analyzed operations stay in the modeled Int range
- `unknown`: analysis was inconclusive or the function is outside that model
- `counterexample`: an actually determined concrete input violates a
  postcondition; a witness is included (including an empty witness for a
  zero-argument function)

No proof is claimed about external host effects, FFI, memory safety, actor
scheduling, distributed consistency, or correctness of production backends.
In particular, these results are **not** a substitute for VM/JIT/WASM/AOT
parity and conformance testing.

**Developer experience and compatibility:**

- Existing `requires`/`ensures` source and runtime checks remain unchanged.
- Ordinary `nulang file.nula`, `nulang --check`, package builds, and tests
  never run this optional analyzer or require an SMT solver.
- `unknown` is advisory (exit 0); `counterexample` returns exit 1.
  `--strict` additionally returns exit 1 when results are `unknown`.
- Syntax/frontend errors and invalid command arguments return exit 2.
- Functions without `ensures` are not reported; `requires` only protects
  calls at runtime and is not itself a postcondition.
- JSON results use schema `nulang.contract-proof/v1` for tooling integration.

This first step intentionally uses **no new dependencies**. An optional SMT
backend can be introduced later behind the same command/report schema, with
solver timeouts always reported as `unknown`.
