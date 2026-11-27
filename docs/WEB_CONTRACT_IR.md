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

### Repeated URL-encoded inputs

HTTP request capture preserves every occurrence of query and URL-encoded form
fields, including names that become identical after percent decoding (for
example `title` and `%74itle`). A direct compiler-bound **scalar** `from query`
or `from form` parameter accepts exactly one occurrence. Missing inputs produce
HTTP 400 `missing_request_input`; repeated inputs produce HTTP 400
`ambiguous_request_input` rather than implicitly choosing the first or last.
Both errors use the existing `application/problem+json` response.

The legacy `parse_urlencoded` convenience API continues to select the final
value for compatibility; HTTP capture instead uses `parse_urlencoded_multi`.
Collection bindings and checkbox-list decoding are **not yet supported** by the
direct handler ABI. Internal UI-action envelopes and identities also reject
repeated fields rather than inheriting last-value behavior.


Both typed and legacy calls remain inside the existing request context during the migration period. This preserves current request helpers and response-cookie behavior while argument sourcing moves to compiler-owned bindings.

Network-level regression coverage compiles real Nulang handlers and route contracts, starts `WebDevServer` on an ephemeral TCP port, and verifies successful typed query decoding, `400 application/problem+json` decode failures, and legacy route fallback over the wire.

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

## Experimental form contracts

`web::forms::FormContract::from_route(&route)` projects the existing
compiler-owned request binding plan into an ordered list of form fields. It
does **not** reparse source or introduce new Frozen Core grammar. The
transport-neutral Rust API supports:

- Typed scalar `String`, `Int`, `Float`, and `Bool` field decoding;
  opaque/domain types remain string-backed, consistent with Web Contract IR.
- Explicit Rust-side `FormRule` constraints: text length and exact integer
  or finite float bounds. Incompatible, duplicate, and inverted rules are
  rejected when attached. These rules are **not** compiler source annotations.
- Aggregated, stable field error codes for missing, empty, repeated,
  incorrectly typed, and out-of-range inputs; submitted values are not
  included in error responses.
- A `422 application/problem+json` adapter through
  `http_problem::form_validation_problem_response`. This is opt-in for
  application handlers, not an automatic replacement for the transport's
  existing `400` request-decode behavior.
- An escaped, accessible `render_fields` HTML fragment with labels,
  required controls, constraints, preserved single values, and correlated
  `aria-describedby` error messages. Boolean fields use a select for
  explicit `true` / `false` rather than ambiguous checkbox omission.

The renderer deliberately emits **fields only**. Applications must supply
an authorized `<form>` wrapper, safe action URL, appropriate CSRF token,
submission/redirect handling, and any draft storage. All fields remain
required because optional request bindings and typed collection values are
not yet implemented. Full Nulang-language forms, optional defaults,
multipart, conditional rules, and browser hydration require later work.

Example in Rust host/tooling code:

```rust
let form = FormContract::from_route(&validated_route)?
    .with_rules("company", vec![FormRule::MinLength(2)])?;
let result = form.validate(&http_capture.form);
```

This uses the same `RouteContract` produced by the package compilation and
the multi-value HTTP capture introduced in the request binding layer.

## Opt-in protected form route execution

The experimental Rust-side HTTP server API
`WebDevServer::bind_runtime_with_form_guards` accepts an explicit
`GuardedFormRoute` for each state-changing form endpoint. At **server startup**
it checks the form's method, path, handler-binding fields, and constraint
metadata against the attached compiler-produced `RuntimeRoutePlan`. Unknown,
duplicate, mismatched, incomplete or unsafe-method configurations fail before
the server accepts connections. Ordinary `bind_runtime` routes remain
**unprotected unless explicitly configured**.

For registered guarded routes, HTTP processing is ordered as follows:

1. Parse the request with existing size and media-type limits, including
   preserving repeated URL-encoded form fields.
2. Reject invalid UI-action envelopes.
3. Verify the unique `__nulang_csrf` form field against a keyed BLAKE3 token
   derived from the *authenticated session cookie value*, HTTP method and
   compiler route path. Missing/ambiguous cookies and tokens fail closed with
   HTTP 403. Compare the complete canonical 64-character token.
4. Validate all form fields using `FormContract` **before invoking the VM**.
   Semantic failures become HTTP 422 `application/problem+json` with
   machine-readable field errors. The 403 and 422 responses are `no-store`.
5. Invoke the same compiler-bound request handler as before on success.

The caller must establish and authenticate sessions independently, provide a
cryptographically random 32-byte secret via secret management, issue an
HTTPS `Secure; HttpOnly; Path=/` session cookie **without** a Domain attribute,
using a `__Host-` name, and insert the generated token into a hidden field.
`CsrfPolicy::token_for_session(session, method, path)` can be called by
authorized server-side rendering code for that authenticated session. A CSRF
token is **not** authentication and it does not validate session legitimacy.
Protect token-bearing pages from cross-origin reads and XSS. Retire/rotate
tokens on session invalidation or key rotation as appropriate.

No automatic CSRF protection is currently installed in `nula dev` or the
ordinary `WebDevServer::bind_runtime` entry point. Compiler deployment
metadata listing `csrf` is not evidence of runtime enforcement. The inert
`Middleware::Csrf` placeholder has been removed, rather than claiming an
unenforced response-only middleware can prevent request execution.

