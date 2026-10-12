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

### Sending (requires a runtime host)

```nulang
let body = send_request(req)   // performs Http.get / Http.post
```

The legacy `send_request` API uses the built-in `Http.get` and
`Http.post` effects. **Those effects cannot send custom request headers,
arbitrary methods, or GET bodies.** The original `send_request` API silently
ignored headers and treated every method other than POST as GET. It remains
available unchanged for compatibility, but avoid it with untrusted or
dynamically constructed requests.

**Prefer the new opt-in checked API** when using the current transport:

```nulang
let req = get_request("https://api.example.com", "/health")
match send_request_checked(req) {
  Ok(body) => perform IO.print(body),
  Error(reason) => perform IO.print(reason),
}
```

`validate_transport(req) -> Result[Unit, String]` performs a pure preflight
aside from reading the request's array length. It rejects unknown HTTP methods,
custom headers, and GET request bodies. `send_request_checked(req)` runs that
validation before network I/O and returns `Result[String, String]`: on success
only the response body is available.

**This is not a full typed HTTP transport.** The existing VM effect returns
`nil` for transport failure, timeout, and non-2xx HTTP responses, so
`send_request_checked` maps all such failures to one generic error. It does
not expose status codes, response headers, redirect details, or cancellation.
The planned typed request/response transport is tracked in
[issue #1576](https://github.com/nulang-org/nulang/issues/1576).

The builder tests verify preflight and rejection without performing network
requests. The built-in `Http` effect remains host- and feature-dependent.

## Tests

```bash
nula test
```
