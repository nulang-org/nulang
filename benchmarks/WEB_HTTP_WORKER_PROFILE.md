# Experimental HTTP worker profiling (2026-11-27)

## Scope and reproducibility

Optimized release-cli Nulang build at commit
`6f90871e07fed9e1fec3296aa1bb2df080770271`, binary SHA-256
`bb4255653b57f321635e593d3c551caf7003257b4baaea7460baf7a6b5a188c7`.
Ryzen 7 8745HS (8 cores, 16 logical CPUs), Linux loopback. Server affinity
0-3,8-11; client affinity 4-7,12-15. Python 3.14.4; Go 1.27.4.
Workload: examples/patch-web, `/probe.bin` (16 KiB) and `/` (437-byte
dynamic response), fresh TCP GETs, HTTP 200 and exact body size checked by
the native client. Each comparison is median of five alternating paired
repetitions of 1,500 measured requests per endpoint/concurrency/variant,
after a separate warmup. Same server binary across the Python-vs-Go trials.
Server CPU time is Linux /proc process user+system CPU ticks divided by
request count; this is coarse at small sample durations. The host was
shared: load averages were 9.52/9.24/5.39 before the client comparison and
7.47/8.76/5.33 afterward. Results are indicative, not production capacity.

The profiling scratch Go client source SHA-256 was
`8413515786000f43191e80e359fafe6091136983aad6f47c0bc5df65e23af756`;
binary SHA-256 `38f1ad3c51525d2ea8f7a308b7460e2e6fd71276fb6485665d08f1cafb56a615`.
The optional repository client is the same successful-request measurement
engine, refactored for testability and stricter CLI/error accounting after
these experiments. Raw scratch report SHA-256
`95476c46289fd888f215c59202ffbc9aa5f49c8bb2ae45e57663c7a58366f528`.
Replay with the optional native comparator and the instructions in
[WEB_HTTP.md](WEB_HTTP.md); raw reports and the prototype patch remain
local to the measurement environment, not part of the runtime.

## Client effects

| Endpoint | Concurrency | Python req/s | Go req/s | Python p95 ms | Go p95 ms |
|---|---:|---:|---:|---:|---:|
| Static | 1 | 4711 | 5337 | .294 | .282 |
| Static | 4 | 9202 | 19514 | .630 | .307 |
| Static | 16 | 8093 | 25672 | 3.074 | 1.029 |
| Static | 32 | 8311 | 26329 | 6.103 | 1.718 |
| Dynamic | 1 | 4099 | 4340 | .337 | .345 |
| Dynamic | 4 | 8838 | 17523 | .630 | .332 |
| Dynamic | 16 | 8464 | 20901 | 3.091 | 1.273 |
| Dynamic | 32 | 8370 | 21120 | 5.909 | 2.100 |

120,000 measured requests, zero failures. Python's plateau does not
establish server saturation. At concurrency 48 and 64, one 1,500-request
dynamic native trial each completed without failures (20,348 and 16,076
req/s respectively, p95 4.52 and 8.06 ms). These single trials are
capacity probes, not paired estimates.

## Synchronous logging and thread creation

Separate file vs /dev/null stderr tests (three paired repetitions, 1,500
requests, concurrency 16/32, both endpoints; 36,000 requests, zero
failures) produced:

| Endpoint | Concurrency | File req/s | Null req/s |
|---|---:|---:|---:|
| Static | 16 | 23143 | 25726 |
| Static | 32 | 22247 | 26784 |
| Dynamic | 16 | 15129 | 17691 |
| Dynamic | 32 | 14709 | 18479 |

This demonstrates sink sensitivity, not that logging should be disabled.
Under separate strace -f -c sessions, each 500-request run made 512
clone3 calls and 3,198 write calls, including startup. Static requests
also opened files. Both runs completed with no request failures. Counts,
not traced elapsed time, are meaningful here. perf hardware events were
unavailable due to host perf_event_paranoid=4; no permissions changed.

## Eight-worker rendezvous experiment (not production code)

An isolated measurement patch starts eight bounded idle workers and
uses a zero-capacity crossbeam rendezvous. Busy workers cause immediate
fallback to the existing dedicated thread path; no new request queue.
Socket ownership and the existing connection cap are retained. Four web
thread-local slots are cleared before/after a pooled request, which is
not proof of isolation across all runtime subsystems.

Prototype patch SHA-256
`0bd9dca06e73776b9bc47f401cef6c3628cce6685c23fd5b525681383f4f2c8e`;
prototype binary SHA-256
`4f29fed89be927120038e2d120319b5a4af369928e29133b7581b71e3116a5fc`.
Raw report SHA-256
`9a3451b85aeaa8da15d1de7414744c5e5aed28893e017c803db5a02703819644`.
Host load before 3.83/4.73/4.54; after 3.03/4.48/4.47.

| Endpoint | C | Base req/s | Reuse req/s | Base p95 ms | Reuse p95 ms | Base CPU us/req | Reuse CPU us/req |
|---|---:|---:|---:|---:|---:|---:|---:|
| Static | 1 | 5509 | 6352 | .261 | .222 | 146.7 | 93.3 |
| Static | 4 | 20054 | 18543 | .292 | .332 | 140.0 | 80.0 |
| Static | 16 | 23616 | 35680 | 1.807 | 1.025 | 166.7 | 100.0 |
| Static | 32 | 25894 | 36699 | 2.640 | 1.929 | 160.0 | 106.7 |
| Dynamic | 1 | 4505 | 5392 | .319 | .262 | 186.7 | 120.0 |
| Dynamic | 4 | 17129 | 19231 | .325 | .311 | 186.7 | 100.0 |
| Dynamic | 16 | 17394 | 24761 | 1.990 | 1.499 | 220.0 | 140.0 |
| Dynamic | 32 | 18503 | 25789 | 3.915 | 2.709 | 233.3 | 153.3 |