Compiled handler invocation for optional/default values and repeated typed
collections remains deferred because the current VM request-binding ABI only
supports required scalar constants. Host-side schema validation for optional
and repeated inputs is documented below. This work does not change Nulang
source grammar; applications still explicitly register guarded form routes.

## Host-side optional and repeated form fields (experimental)

`FormContract::with_shape(field, FormFieldShape::Optional)` and
`FormFieldShape::Repeated { min_items, max_items }` extend form **validation
metadata**, without implying support in the compiled Nulang handler ABI.
They work on the retained multi-value URL-encoded HTTP capture:

- Optional scalar inputs decode when provided; an absent or single blank
  optional input is absent from `ValidatedForm.values`, never coerced to an
  empty string or a fake `None` VM variant. Two occurrences (including encoded
  aliases) remain invalid for a scalar.
- Repeated fields decode each occurrence into the declared scalar element type
  and preserve order in `ValidatedForm.repeated`. They validate each element,
  apply existing scalar constraints to every value, and enforce explicit
  `min_items` / `max_items` bounds (maximum 64). A zero minimum permits an
  explicitly empty collection.
- The HTML field-fragment renderer distinguishes optional from required
  controls and renders repeated fields as a labeled fieldset of indexed
  controls. All submitted values and attribute content remain escaped; errors
  identify the field without echoing submitted content.
- The legacy `GuardedFormRoute::new` **still rejects optional and repeated
  field shapes** at startup. The separate `GuardedFormRoute::new_typed` API
  accepts compiler-verified, primitive `Option[T]` and `[T]` handler fields
  when the in-memory compiler field layout is available. See below.
- A structured handler annotation, such as `Option[String]` or `List[Int]`,
  cannot be staged by the scalar request binder. Such a binding fails as a
  server-side unsupported-binding-type error instead of silently delivering
  `Constant::String` with an incompatible VM type. Simple nominal identifiers
  remain string-backed for backwards compatibility; their actual runtime
  representation still requires separate type-system guarantees.

Also, client-facing typed decoding errors no longer embed the raw submitted
value in the problem message. This matters for sensitive form fields,
query parameters, and logs.

The scalar-only binder is still unchanged and rejects structured handler
annotations rather than coercing them to JSON strings or selecting a repeated
field's last value. Native variant/array VM materialization is an explicitly
opt-in, compiler-owned path described below.

## Compiler-owned typed form handler ABI (opt-in)

`compile_mir_with_web_field_layout` returns `CompiledWebModule`, which couples
the emitted `CodeModule` with the **same compilation's** module-wide record
field IDs and MIR handler parameter types. The layout is an opaque, in-memory
sidecar, not another serialized bytecode field: the frozen `.nbc` format and
existing `compile_mir` output are unchanged. Loading a bytecode artifact
without the sidecar **does not** authorize typed form execution.

The host may use `GuardedFormRoute::new_typed(form, csrf, compiled)`, then
register it via `WebDevServer::bind_runtime_with_form_guards`. Registration
fails before listening if the compiler-produced module differs from the
registered handler module, if the binding plan is missing/incomplete or uses
non-form sources, if mutable contract types disagree with actual MIR parameter
types, or if form shapes do not exactly match the handler's type algebra.

Supported **only in the explicitly typed path**:

- `String`, `Int`, `Float` and `Bool` required scalar parameters;
- `Option[T]` where `T` is one of those four primitives, represented in the
  resolved compiler contract as `Some T | None`. Absent/blank optional fields
  become the nullary `None` constructor; present values become a **real
  compiler-layout** `{ ctor: "Some", payload: value }` record, with no guessed
  field IDs;
- `[T]` where `T` is one of those primitives. Each retained form occurrence
  is decoded, validated, and stored in submission order in a real VM-allocated
  array. The form metadata's existing 64-item bound still applies.

The trampoline checks values **against the actual compiled MIR parameter
types** before allocating variants or arrays. It rejects mixed element types,
non-finite floats, integers outside the 48-bit VM payload range, invalid
argument banks, and missing layout metadata. VM allocation and container
retention use the existing `RecMk`/`RecS` and `ArrAlloc`/`ArrStore`
opcodes. No JSON-as-string or last-value fallback is permitted.

The HTTP execution order remains: authenticated-session cookie **and**
session-bound CSRF token (403), then semantic form validation (422), then the
type-checked VM handler. Unregistered and legacy guarded routes retain their
prior execution semantics. Session authentication, secure cookie issuance,
CSP/XSS defenses, and exposing the typed compiler sidecar through the
production build pipeline remain the host application's responsibilities.
This is **not** automatic support in `nula dev`, nor support for arbitrary
generic/nested collection types or multipart form bodies.

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

## Remaining implementation priorities

1. Define an explicit request/response algebra (`Json[T]`, `Html`, bytes, streams, typed bodies/media) so Contract IR and OpenAPI can represent payloads without guessing.
2. Add richer request decoders for optional/default values, repeated query parameters/collections, binary bodies, transparent aliases, and opaque/domain types.
3. Execute requests under lightweight supervised request actors with structured cancellation/backpressure.
4. Replace ambient request-context dependency injection with effect handlers once compatibility coverage is sufficient.
5. Introduce authorization/resource capabilities separately from Nulang reference capabilities, including attenuation and capability-parameterized effects.
6. Unify HTTP, SSE, and WebSocket entry points over the same actor/effect/capability execution model.
