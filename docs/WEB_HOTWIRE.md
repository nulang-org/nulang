# Hotwire / Turbo Stream interoperability (experimental)

Nulang Web can emit standard Turbo Stream HTML for the official
[Hotwire Turbo](https://turbo.hotwired.dev/) browser runtime. This is
a transport and HTML interoperability layer, **not** a new JavaScript
DOM diffing engine or a replacement for the existing Nulang UI protocol.

## Nulang API

Import `stdlib::web::turbo` and use a rendered `Html` value:

```nulang
import stdlib::web::turbo

fn created() {
    append("todos", perform Web.text("Tea & toast"))
}

app "todos" {
    route "POST" "/todos" -> created
}
```

The response is `200 OK` with
`Content-Type: text/vnd.turbo-stream.html; charset=utf-8` and the body:

```html
<turbo-stream action="append" target="todos"><template>Tea &amp; toast</template></turbo-stream>
```

Available helper functions: `append`, `prepend`, `replace`, `update`,
`before`, `after`, `remove`, `refresh`. They use the host effect
`Web.turbo_stream(action, target, content)` and the existing
`Html` abstraction. A removal omits the `<template>`; a refresh
omits both target and template. Only DOM **IDs** are currently supported,
not selector-based `targets` or morph options.

The server only emits the standard wire format; applications must load
the official Turbo JavaScript client. Server-side Nulang actions do not
automatically enable Turbo navigation, Turbo Frames or Stimulus.

## Safety and delivery behavior

- Target IDs are intentionally restricted to ASCII letters, digits,
  underscores, hyphens, periods and colons, starting with a letter,
  digit or underscore (maximum 128 bytes). Invalid IDs or action names
  fail closed.
- Pass `Html` from `Web.text`, `Web.html` or safe components. Never
  construct a stream from untrusted `Web.raw` content; raw HTML remains
  explicitly trusted and can break out of a `<template>`.
- The Turbo response media type is scoped to a single HTTP request.
  Normal HTML routes keep their normal content type. The Nulang browser
  runtime script is **not** injected into Turbo Stream responses.
- This slice does not introduce automatic Turbo content negotiation or
  an SSE-to-Turbo subscription bridge. `accepts_turbo_stream` is a
  low-level parser for future adapters, not an HTTP admission policy.
- Full typed response semantics, redirects, application security and
  production-ready serving are separate active work. `nula dev`
  remains a development server; do not treat this as production hardening.

## Testing

The Rust renderer has focused serializer/target-injection tests, and the
HTTP integration tests cover correct MIME selection, escaped content,
and no content-type leakage to subsequent HTML requests.

```bash
cargo test --locked --no-default-features --lib web::turbo::tests
cargo test --locked --no-default-features --lib turbo_mime_is_only_selected
cargo test --locked --no-default-features --test web_http_server_contracts
```
