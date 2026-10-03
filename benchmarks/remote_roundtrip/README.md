# Remote actor round-trip baselines

This suite measures a steady-state distributed-runtime operation rather than the
NUL0 codec in isolation. One logical round trip is:

1. sender/network endpoint transmits one signed 64-bit sequence value over loopback TCP;
2. the receiver dispatches that value through an actor/process inbox and handler;
3. the receiver returns the same value over the established TCP connection;
4. the sender dispatches the returned value through its local actor/process inbox.

The measured interval excludes process startup, listener creation, connection
establishment, and warm-up. Every implementation verifies the returned sequence
so the workload cannot be optimized into a no-op.

## Implementations

- **Nulang** — `src/bin/nulang_remote_roundtrip.rs`: two real `Runtime`s using the production NUL0 TCP transport in explicit `PlaintextInsecure` mode, remote actor routing, mailbox admission, scheduler dispatch, and native actor handlers.
- **Go** — `go_baseline.go`: loopback TCP plus a goroutine/channel actor on each endpoint. It deliberately does not use a raw echo loop, because that would remove the scheduling/handler boundary Nulang must execute.
- **Erlang** — `erlang_baseline.escript`: loopback `gen_tcp` plus one Erlang process actor on each endpoint, with request/reply references pinning handler completion.

The wire encodings are runtime-native rather than byte-identical. The Go and
Erlang fixtures therefore answer "what does this complete semantic operation
cost in this runtime stack?", not "which codec is faster?". Use
`dist/nul0_actor_message_{encode,decode}` for NUL0 codec-specific questions.

All three use plaintext loopback transport. This is a runtime overhead benchmark,
not a WAN, TLS, packet-loss, or cluster-convergence benchmark.

## Run

```bash
python3 scripts/remote_roundtrip_bench.py \
  --runs 8 \
  --roundtrips 10000 \
  --warmup 1000 \
  --cpu-mode single \
  --output /tmp/nulang-remote-roundtrip.json
```

The default `single` mode pins each measured runtime process (including child
threads) to the same one logical CPU. Go additionally uses `GOMAXPROCS=1`; BEAM
uses one normal scheduler. `host` mode is diagnostic and should not be mixed
with single-CPU results.

Run the harness tests with:

```bash
python3 -m unittest scripts/tests/test_remote_roundtrip_bench.py
cargo test --locked --test distributed_remote_roundtrip
```

## Interpretation

Compare medians only within one report produced on one host. Do not compare
absolute values copied from different machines. The harness counterbalances
runtime order on alternating rounds to reduce monotonic thermal/load drift and
records CPU/toolchain metadata in the JSON report.

A result from this suite supports only the specific steady-state loopback
actor-roundtrip workload above. It is not a universal language/runtime ranking.
