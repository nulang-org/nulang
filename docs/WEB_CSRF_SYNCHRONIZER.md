# Nulang Web synchronizer CSRF primitive (draft)

`web::csrf::SessionCsrfToken` provides a 32-byte OS-random, canonical
64-character lowercase-hex **synchronizer token**. Verification uses
`subtle::ConstantTimeEq` over fixed 32-byte decoded values. It rejects
missing, duplicate, malformed, or wrong-session tokens.

**This module is not middleware and does not enable POST, any mutating HTTP
route, or browser-facing preflight.** It is stacked after the compiler-owned
request preflight in PR #2336 and is review-only.

## Required integration boundary

A future authenticated Web handler MUST, in this order:

1. Apply the audited Host and same-origin/Fetch Metadata request-admission
   policy before any handler, token lookup, or database call.
2. Authenticate the browser's session using **server-owned session state**.
   Return a generic denial for absent/invalid sessions. Keep opaque bearer
   token handling distinct from cookie-session CSRF policy.
3. Load the expected `SessionCsrfToken` from that exact server-side session.
   Rotate it on session creation, privilege changes, and logout. **Never**
   accept the expected token from the incoming request or from a cookie.
4. Call `http_request::capture_submitted_csrf_tokens(headers, raw_body)`
   on the original header list and raw bytes *before* collapsing form fields
   into a `HashMap`. This helper preserves duplicate headers and form names,
   including percent-encoded aliases. It rejects multiple `Content-Type`
   headers, malformed UTF-8 in form bodies, and URL-encoded bodies over 1 MiB.
   Refuse multiple token submissions even if identical. Ignore query parameters
   as token carriers.
5. Call `verify_presented(&[candidate])`; reject all failure kinds with the
   same generic HTTP 403 (no details to clients). Apply request size and rate
   limits at transport admission.
6. Resolve the caller's tenant and resource authority **independently from
   request-controlled tenant headers/fields**, under the authoritative identity
   and database RLS boundary. Verify resource ownership before returning
   field-level preflight diagnostics, which may disclose schema information.
7. Use `preflight_after_session_csrf` only after all admission checks. The
   helper combines token verification and the existing effect-free scalar
   decoder, **not** authentication or tenant authorization. On final actual
   submission, repeat *all* security and validation checks inside the
   authoritative transaction, including concurrency/version constraints.

The current `HttpRequestBindingInputs::capture` still collapses form keys
into a map. That map is **not suitable for CSRF admission**. The separate
`capture_submitted_csrf_tokens` helper is now tested against duplicate form
names and headers, but it is **not wired to any network handler**. Before using
it on a listener, independently review its error mapping, request admission,
body limits, session storage, and tenant-authority integration.

### Security limits

- A synchronizer token does not defend against XSS; output escaping and CSP
  remain necessary.
- `SameSite` cookies and Origin checks are defense in depth, not a substitute
  for validating the session-bound token on cookie-authenticated mutations.
- The utility does not own session persistence/expiration, key rotation,
  revocation, authenticating identity, or PostgreSQL tenant roles.
- Do not introduce a public endpoint until exact-head native/WASM/security/
  release validation and independent review of the full Web chain are complete.
- Keep the existing disposable JWT→Postgres test HTTP route GET-only until a
  real authenticated-session, CSRF, and tenant authority path is proven.

## Focused verification

```sh
cargo test --locked --no-default-features --test web_csrf_synchronizer
cargo test --locked --no-default-features --test web_csrf_http_capture
cargo test --locked --no-default-features --test web_request_preflight
cargo test --locked --no-default-features --lib web::
cargo fmt --all -- --check
```
