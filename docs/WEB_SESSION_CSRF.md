# Development session CSRF enforcement

Nulang's experimental `@nulang/auth` development module issues two independent
256-bit random values when a session starts: an opaque session cookie and a
server-side CSRF synchronizer token. Both are kept in the **ephemeral** Web KV
store; logout removes both keys.

For an unsafe HTTP method (POST, PUT, PATCH, DELETE, or an unrecognized unsafe
method), the experimental routed Web server checks requests containing the
first-party `nulang_session` cookie **before** running typed or legacy route
handlers. The request must supply exactly one valid token through either:

- Form POST: a single `_csrf` field in an `application/x-www-form-urlencoded`
  body, obtained in server-rendered Nulang code through `csrf_token()`.
- JS/API request: an `X-CSRF-Token` header containing the same value.

Missing, malformed, duplicated, or mismatched tokens receive HTTP 403 with
`Cache-Control: no-store`. A valid token permits normal route processing;
it does not grant authorization. GET, HEAD, and OPTIONS are not CSRF-protected
and must remain free of state-changing effects. The actual session cookie
is `HttpOnly; Secure; SameSite=Lax` and should never be made available to JS.

This enforcement only covers the first-party `nulang_session` development
cookie. It does **not** protect external auth providers, anonymous login CSRF,
or other session cookie names. It also does not replace strict Host/Origin
admission, XSS prevention, authorization, production session expiry and
revocation, or persistent/hashed session storage. Do not deploy this module
as production authentication. Combine it with the independent Web
same-origin admission work before production use.

Cookie helper hardening: `Web.set_cookie` only emits names conforming to HTTP
token syntax and unquoted RFC 6265 cookie-octet values; `Web.clear_cookie`
also rejects malformed names. Values containing delimiters, whitespace, or
header controls are not emitted. Encode non-token payloads with base64url.
These safeguards do not imply production readiness or harden arbitrary
application-provided response headers.

## Request-origin and response-writer integration (draft)

The routed experimental Web server now checks request provenance **before**
the development-session synchronizer check or any typed/legacy handler
dispatch. An unsafe request requires a unique Host and either a matching
Origin or a same-origin Referer, with strict origin-boundary matching. A
contradictory `Sec-Fetch-Site` or duplicate provenance header is rejected.
Only direct-listener IP/localhost hosts are accepted by default. Set
`NULANG_WEB_PUBLIC_ORIGIN` to one canonical `https://host[:port]`
origin when terminating TLS at a trusted reverse proxy. The code does
not trust arbitrary `X-Forwarded-*` headers. A missing or invalid
configuration blocks unsafe requests rather than inferring an origin.
Browser forms send an Origin/Referer automatically; non-browser callers
must also provide valid provenance.

The response writer rejects malformed response header names/values and
caller-supplied Content-Length or Transfer-Encoding, returning a generic
HTTP 500 instead of interpolating invalid framing. Existing development
cookie helpers separately enforce RFC cookie-name/value grammar.
This does **not** establish production authentication or authorization,
cover state changes implemented through safe HTTP methods, or replace
general HTTP/1.1 request admission, verified identity, durable session
expiry/revocation, or per-tenant database isolation. The generic
`Http.serve` endpoint is not covered by this routed Web origin gate.
