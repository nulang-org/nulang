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
