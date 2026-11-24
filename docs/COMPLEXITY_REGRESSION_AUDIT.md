# Complexity regression audit

Status: focused audit starter, not a comprehensive performance guarantee.

Inspired by Zed's `COMPLEXITY.md` discipline: record the complexity bound,
a reproducer that fails when the bound is accidentally violated, and the
measured evidence before claiming a performance improvement. Correctness
and deterministic execution take precedence over benchmark throughput.

## Tracked paths

| Path | Expected work | Guard | Limitation |
| --- | --- | --- | --- |
| `package::module_invalidation::invalidated_modules` | Graph construction and traversal over `V` modules and `E` imports, with ordered-map/set work typically `O((V + E) log V)` plus the cost of string comparisons | `long_dependency_chain_invalidates_iteratively_without_missing_dependents`, `layered_diamond_invalidates_each_module_once_despite_shared_paths` | Large fixtures establish correct iterative/unique traversal results; they do not prove asymptotic wall-clock time |
| `DurableEffectCoordinator::execute_compiled_async_with_deduplication` | One logical operation gets its own prepare/receipt record; replay must be idempotent with respect to provider dispatch and journal mutation | `repeated_compiled_effect_site_occurrences_have_distinct_replay_stable_receipts` | This tests the trusted Rust adapter using `MemoryStore`, not the live VM host callback or a process crash |

## CI validation

```sh
cargo test --locked --no-default-features --lib package::module_invalidation::tests
cargo test --locked --no-default-features --test compiled_durable_dispatch
bash scripts/test-durability-guarantees.sh
cargo fmt --all -- --check
```

The last two commands provide the broader durability and formatting gates.
The existing differential fuzzer, hosted CI, and real-KVM tests remain
independent requirements. No test here authorizes incremental compiler cache
reuse, Cloud admission, or a production durability claim.

## Next evidence-based steps

1. Establish controlled 1x, 2x, 4x and 8x dependency-graph size measurements
   on the same worker, keeping graph topology and compilation context fixed.
   Publish operations/counts or stable profiler evidence alongside wall time;
   do not gate shared-CI wall-clock ratios without noise controls.
2. Add source-compiled actor-turn and VM `PerformAsync` host dispatch
   conformance to the existing durable-effect matrix. Exercise process kill,
   restart, provider-commit/receipt-loss, epoch fencing, and provider
   idempotency boundaries against a real durable store.
3. Reuse canonical semantic IDs and compilation metadata for error
   explanations. Do not create a second parser or cloud-side semantics engine.
4. Promote incremental reuse only after clean/incremental diagnostics,
   type/effect analysis, MIR, artifact digests, and executable behavior match.

## Claim boundaries

These guards are narrow and deterministic. They are not proof that
`O((V + E) log V)` holds for every caller input or that arbitrary external
providers implement effectively-once behavior. Any optimization proposal must
state its own observed workload, baseline, and exact-head evidence.