120,000 paired HTTP requests, zero failures. A separate 500-request
dynamic trace per variant recorded 512 vs 28 clone3 calls (including
startup) and 3,198 writes each, zero failures. Idle threads were 2 vs 10.
Median idle RSS was 25,584 vs 24,996 KiB, too noisy to claim memory savings.

### Mixed SSE/HTTP single-run check

32 held SSE streams (four rooms of eight) occupied workers. Ninth stream
in the first room returned 503; all 12,000 measured HTTP requests across
both variants succeeded. All streams were reset and capacity readmitted;
threads returned to 2 vs 10 after 41 vs 103 ms, respectively. These
are single-run reset recovery observations, not a timing guarantee.

| Endpoint | C | Base req/s | Reuse req/s |
|---|---:|---:|---:|
| Static | 4 | 20237 | 20314 |
| Static | 16 | 25959 | 24127 |
| Dynamic | 4 | 17549 | 16417 |
| Dynamic | 16 | 16636 | 16108 |

The reuse win disappears when long-lived SSE occupies the idle workers.
That and the static c4 regression require further design/measurement.

## Decision and next work

- Keep the production runtime unchanged in this diagnostic change.
- First test a single-write request logging change without losing log
  content, or changing filtering, failure semantics or security controls.
- Evaluate worker reuse only with a complete thread-local and per-request
  resource/tenant isolation audit, deterministic cross-tenant sequential
  tests, panic and shutdown handling, and a design separating short HTTP
  work from potentially indefinite SSE lifetimes without unbounded queues.
- Require repeated mixed-load measurements (including quiet and busy hosts),
  bounded RSS and identical overload behavior; do not interpret the isolated
  prototype as a security fix, deployable design or real-world capacity claim.

These measurements cover HTTP and SSE, not browser interaction metrics,
WCAG conformance, real network clients, TLS, or multi-process scaling.

## Single-write request logging A/B (2026-11-27, review candidate)

A separate, isolated patch preformats one request record and calls
`eprint!` once, replacing the `eprintln!` call whose formatting fragments
produced multiple stderr writes. Request-log contents, stderr destination,
response behavior, connection limits and runtime dependencies remain
unchanged. The change does not attempt to sanitize previously logged query
parameters; existing logging privacy/security policy still needs review.

Control is the exact release-cli binary SHA-256
`bb4255653b57f321635e593d3c551caf7003257b4baaea7460baf7a6b5a188c7`;
candidate binary SHA-256
`bcbeb53f33cc8e1ff671598a5380116becdb7a8dec4aaf4a245841eb42312696`.
Same disjoint CPU affinities, fixtures, load client and hardware as above.

Five paired, alternating 1,500-request trials per endpoint/concurrency,
120,000 requests with zero failures; all 126,400 warmup+measurement
request-log records matched the expected line text and counts for
each variant (ordering was nondeterministic). The baseline
vs candidate medians were: static c1 5,340/5,791 req/s, static c4
19,841/19,559, static c16 27,831/32,866, static c32 28,625/32,016;
dynamic c1 4,314/4,639, dynamic c4 18,393/19,069, dynamic c16
21,426/22,386, dynamic c32 22,094/20,773. The dynamic c32 result
regressed in this shorter experiment. Under separate strace -f -c
500-request dynamic trials, `write` syscall counts fell from 3,198
to 698, including startup; `clone3` remained 512. Do not infer
syscall timing from traced execution. Raw report SHA-256:
`a16380181a97d0c2edae153e7f716be3e55614eabc0f52487d1a4a23c4b75e07`.
Host load averages were 1.55/2.56/3.26 before and 1.18/2.41/3.20 after.

To resolve the high-concurrency ambiguity, the same experiment was
extended to five paired 10,000-request trials for the four workloads
below (400,000 requests, zero failures). The baseline vs candidate
throughput and p95 are medians across five trials, not aggregated
percentiles:

| Endpoint | Concurrency | Base req/s | Log req/s | Base p95 ms | Log p95 ms | Base CPU us/req | Log CPU us/req |
|---|---:|---:|---:|---:|---:|---:|---:|
| Static | 4 | 20408 | 20270 | .276 | .288 | 150 | 140 |
| Static | 16 | 30885 | 32423 | .767 | .705 | 169 | 156 |
| Static | 32 | 30274 | 31776 | 1.392 | 1.291 | 175 | 160 |
| Dynamic | 32 | 23440 | 24240 | 1.908 | 1.616 | 243 | 219 |

Raw longer-run report SHA-256:
`2ae5f7aa386330029a873ef7a855cde81b7984a6b43660a38ff0ab2453b6d751`.
Interpret the ~5% high-concurrency gains conservatively, since this is
one shared development host, not a production workload. Static c4
throughput was ~0.7% lower, p95 ~0.012 ms higher. No new logging
configuration, suppression, queue, thread pool or security policy is
introduced. This patch remains separate from the worker-reuse
prototype and requires exact-head CI plus independent review.
