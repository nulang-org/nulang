# Hotwire interoperability on the audited Web development ingress

This draft integration is **stacked on** the consolidated Web security
branch in PR #2517. It combines the independently reviewed Hotwire
Streams (#2576), Turbo Frames (#2578), and fail-closed handler result
(#2583) changes. Nothing here turns the experimental Web development
server into a production authentication/authorization boundary.

## HTTP contract and security admission

- For ordinary Turbo Frames, send `text/html; charset=utf-8` and render
  a `<turbo-frame>` matching the requesting frame ID.
- For successful Turbo Stream actions, the Web host emits
  `text/vnd.turbo-stream.html; charset=utf-8` and the standard Turbo
  markup without injecting the Nulang browser-runtime script.
- The stream handler never bypasses the parent Web admission pipeline.
  The same-origin POST must present **exact** listening authority in Host
  (including the nondefault port) and matching Origin or accepted Referer.
  Cross-site Fetch Metadata is rejected. Invalid/missing provenance must
  fail with 403 before handler dispatch and must not expose stream markup.
- When a development-session cookie is present, the parent synchronizer
  token admission remains mandatory. A forged or stale session cannot
  simply add the Turbo MIME Accept header to avoid CSRF checks.
- Host serialization rejects unsafe URL attributes, which now produce
  HTTP 500 rather than a successful response containing literal `nil`.
  A legitimate String value `"nil"` remains a successful rendered body.
- Neither the `Turbo-Frame` header nor the Turbo MIME Accept value proves
  identity, tenant membership or authorization. Every frame and mutation
  handler needs its own application access check.
- `Web.raw` remains a trusted escape hatch. Dynamic text should go
  through `Web.text`; this integration does not provide taint tracking.

## SSE / live updates (not enabled by this PR)

The legacy `/__nulang/sse/<room>` endpoint stores and replays room
messages without a tenant/session authorization contract. **Do not**
point Turbo at it for private data. The existing authenticated UI
session-stream proposal (#2400) uses `nulang-ui-msg/1` and carries its
own provenance, revision and reconnect guarantees. Those events are
*not* Turbo Stream HTML, so there is no automatic protocol conversion.

A future SSE-to-Turbo adapter must be attached **only after** the host
has verified the session/tenant/view, authorized the specific event,
committed state, and handled revocation, bounded queues, reconnects and
backpressure. Reuse existing admission and SSE lifecycle controls; do
not create a second public streaming route.

## Validation and stop-ship conditions

Focused local tests:

```
cargo test --locked --no-default-features --test web_http_server_contracts --test web_turbo_frames
cargo test --locked --no-default-features --lib
cargo fmt --check
cargo audit
```

The isolated browser regression exercises the actual guarded WebDevServer
with the official Turbo client (pinned to 8.0.23) and Chromium/Playwright:

```bash
npm ci --prefix scripts/tests/hotwire-browser
scripts/tests/hotwire-browser/node_modules/.bin/playwright install chromium
cargo build --locked --no-default-features --example web_hotwire_browser_test_server
node scripts/tests/web_hotwire_browser_e2e.cjs
```

The dedicated `Hotwire browser acceptance` workflow runs that regression
on changes to the relevant Web files. Dependencies live only in
`scripts/tests/hotwire-browser`, not Nulang's product runtime. Tests cover
same-origin POST create/update/delete, Frame navigation, hostile HTML
escaping, origin/CSRF refusals and a no-JavaScript navigation fallback.
This is **not** a substitute for authenticated tenant/stream testing.

The audit **currently fails** on this parent lockfile with
`RUSTSEC-2026-0459`: `nulang-ai-local → rusqlite 0.32.1 →
libsqlite3-sys 0.30.1`. This is not caused by Hotwire, but is a genuine
release blocker for the combined tree. The distinct patched SQLite
provider / actor-store unification proposal is PR #2551, with its own
stacked dependency/release gate. Do not suppress RustSec or directly
upgrade to two incompatible bundled SQLite FFI providers.

Also require terminal exact-head CI, default-features/SQLite linkage,
reverse-proxy testing, browser acceptance with the official Turbo
client against the *actual* guarded host, and separate authenticated
session/SSE authorization tests before calling this production-ready.
Do not merge or deploy this draft while these gates remain open.
