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

`send_request` uses the built-in `Http` effect and is only available under a
runtime host, not the standalone VM — the package tests therefore cover
only the pure builder and encoding parts.

## Tests

```bash
nula test
```
