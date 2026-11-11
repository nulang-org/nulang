# Nulang exact-revision performance envelope

This is a **measurement tool**, not a claim that the compiler or actor runtime
has already become faster. Run on an otherwise idle **dedicated Linux host**.

```sh
python3 -m unittest discover -s scripts/tests -p 'test_performance_envelope.py'
python3 scripts/performance_envelope.py --output performance-envelope.json \
  --repeats 5 --warmup 1 --actors 100000 --allocations 10000 \
  --cpu-mode single
```

The script refuses tracked dirty trees unless `--allow-dirty` is given. Untracked
files do not affect the executable's source revision, but do inspect generated
files/configuration before publishing. It records the full Git commit, fixture
SHA-256, Rust version, platform, CPU model, CPU affinity, Cargo release build
profile and features, raw repetitions, medians, and process-specific Linux
`wait4` peak RSS. Cargo builds **finish before** measurement begins.

| Metric | Timed boundary | Important exclusion |
|---|---|---|
| Bytecode compilation | CLI process start → `.nbc` emission | Does not isolate pure frontend CPU; includes CLI startup and disk write |
| Native compilation + run | CLI process start → native program completes | **Not** compiler-only: includes compiled execution |
| Interpreter | Existing `nulang-jit-bench` `interpreter_run_ns` | Source compilation and VM setup excluded |
| First JIT | First `VM.run()` including tier-up compilation | Source compilation excluded |
| Warm JIT | Second `VM.run()` on same VM | Source compilation and warm-up excluded |
| JIT codegen | Compiler requests measured by `JitBackend` | Region planning/warm-up excluded |
| 100k idle actors | Existing `actor_density` Linux RSS spawn delta | **Not** process peak RSS or a live service memory estimate |
| ORCA size distribution | Synthetic separate heaps with 8/16/32/64/128/256-byte objects | **Not** production allocation distribution or total process RSS |

The ORCA experiment deliberately reports physical class occupancy from actual
successful allocations, including headers and class rounding. It keeps all
objects live. It does not extrapolate improvements from changing header size to
system-wide RSS savings; changing size classes requires an exact-base A/B study.

The report's `publishable` flag only indicates a clean tracked checkout and
single-CPU affinity. It does **not** establish an isolated physical host,
semantic parity across AOT/JIT for arbitrary programs, or statistical confidence.
Shared GitHub Actions runners are diagnostic; controlled-runner provenance should
accompany any external performance claim. For before/after performance change
claims use `scripts/nulang_ab_bench.py` or `scripts/actor_density_ab_bench.py`
with an exact base SHA and paired measurements instead of comparing independent
envelope reports from different hosts.

Limitations: AOT has a restricted semantic surface; this fixture uses only numeric
operations. The allocation profile is intentionally a synthetic memory accounting
probe, not a heap profiler. True production allocation histograms require sampling
allocation sites/types under representative workloads and attributing allocator,
metadata and GC/retained memory separately.
