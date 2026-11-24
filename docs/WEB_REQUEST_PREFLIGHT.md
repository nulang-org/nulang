# Web request preflight (experimental library primitive)

Nulang provides a transport-neutral, non-executing **input decoding preflight**
for typed Web route contracts. It is inspired by Laravel Precognition but does
not yet implement all Precognition features.

`web::request_bindings::preflight_request_arguments(bindings, handler_param_count, values)`
uses the same compiler-owned binding plan, scalar decoder, and plan validation
as `bind_request_arguments`. It validates path, query, header, cookie, body,
and URL-encoded form inputs without calling the route handler or performing
Nulang effects. It returns all missing/invalid scalar fields in binding order,
not only the first error.

Example (input `quantity=abc&expedited=perhaps` for an Int and Bool form):

```json
{
  "valid": false,
  "errors": [
    {
      "code": "invalid_request_input",
      "source": "form",
      "source_name": "quantity",
      "expected_type": "Int",
      "message": "Invalid field value"
    },
    {
      "code": "invalid_request_input",
      "source": "form",
      "source_name": "expedited",
      "expected_type": "Bool",
      "message": "Invalid field value"
    }
  ]
}
```

The public JSON intentionally does not contain handler names, handler ABI slots,
raw supplied values, or compiler diagnostics. Malformed/incomplete plans return
`Err(RequestDecodeError)` with server-error classification; they are not field
errors. A successful preflight means only that **scalar request decoding** works.

## Security boundary and non-goals

**There is no new HTTP route or browser API.** In particular, the preflight
does not authenticate or authorize the caller, enforce tenant/resource policy,
check session/CSRF tokens, validate domain/business rules, reserve database
state, execute transactions, or guarantee final submission will succeed.

Before exposing this from any Web adapter:

1. Use the **same** route admission, authentication, CSRF, tenant authority,
   capability checks, and throttling as the corresponding real mutation.
2. Derive all trusted identities and locked fields server-side. Never take
   user/tenant authority from arbitrary preflight form fields.
3. Ensure domain validators are explicitly pure/read-only and bounded before
   extending the preflight beyond scalar type decoding.
4. On final submission, **repeat validation** and authorize inside the
   authoritative transaction. A prior preflight result is never an
   authorization token or a substitute for concurrency controls.
5. Keep the HTTP mutation surface disabled until the audited Web security
   integration and its exact-head CI/release gates have been approved.

This deliberately reuses existing Web contracts; it does not introduce a
second validation DSL, HTTP router, or action runtime.
