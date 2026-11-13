# Experimental Nulang Web HTTP benchmark

This diagnostic harness runs against an **already-running local Nulang Web server**. Python standard library only; it opens a new TCP connection for each GET, matching the current experimental server's connection-close behavior. This is not a browser benchmark, distributed load test, or production capacity claim.

## Baseline

Start a Nulang Web application exposing a known static file and note the local port:

```sh
python3 scripts/web_http_bench.py --port 8080 --path /app.js --requests 1000 --concurrency 4 --warmup 100 --label "$(git rev-parse HEAD)" --output /tmp/nulang-web-get.json
```

The JSON includes request count, status distribution, transport errors, failures, wall-clock requests/second, successful-request end-to-end p50/p95/max latency, and total **response body** bytes. Failures include all nonexpected statuses and connection errors. Failed warmup aborts measurement; failed measured requests cause a nonzero exit and remain in the request count.

For a build with static ETags, measure revalidation:

```sh
python3 scripts/web_http_bench.py --port 8080 --path /app.js --requests 1000 --concurrency 4 --warmup 100 --revalidate --label "$(git rev-parse HEAD)" --output /tmp/nulang-web-304.json
```

The preflight performs one **unmeasured** GET and requires HTTP 200 with an ETag, then sends that ETag as If-None-Match. Conditional requests expect HTTP 304. The current proposed ETag implementation still reads and hashes the file for each request: revalidation reduces body bytes, not necessarily CPU or end-to-end latency.

For dynamic routes, omit `--revalidate`. Use `--expected-status` for deliberate non-200 responses.

## Fair comparison and limitations

1. Compare **the same request and expected status** for performance A/B claims. Comparing a 200 and 304 is useful for transfer behavior but not a controlled server throughput optimization.
2. Pin exact binary and source commit, hardware, Python version, concurrency, and endpoint/file size. Alternate base and candidate on the **same otherwise-idle host**, at least five paired repetitions.
3. Keep failures in the report. Treat errors as a correctness/capacity failure, never discard them as slow samples. Report both throughput and latency.
4. Vary concurrency 1, 4, 16, 32 in separate trials. The Python client, TCP connection setup, loopback stack, and host scheduling all affect reported performance.
5. Browser DOM/hydration speed needs a real Chromium/WebKit browser and compiled Nulang SSR fixture; this tool cannot measure rendering, INP, memory leaks, or hydration lifecycle.
6. The tool permits only loopback hosts and limits concurrency to 64. Do not use it to load-test external services.

## Harness self-tests

```sh
python3 -m unittest discover -s scripts/tests -p 'test_web_http_bench.py' -v
```

The tests use an independent local Python HTTP fixture and verify harness accounting, conditional GET handling, and CLI validation. Passing them does **not** establish a Nulang Rust build or a production throughput baseline.

Benchmarks are opt-in and do not alter existing CI gates, compiler, browser, or runtime behavior.
