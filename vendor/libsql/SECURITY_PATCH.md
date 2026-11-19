# libsql 0.10.0-pre.4 security overlay

Source: the immutable published crates.io package `libsql` version `0.10.0-pre.4`, including the original source tree, without remote transport behavior edits. Original crates.io package checksum (from NuLang parent Cargo.lock): `80e6206c72726b16e7ec9d68c3661d50ebe70947f8e60e0ebb4388d327e1fc2b`.

Only change to the published crate's Cargo manifest: remove Hyper 0.14 `http2` from its optional Hyper dependency feature list; `client`, `http1`, `stream`, and `runtime` remain. This prevents the unpatched `h2 0.3.27` package from entering NuLang's resolved graph, rather than suppressing RUSTSEC-2026-0258 with an audit exception. The feature is not required by Hrana's HTTP/1.1 request/response exchange. The `hyper-tls` connector keeps `https_only(true)`, normal platform CA and hostname verification; no TLS bypass or plaintext fallback is permitted.

The published crate must be replaced by a newer upstream HTTPS client once the upstream Hyper 0.14 dependency is retired. If the remote server requires HTTP/2, an HTTP/1.1-only client may be incompatible; verify with a disposable authenticated Turso database before production use.

Do not edit `src/` or widen the patch. The original crates.io checksum is recorded in the upstream `Cargo.lock` history. Cargo's `[patch.crates-io]` sources are local and reviewable.
