# Web Contract IR

Status: experimental implementation foundation.

Nulang Web routes are moving from runtime-only `Web.route` registrations toward a compiler-visible contract shared by validation, HTTP dispatch, deployment metadata, OpenAPI, generated clients, tests, adapters, and Nulang Cloud.

The central rule is that HTTP provenance is metadata, not a wrapper type. A parameter declared as `limit: Int from query` remains an ordinary `Int` to the typechecker and function body. The compiler records where the runtime obtains that value and lowers the metadata into a transport-neutral binding plan.

## Request-source parameter syntax

Top-level function parameters may declare a contextual request source after their optional type annotation:

```nulang
fn endpoint(
    id: Int from path("user_id"),
    limit: Int from query,
    trace: String from header("X-Trace"),
    session: String from cookie("session"),
    payload: String from body,
    title: String from form("title")
) -> String {
    // id is Int, limit is Int, and the remaining parameters are String here.
    "ok"
}
```

Supported sources are:

- `path` — a captured route path segment. A string override selects a differently named capture.
- `query` — one URL query value. Without an override, the parameter name is used.
- `header` — one HTTP header. Header lookup is case-insensitive.
- `cookie` — one cookie value.
- `body` — the whole request body. It does not accept a source-name override and only one handler parameter may bind the whole body.
- `form` — one `application/x-www-form-urlencoded` field.

`from` remains contextual rather than becoming a globally reserved keyword. Request-source bindings are not accepted on `using` parameters. Unknown request-source names are parse errors. An explicit path source must identify a path capture declared by the route.

This Web surface is experimental and is intentionally documented here rather than by modifying the Frozen Core parameter production in `spec/grammar.ebnf`. Promoting request-source syntax into a stable language tier requires the corresponding RFC/stability process.

## Additive Deployment IR v1 contract metadata

`nula build --web` keeps the deployment document at schema version 1 and adds optional/defaulted per-route metadata. Existing v1 consumers can ignore unknown fields, so a schema-version bump would create incompatibility without strengthening the current contract.

Route metadata includes the existing method/path/placement/artifact fields plus:

- `handler`: statically resolved handler name when available.
- `params`: route path parameter names and source-level types when known.
- `handler_params`: handler parameter names, types, Nulang reference capabilities, and optional request-source metadata.
- `bindings`: deterministic request-source-to-handler-slot bindings for `path`, `query`, `header`, `cookie`, `body`, and `form`. Each binding records `source_name`, `handler_param`, `handler_index`, and resolved source-level type.
- `response_type`: declared handler return type.
- `error_type`: declared typed-error type.
- `effects`: declared handler effect row.
- `reference_capability`: the existing Pony-style handler capability (`iso`, `ref`, `val`, etc.).

Bindings are ordered by handler slot rather than URL segment order. The runtime therefore stages arguments directly into the call ABI without inferring argument order from route text.

The package-level `capabilities` field remains unchanged for compatibility. It is not the same concept as an authorization/resource capability.

## Route path compatibility

The contract parser accepts legacy and contract-first path forms:

```text
/users/:id
/users/{id}
/users/{id: UserId}
```

Legacy `:id` routes remain backwards compatible. If a same-named unannotated handler parameter exists, the binding compiler can emit a direct path binding; otherwise legacy code may continue reading `Web.param("id")` during migration.

Brace syntax is contract-first. `{id}` and `{id: UserId}` require an appropriate handler binding. Typed route/handler mismatches remain contract diagnostics. Explicit source renaming is also supported:

```nulang
fn show(id: UserId from path("user_id")) -> Html { ... }

fn web_main() {
    perform Web.route("GET", "/users/{user_id: UserId}", show)
}
```

An explicit `from path("missing")` binding is rejected when `missing` is not a route capture.

## Package-wide extraction and validation

Contract extraction parses the package source tree and builds package-level analysis so a route in one file can retain handler metadata declared in another. Public `route(...)` and `route_method(...)` helpers lower to the same contract representation.

