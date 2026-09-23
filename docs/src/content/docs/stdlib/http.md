---
title: "Http Effect"
description: "Built-in Http effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Http"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Http Effect

The `Http` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Http.get` | `get(url: String) -> String` | Perform an HTTP GET request to `url` and return the response body as a string on success, nil on error. Requires the `http-client` or `ai-runtime` feature for the reqwest provider. |
| `Http.post` | `post(url: String, body: String) -> String` | Perform an HTTP POST request to `url` with a JSON body and return the response body as a string on success, nil on error. Requires the `http-client` or `ai-runtime` feature for the reqwest provider. |
| `Http.serve` | `serve(port: Int, handler: fn(String) -> String) -> Int` | Start an HTTP/1.1 server on `port`. For each request, calls `handler(body)` and returns the result as the response body with status 200. Returns the actual bound port. |

_Implementation site: Standalone VM_
