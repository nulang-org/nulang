---
title: "Web Effect"
description: "Built-in Web effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Web"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Web Effect

The `Web` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Web.route` | `route(method: String, path: String, handler: fn() -> Html) -> Unit` | Register a request handler for the given method and path. |
| `Web.html` | `html(tag: String, attrs: [(String, Html)], children: [Html]) -> Html` | Construct an HTML element from a tag, attributes, and children. |
| `Web.text` | `text(s: String) -> Html` | Escape a string and wrap it as an Html text node. |
| `Web.raw` | `raw(s: String) -> Html` | Wrap a raw string as an Html value without escaping. |
| `Web.redirect` | `redirect(url: String) -> Html` | Produce a redirect response to the given URL. |
| `Web.serve_static` | `serve_static(path: String) -> Html` | Serve the contents of a static file as an HTML response. |
| `Web.read_body` | `read_body() -> String` | Read the body of the current HTTP request. |
| `Web.param` | `param(name: String) -> String` | Get a route parameter from the current HTTP request. |
| `Web.header` | `header(name: String) -> String` | Get a request header from the current HTTP request. |
| `Web.cookie` | `cookie(name: String) -> String` | Get a cookie value from the current HTTP request by name. |
| `Web.set_cookie` | `set_cookie(name: String, value: String) -> Unit` | Add a Set-Cookie header to the current HTTP response. |
| `Web.clear_cookie` | `clear_cookie(name: String) -> Unit` | Add a Set-Cookie header that clears the named cookie. |

_Implementation site: Runtime Host_
