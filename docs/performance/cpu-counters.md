# Linux CPU-counter evidence (opt-in)

Use this tool to investigate *why* a controlled Nulang benchmark differs, not as a replacement for paired A/B or interpreter semantic parity. This captures host hardware events for **the supplied executable**; it does not attribute individual machine instructions or assert a performance improvement.

## Prerequisites and usage

- Linux host with `perf` installed and permission to read hardware events (`perf_event_paranoid` / container restrictions may prevent access). No automatic `sudo` or system configuration changes.
- An **already-built**, non-destructive benchmark executable. The command is run once per warmup and once per measurement. Do not pass production credentials or effectful commands.
- Capture before/after on **the same CPU** with matched affinity, build flags, input, and source revision; prefer an idle, dedicated host.

Example (the runner must already be built):

```sh
cargo build --release --bin nulang-jit-bench
python3 scripts/cpu_counter_profile.py --output target/cpu-profile-jit-1 --repeat 5 --warmup 1 -- target/release/nulang-jit-bench --format jsonl --repeat 1
python3 -m unittest discover -s scripts/tests -p 'test_cpu_counter_profile.py'
```

The profiler writes `manifest.json` with repo HEAD, CPU model, kernel, process affinity, perf version, command, run exit codes, and available counters. Raw perf reports, stdout, and stderr are saved per run. Output directories must not exist already. A missing `cycles` or `instructions` counter fails closed and preserves captured evidence; optional unavailable counters appear as `null`.

Counter totals cover the **whole benchmark process**, including its initialization and potentially more than one benchmark case. For instruction-specific conclusions, use an isolated executable/workload and correlate with a disassembly or profiler sample. Branch and cache event counts are hardware-specific; raw totals must be normalized to equal work. The profiler intentionally avoids automatic CI gates and publication.

See `benchmarks/README.md` and `benchmarks/JIT_CODEGEN.md` for properly paired benchmark methodology.
