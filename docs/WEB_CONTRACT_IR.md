# Web Contract IR

Status: experimental implementation foundation.

Nulang Web routes are moving from runtime-only `Web.route` registrations toward a compiler-visible contract that can be shared by the HTTP runtime, generated clients, tests, adapters, and Nulang Cloud.

## Deployment IR v2

`nula build --web` emits route metadata in `nulang-app.ir.json` with the existing method/path/placement/artifact fields plus:

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

Recognizing brace syntax and emitting bindings does not yet imply that the current HTTP runtime invokes handlers with those arguments. The runtime registration object currently stores only method, path, module, and function index. Runtime support should land together with a place to preserve the compiled binding plan so Nulang does not re-parse route conventions at dispatch time.

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
       -> HTTP runtime
       -> OpenAPI / client generation
       -> test harness
       -> Nulang Cloud deployment metadata
       -> observability metadata
```

The runtime should ultimately execute a route as an ephemeral supervised request actor. Request-scoped dependencies should resolve through effect handlers rather than ambient thread-local state. Stateful realtime/domain coordination should use explicit persistent or virtual actors rather than making every model object an actor.

## Next implementation slices

1. Integrate contract/binding validation with the typed compiler pass and make invalid contract-first routes hard diagnostics.
2. Preserve the compiled binding plan on runtime route registrations and stage typed path arguments into the VM call ABI; then enable `{name: Type}` runtime routing.
3. Extend binding sources to query/body/header inputs and generate transport-independent response/error contracts and OpenAPI/client artifacts from the same IR.
4. Execute requests under lightweight supervised request actors with structured cancellation/backpressure.
5. Replace ambient request context and middleware dependency injection with effect handlers.
6. Introduce authorization/resource capabilities separately from Nulang reference capabilities, including capability attenuation and capability-parameterized effects.
7. Unify HTTP, SSE, and WebSocket entry points over the same actor/effect/capability execution model.