Bare helper recognition is scoped to modules importing `stdlib::web*`. A Web import in one file cannot reinterpret an unrelated user-defined `route()` call in another file as framework metadata.

Extraction is intentionally best effort so IDE/compiler tooling can inspect partial metadata. Artifact-producing and serving entry points use the stronger boundary:

```text
web::validation::compile_validated_contracts_from_tree(...)
```

It aggregates extraction and binding diagnostics, sorts/deduplicates them deterministically, and returns the original `ContractCompilation` only when the contract is valid. Package build/dev paths enter this boundary through `web::dispatch::compile_runtime_routes(...)`; downstream consumers should reuse the validated compilation rather than reparsing source independently.

The intended flow is:

```text
source tree
    -> best-effort contract extraction
    -> authoritative validation
    -> one validated ContractCompilation
       -> runtime route plans
       -> deployment IR
       -> OpenAPI / client generation
       -> tests / tooling
```

## Runtime binding and dispatch

Runtime registration remains method/path/module/function. Compiler metadata is attached as a sidecar so the low-level `Web.route` host effect does not need source-level type information.

A `RuntimeRoutePlan` precompiles:

- literal and path-parameter segments,
- deterministic request-input-to-handler-slot bindings,
- handler parameter count,
- whether the route is safe for complete direct invocation.

The request path is matched without its query component. HTTP request capture then derives the transport inputs once:

```text
HTTP request
    -> path captures
    -> query map
    -> case-insensitive headers
    -> cookie map
    -> whole text body
    -> URL-encoded form map when Content-Type permits it
    -> RequestBindingValues
    -> bind_request_arguments(...)
    -> BoundRouteArgument[]
    -> handler-call VM ABI
```

`Int`, `Float`, and `Bool` request values are decoded explicitly. `String`, untyped values, and custom/opaque identifier types remain string-backed until Web Contract IR carries a richer runtime representation.

The source-agnostic VM call seam validates that handler slots are complete, unique, and in range before execution. A complete compiler binding plan can therefore invoke a handler regardless of whether its arguments came from path, query, header, cookie, body, or form.

The transport-facing dispatch seam includes:

```text
compile_runtime_routes(...)                -> validate source contracts + attach plans
compile_runtime_routes_from_contracts(...) -> attach one validated contract set
match_route(...)                           -> compiled matching or legacy fallback
render_direct_request(...)                 -> complete multi-source typed invocation
render_direct_route(...)                   -> path-only compatibility API
```

`WebDevServer` constructs `RequestBindingValues` from the real HTTP request and selects `render_direct_request(...)` for complete plans. `Ok(None)` deliberately retains the legacy renderer for routes that still depend on ambient `Web.param(...)` behavior. Typed decode failures never silently fall back to legacy execution.

Both typed and legacy calls remain inside the existing request context during the migration period. This preserves current request helpers and response-cookie behavior while argument sourcing moves to compiler-owned bindings.

Network-level regression coverage compiles real Nulang handlers and route contracts, starts `WebDevServer` on an ephemeral TCP port, and verifies successful typed query decoding, `400 application/problem+json` decode failures, and legacy route fallback over the wire.

## Route precedence and HTTP method semantics

The contract validator rejects same-method route patterns that can match the
same request without one being strictly more specific (for example,
`/users/{first}` vs `/users/{second}`, or `/{a}/fixed` vs `/fixed/{b}`).
Literal segments narrow a parameter segment, so `/users/me` wins over
`/users/{id}` independent of registration order. Disjoint methods and
non-overlapping paths can coexist. Legacy registrations without a compiler
contract remain supported, but bypass compile-time ambiguity checks.

The development HTTP dispatcher uses the same relation when selecting a route,
and implements these method behaviors:

