# Experimental HTTP/1.x ingress contract

The current TCP-backed Nulang Web host uses bounded, single-request HTTP/1.x
framing. This is **not** a claim of full HTTP/1.1 compliance. The host closes
each request after sending its response; persistent connections, chunked bodies
and `Expect: 100-continue` remain unsupported.

## Admission behavior

- Parse the HTTP version explicitly; accept HTTP/1.0 and HTTP/1.1 only.
- Require exactly one `Host` field line for HTTP/1.1. Duplicated Host
  fields are rejected even when values agree, including case variants.
- Reject invalid Host values containing whitespace or authority delimiters
  such as comma, slash, backslash, userinfo, query or fragment characters.
- A present, empty Host is allowed for authority-less targets, as described
  by RFC 9112. HTTP/1.0 requests may omit Host.
- Reject request targets containing URI fragments or ASCII control characters
  before route dispatch.
- Accept HTTP(S) absolute-form request targets, normalize their path and query
  for route dispatch, and use the absolute target's authority in preference
  to a conflicting `Host` header, as required by RFC 9112.
- Reject unsupported URI schemes and malformed or ambiguous authority values.
  This bounded single-request host is not a forward proxy. URI validation,
  full proxy semantics, persistent connections and advanced HTTP features
  still require a mature production-grade transport.
- Existing Content-Length, transfer-encoding, header-size, body-size and
  connection-limit safeguards remain in place.

## Security boundary with routed Web

`read_http_request` is shared by the standalone `Http.serve` listener and
the experimental routed Web server, so HTTP version, target, and Host admission
apply to both. The later Origin/Referer and development-session CSRF checks
are **routed-Web only**; they are not a general-purpose authorization layer for
`Http.serve`. HTTP/1.1 Host syntax validation alone does not authenticate a
host, prevent DNS rebinding for safe-method handlers, or provide an origin
trust policy at a reverse proxy. A deployment must enforce trusted proxy
routing and application authorization separately.

The normative reference is
[HTTP/1.1 RFC 9112](https://www.rfc-editor.org/rfc/rfc9112.html),
especially Section 3.2 (request target/Host) and message framing.

## Regression tests

`src/runtime/http_server.rs` exercises fragmented reads, malformed lengths,
unsupported transfer encoding, missing/duplicate/invalid Host, HTTP/1.0
compatibility, protocol versions, and rejected fragment-bearing targets.

```sh
cargo fmt --all -- --check
cargo test --locked --lib runtime::http_server::tests
cargo test --locked --test web_http_server_contracts
cargo clippy --all-targets -- -D warnings
```

These must pass at the exact proposed commit, followed by repository-wide
feature-matrix and release checks. Dedicated wire-level conformance and
fuzzing are still required before promoting this development transport to a
production-grade HTTP engine.
