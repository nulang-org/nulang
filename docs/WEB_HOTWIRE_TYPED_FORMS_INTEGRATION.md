# Draft: Hotwire interoperability on guarded typed-form Web runtime

This review-only integration reconciles the audited Turbo Frames/Streams branch
(#2584) with the patched SQLite, typed form/VM ABI and HTTP lifecycle branch
(#2585). It does not replace either source PR, merge to `main`, or deploy.

## Merge-specific behavior

`WebDevServer` now executes compiler-bound typed-form handlers inside the same
request context as legacy and direct-bound handlers. Without that context,
`Web.turbo_stream` rendered correct markup but returned `text/html`, because
its request-scoped media-type marker could not be set. Each handler invocation
resets response metadata; normal HTML requests still use `text/html` and inject
the existing lightweight client runtime, while Turbo Stream responses emit
`text/vnd.turbo-stream.html` and are not script-injected.

All existing HTTP provenance and session-bound CSRF checks execute **before**
form binding or handler dispatch. Guarded typed-form inputs still receive
structured 422 validation errors. Stream HTML stays escaped using `Web.text`.
Do not bypass or weaken these checks for browser-driven mutations.

## Local verification (Gertrude, isolated worktree, Rust 1.95.0)

- Red-before test: three of four Turbo Frames tests failed on the typed-form
  parent, including unsafe HTML/URL becoming HTTP 200 `nil`.
- Red-before integration regression: CSRF-validated `Option[String]` form POST
  generated a Turbo Stream body with incorrect `text/html` content type.
- Green-after: `web_http_server_contracts` 14/14, `web_turbo_frames` 4/4,
  `web_typed_form_http` 8/8, plus `web_http_form_guard` suite passed.
- `cargo test --offline --locked --no-default-features --lib`: 2327 passed,
  0 failed, 1 ignored.
- `cargo fmt --all -- --check`: passed.
- `cargo audit --no-fetch`: exit 0, no known vulnerabilities in the locally
  cached advisory database; three allowed warnings remain. This is not a
  substitute for fresh CI-based security audit.
- Form-specific regression confirms typed Turbo MIME, CSRF 403 admission,
  no Nulang client script injection, and escaped hostile form values.

## Outstanding release gates

- Keep **draft** pending *exact-head hosted* default and no-default features,
  Clippy, fresh cargo-audit, browser/Playwright, WASM, release, and relevant
  security CI; treat pending or red gates as blockers.
- The inherited patched SQLite change intentionally refuses remote
  libSQL/Turso connections. This is a breaking capability limitation for
  remote-libSQL deployments and must be resolved or explicitly approved.
- Confirm session revocation, tenant authorization/RLS, and authenticated
  realtime scopes before production use. The dev HTTP server is not an
  independently audited Internet-facing production boundary.
- The official Turbo JavaScript client is optional and pinned by the browser
  test harness; do not introduce a second browser runtime or public private-data
  SSE path. Complete no-JavaScript navigation and browser-level security
  acceptance on the exact integration commit.