- Explicit `HEAD` routes take priority over fallback `GET` handlers.
- Otherwise `HEAD` invokes the matching `GET` handler but omits response bytes,
  retaining the representation's `Content-Length`.
- `OPTIONS` for a routed path without an explicit handler returns `204` with
  an `Allow` header; other unsupported methods return `405` with `Allow`.
- The `Allow` list includes implied `HEAD` for `GET` and the automatic
  `OPTIONS` method. Static files accept GET/HEAD and report other methods.
- The experimental SSE endpoint permits streaming only on GET, not on HEAD or
  write methods.

The typed response algebra and supervised request execution are separate
follow-up work; they are not implied by this protocol-level change.

## Decode failures and HTTP problems

Request decoding distinguishes client input failures from compiler/runtime invariant failures:

- missing required request input -> HTTP `400`
- invalid scalar request input -> HTTP `400`
- duplicate handler slots -> HTTP `500`
- invalid handler slots -> HTTP `500`
- incomplete binding plans -> HTTP `500`

The transport-neutral `RequestDecodeError` exposes stable machine-readable codes and an RFC-style problem document. HTTP adapts it to `application/problem+json`; transport-independent code does not parse display strings to classify errors.

Compiler/runtime binding defects deliberately remain server errors. They must not be presented as mistakes made by the caller.

## OpenAPI

OpenAPI 3.1 generation consumes the same `ContractCompilation` and binding IR; it does not reparse Nulang source.

- path bindings become required path parameters.
- query bindings become query parameters.
- header bindings become header parameters.
- cookie bindings become cookie parameters.
- all request bindings are retained in `x-nulang-request-bindings`.
- form bindings become an `application/x-www-form-urlencoded` OpenAPI `requestBody`, because the HTTP transport now defines that media type explicitly.
- raw whole-body bindings remain Nulang extensions until the request algebra defines their media type/schema semantics without guessing.
- Nulang response/error types, effects, placement, and reference capabilities remain extensions where OpenAPI has no exact equivalent yet.

This keeps runtime dispatch and generated API metadata on the same compiler-owned contract.

## Binding example

For:

```nulang
fn show(
    user: UserId from path,
    org: OrgId from path,
    expand: Bool from query,
    trace: String from header("X-Trace")
) -> Html { ... }
```

with route:

```text
GET /orgs/{org: OrgId}/users/{user: UserId}
```

the binding plan is equivalent to:

```json
[
  {
    "source": "path",
    "source_name": "user",
    "handler_param": "user",
    "handler_index": 0,
    "ty": "UserId"
  },
  {
    "source": "path",
    "source_name": "org",
    "handler_param": "org",
    "handler_index": 1,
    "ty": "OrgId"
  },
  {
    "source": "query",
    "source_name": "expand",
    "handler_param": "expand",
    "handler_index": 2,
    "ty": "Bool"
  },
  {
    "source": "header",
    "source_name": "X-Trace",
    "handler_param": "trace",
    "handler_index": 3,
    "ty": "String"
  }
]
```

## Architectural direction

The Web Contract IR is intended to be the stable seam between language semantics and transports:

```text
Nulang source
    -> typed/effect/capability analysis
    -> Web Contract IR
       -> authoritative validation
       -> runtime route plan
          -> HTTP runtime
          -> future request actor runtime
       -> deployment IR
       -> OpenAPI / client generation
       -> tests / tooling
       -> Nulang Cloud deployment metadata
       -> observability metadata
```

The runtime should ultimately execute each route under lightweight supervised request execution with structured cancellation and backpressure. Request-scoped dependencies should move from ambient state toward effect handlers. Stateful realtime/domain coordination should continue to use explicit persistent or virtual actors rather than making every model object an actor.

## Explicit JSON scalar request and response contracts (experimental)

The compiler accepts a scalar `from json` request binding alongside the
existing `from body` raw-string and `from form(...)` sources:

```nulang
fn counter(count: Int from json) -> Int { count }
```

