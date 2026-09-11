# Web Contract IR

Status: experimental implementation foundation.

Nulang Web routes are moving from runtime-only `Web.route` registrations toward a compiler-visible contract that can be shared by the HTTP runtime, generated clients, tests, adapters, and Nulang Cloud.

## Additive Deployment IR v1 contract metadata

`nula build --web` keeps the deployment document at schema version 1 and adds optional/defaulted per-route metadata. This is deliberately an additive extension: existing v1 consumers can ignore unknown fields, so a schema-version bump would create incompatibility without buying stronger guarantees.

The route metadata in `nulang-app.ir.json` now includes the existing method/path/placement/artifact fields plus:

- `handler`: statically resolved handler name when available.
- `params`: path parameter names and source-level types when known.
- `handler_params`: handler parameter names, types, and Nulang reference capabilities.
- `bindings`: deterministic request-source to handler-slot bindings. The first implementation supports `path` bindings and records `source_name`, `handler_param`, `handler_index`, and the resolved source-level type.
- `response_type`: declared handler return type.
- `error_type`: declared typed-error type.
- `effects`: declared handler effect row.
- `reference_capability`: the existing Pony-style handler capability (`iso`, `ref`, `val`, etc.).

Bindings are ordered by handler slot rather than URL segment order. That makes the IR directly usable by a runtime call ABI and allows a handler signature to order parameters independently from the path.

The package-level `capabilities` field remains unchanged for compatibility. It is currently inferred partly from source/module usage and is **not** the same concept as an authorization/resource capability.

## Route path compatibility

The contract parser accepts all of the following as metadata:

```text
/users/:id
/users/{id}
/users/{id: UserId}
```

Legacy `:id` routes remain backwards compatible. If a same-named handler parameter exists, the compiler emits a direct binding for it; otherwise legacy code may continue to read the value through `Web.param("id")` during the migration period.

Brace syntax is contract-first. `{id}` and `{id: UserId}` require a same-named handler parameter in the binding compiler. A typed route/handler mismatch is also retained as a contract diagnostic. For example, `{id: ExternalId}` paired with `fn handler(id: UserId)` is invalid contract metadata.

## Package-wide extraction

Contract extraction parses the package source tree and builds one package-level analysis module so a route in one file can retain handler metadata declared in another. Public `route(...)` and `route_method(...)` helper calls are also lowered to the same route contract representation.

Bare helper-call recognition is scoped to the source module that imports `stdlib::web*`. A web import in one file therefore cannot reinterpret an unrelated user-defined `route()` call in another file as framework metadata.

## Runtime binding and dispatch

The runtime bridge is implemented as a sidecar rather than adding source-level type metadata to the low-level `Web.route` host effect. VM registration remains method/path/module/function. Package analysis then joins the matching compiler contract onto each collected route.

A `RuntimeRoutePlan` precompiles:

- literal and path-parameter segments for request matching,
- the deterministic request-input to handler-slot binding plan,
- handler parameter count,
- whether the compiler proved the handler is safe for a complete direct call.

Contract-backed request matching therefore does not re-parse `{id: Type}` or rediscover parameter ordering on every request. Routes without a compiler plan keep the legacy `:name` matcher.

Direct path arguments are staged into VM registers `r0..rN`, followed by a non-capturing handler closure and a `ClosureCall` with the real argument count. Primitive `Int`, `Float`, and `Bool` path values are decoded explicitly. `String`, untyped values, and custom/opaque identifier types remain string-backed until the contract IR carries an explicit runtime representation for aliases and opaque types.

A route is marked `direct_call` only when every declared handler parameter and every route path parameter has a compiler-produced binding. Legacy handlers that still depend on ambient `Web.param(...)` therefore do not silently switch execution models.

The transport-facing `web::dispatch` seam exposes three operations:

```text
compile_runtime_routes(...)  -> validate package contracts + attach plans
match_route(...)             -> match using compiled plan or legacy fallback
render_direct_route(...)     -> invoke only fully bound typed handlers
```

HTTP remains responsible for request lifecycle, headers, cookies, cancellation, and the existing request context during migration. The final dev-server call-site wiring should wrap both legacy and typed calls in that request lifecycle while selecting `render_direct_route(...)` only for `direct_call` plans.

## Binding example

For:

```text
GET /orgs/{org: OrgId}/users/{user: UserId}
fn show(user: UserId, org: OrgId) -> Html
```

the compiler can emit the equivalent of:

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
  }
]
```

The runtime therefore does not need to infer parameter order from the URL.

## Architectural direction

The contract IR is intended to become the stable seam between language semantics and transports:

```text
Nulang source
    -> typed/effect/capability analysis
    -> Web Contract IR
       -> validated runtime route plan
          -> HTTP runtime
          -> future request actor runtime
       -> OpenAPI / client generation
       -> test harness
       -> Nulang Cloud deployment metadata
       -> observability metadata
```

The runtime should ultimately execute a route as an ephemeral supervised request actor. Request-scoped dependencies should resolve through effect handlers rather than ambient thread-local state. Stateful realtime/domain coordination should use explicit persistent or virtual actors rather than making every model object an actor.

## Next implementation slices

1. Wire `compile_runtime_routes`, compiled matching, and `render_direct_route` into `nula dev` / the existing `WebDevServer`, preserving the legacy request context around execution during migration.
2. Make contract/binding diagnostics hard `nula build --web` and `nula dev` failures before serving or emitting deployment artifacts.
3. Extend binding sources to query/body/header inputs and generate transport-independent response/error contracts and OpenAPI/client artifacts from the same IR.
4. Execute requests under lightweight supervised request actors with structured cancellation/backpressure.
5. Replace ambient request context and middleware dependency injection with effect handlers.
6. Introduce authorization/resource capabilities separately from Nulang reference capabilities, including capability attenuation and capability-parameterized effects.
7. Unify HTTP, SSE, and WebSocket entry points over the same actor/effect/capability execution model.
