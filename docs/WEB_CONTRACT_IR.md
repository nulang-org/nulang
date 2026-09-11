# Web Contract IR

Status: experimental implementation foundation.

Nulang Web routes are moving from runtime-only `Web.route` registrations toward a compiler-visible contract that can be shared by the HTTP runtime, generated clients, tests, adapters, and Nulang Cloud.

## Deployment IR v2

`nula build --web` emits route metadata in `nulang-app.ir.json` with the existing method/path/placement/artifact fields plus:

- `handler`: statically resolved handler name when available.
- `params`: path parameter names and source-level types when known.
- `handler_params`: handler parameter names, types, and Nulang reference capabilities.
- `response_type`: declared handler return type.
- `error_type`: declared typed-error type.
- `effects`: declared handler effect row.
- `reference_capability`: the existing Pony-style handler capability (`iso`, `ref`, `val`, etc.).

The package-level `capabilities` field remains unchanged for compatibility. It is currently inferred partly from source/module usage and is **not** the same concept as an authorization/resource capability.

## Route path compatibility

The contract parser accepts all of the following as metadata:

```text
/users/:id
/users/{id}
/users/{id: UserId}
```

Legacy `:id` routes remain the executable runtime form in this implementation slice. A legacy parameter inherits the type of a same-named typed handler parameter when available.

For explicit typed contract syntax, a route/handler mismatch is retained as a contract diagnostic. For example, `{id: ExternalId}` paired with `fn handler(id: UserId)` is invalid contract metadata.

Recognizing brace syntax in the contract IR does not yet imply runtime argument binding. Runtime support should land only together with direct typed handler binding so Nulang does not create a second ambient `Web.param` convention.

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

1. Integrate contract validation with the typed compiler pass and make invalid typed route contracts hard diagnostics.
2. Bind path/query/body/header inputs directly to typed handler parameters; then enable `{name: Type}` in runtime routing.
3. Generate transport-independent response/error contracts and OpenAPI/client artifacts from the same IR.
4. Execute requests under lightweight supervised request actors with structured cancellation/backpressure.
5. Replace ambient request context and middleware dependency injection with effect handlers.
6. Introduce authorization/resource capabilities separately from Nulang reference capabilities, including capability attenuation and capability-parameterized effects.
7. Unify HTTP, SSE, and WebSocket entry points over the same actor/effect/capability execution model.