The JSON binding validates the `Content-Type` header (application/json and
application/*+json) and parses a *single* JSON value into an exact `Int`,
`Float`, `Bool`, or `String` VM constant without coercion. Wrong media types
return HTTP 415; invalid JSON or mismatched scalar types return HTTP 400.
Aggregate DTO types, JSON arrays, record decoding, `Json[T]` and automatic
response serialization remain **unsupported** and cause compiler contract
diagnostics rather than being silently string-backed. JSON bindings cannot
share a route with form bindings; existing raw-body plus form compatibility
is preserved.

The explicit `Json` response marker and `Web.json`/`Web.route_json` effect
operations select application/json responses from compiler-owned route
metadata. The HTTP transport validates the serialized result before writing
a successful response. Invalid output returns a generic 500, never a
successful HTML response containing untrusted markup. OpenAPI emits real
application/json request/response media types only for these explicit
contracts, while preserving extensions for opaque/unmodeled body types.

### Flat typed JSON record DTOs (experimental follow-up)

For a statically declared record whose fields are only `Int`, `Float`,
`Bool`, and `String`, the compiler can lower `from json` into a
typed input schema and a real VM record argument:

```nulang
type CreateUser = { name: String, age: Int }

fn create_user(input: CreateUser from json) -> String {
    input.name
}
```

Request bodies must be JSON objects with the exact declared field names.
Missing fields, unknown fields (including privilege-escalation fields), wrong
scalar types, malformed input, and duplicate root-level keys are rejected
with a 400 problem response. This deliberately does not implement optional,
nested, collection, union, or user-defined custom scalar decoding.

The compiler carries record slot IDs into bytecode metadata so the HTTP
adapter materializes Nulang VM records using the same offsets the compiled
handler reads. OpenAPI exposes required properties and
`additionalProperties: false` rather than inventing a permissive schema.
Existing raw-body and primitive JSON binding behavior remains unchanged.

### Compiler-backed typed JSON responses (experimental)

Typed response serialization is opt-in via `Web.route_json_typed`:

```nulang
type UserView = { name: String, age: Int }

fn show() -> UserView {
    { name: "Ada", age: 42 }
}

fn register() {
    perform Web.route_json_typed("GET", "/user", show)
}
```

The compiler projects the handler's declared result into a `Json[T]`
**route contract**, where `T` is `Int`, `Float`, `Bool`, `String`,
or a flat record composed entirely of those primitives. The HTTP adapter
serializes the *actual* VM value; record fields use compiler-owned field IDs,
strings are JSON-escaped, and numeric/boolean values retain their JSON types.
The response is `application/json` and the OpenAPI response schema reflects
the same declared `T`. An unsupported return type is a compiler diagnostic;
mismatched runtime values fail closed with HTTP 500.

This route-contract notation does **not** add a generic source-level
`Json[T]` type or automatic DTO derivation across arbitrary Nulang types.
The original `Web.route_json`/`Web.json` path remains compatible: it still
expects already-serialized JSON and validates the resulting bytes.

Nested/optional records, arrays, user-defined custom scalars, statuses other
than 200, streaming, and response envelopes remain separate follow-up work.

## Remaining implementation priorities

1. Extend the experimental scalar JSON binding and `Json` marker into a typed request/response algebra (`Json[T]`, `Html`, bytes, streams, typed record schemas/media) so Contract IR and OpenAPI can represent full payloads without guessing.
2. Add richer request decoders for optional/default values, repeated query parameters/collections, binary bodies, transparent aliases, and opaque/domain types.
3. Execute requests under lightweight supervised request actors with structured cancellation/backpressure.
4. Replace ambient request-context dependency injection with effect handlers once compatibility coverage is sufficient.
5. Introduce authorization/resource capabilities separately from Nulang reference capabilities, including attenuation and capability-parameterized effects.
6. Unify HTTP, SSE, and WebSocket entry points over the same actor/effect/capability execution model.
