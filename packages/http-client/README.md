# http-client

A small HTTP request builder over the built-in `Http` effect, with
percent-encoding and query-string helpers. Official seed package
(Experimental tier).

## Install

```bash
nula add http-client
```

## Usage

```nulang
import lib
```

### Building requests (pure)

Requests are plain records assembled by builder helpers — nothing touches
the network until `send_request`:

```nulang
let req = with_header("Accept", "application/json",
          with_query_param("q", "nu lang",
          get_request("https://api.example.com", "/search")));
request_url(req)
// "https://api.example.com/search?q=nu+lang"
```

| Function | Description |
| --- | --- |
| `get_request(base, path)` / `post_request(base, path, body)` | start a request |
| `with_header(name, value, req)` | add a header |
| `with_query_param(k, v, req)` | add a percent-encoded query parameter |
| `request_url(req)` | full URL including the query string |
| `url_encode(s)` | percent-encode (space becomes `+`) |
| `build_query(params)` | `k1=v1&k2=v2` from `(key, value)` pairs |

### Sending

The experimental Http.request operation is available in native builds with
the `ureq` Cargo feature (included in the full/default build). The package
serializes headers using the standard JSON encoder before invoking that
effect, so headers are **actually transmitted**.

```nulang
let req = with_header("Authorization", "Bearer example-token",
          get_request("https://api.example.com", "/health"))
let raw = send_response(req)  // JSON string: {status, ok, headers, body}
let body = send_request(req)   // body-only compatibility shortcut
```

Each call above performs **its own HTTP request**; choose one or the other.
Do not call both on a non-idempotent operation.

- `send_response` preserves status (including 4xx/5xx), headers, and
  UTF-8 response body. `send_request` returns `nil` on non-2xx status to
  preserve the legacy body-only behavior. Both functions return `nil` for invalid requests,
  transport failures, binary responses, or body-size limits.
- Supported methods: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS.
- No automatic redirects or retries, including on requests carrying credentials.
- Header name/value validation rejects control characters, duplicates, and
  transport-managed fields such as Host or Content-Length.
- Each request has a 15-second timeout and 4 MiB request/response text limit.
- The base builder's URL encoder currently handles ASCII reliably; **non-ASCII
  query values should be percent-encoded by a Unicode-aware caller** pending a
  dedicated UTF-8 encoding fix. Existing `Http.get` / `Http.post` retain their old semantics.
- No streaming, multipart uploads, cancellation or user-configurable timeouts
  in this first iteration. Those need an expanded typed request contract.

## Tests

```bash
cargo test --locked --features ureq --test http_outbound_request
nula test
```

The Rust tests use loopback HTTP servers to assert that authorization and
custom headers cross the wire and that response status, redirect policy and
request validation match the contract. They do not require public internet.
