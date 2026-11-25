# Nulang identity and authorization plan (2026-11-20)

## Current state and no-merge safety boundary

- `packages/nulang-auth/src/lib.nula` on `main` is a development-only demo with an arbitrary-user `login(user)` API and predictable sequential session identifiers. Do **not** treat it as a production identity provider.
- Issue [#2178](https://github.com/nulang-org/nulang/issues/2178) is the production auth/session and CSRF release gate.
- Draft [#2517](https://github.com/nulang-org/nulang/pull/2517) consolidates demo-auth deployment rejection, hardened sessions, HTTP ingress, CSRF checks and audited dependencies. Draft [#2630](https://github.com/nulang-org/nulang/pull/2630) reconciles much of it with host lifecycle changes. Do not independently merge overlapping versions.
- Draft [#2606](https://github.com/nulang-org/nulang/pull/2606) introduces an explicitly guarded verified-tenant request context and PostgreSQL RLS tests.
- `@nulang/permissions` is intentionally a *pure policy decision helper*, not a new trusted runtime, login provider, or substitute for those security gates.

## Module boundaries

| Component | Ownership | First usable contract |
| --- | --- | --- |
| `@nulang/auth` | Verified identity + server-side session | Sign in only after external/authentic credential verification; revoke/rotate session |
| `@nulang/permissions` | Local application policy | Deterministic tenant-scoped RBAC; explicit deny; deny by default |
| Request/runtime gate | Authenticated request/actor authorization | Server-established principal + tenant; deny before dispatch/effect |
| DB isolation | Resource tenant correctness | Scoped transactions and PostgreSQL RLS where available |
| `@nulang/permissions/openfga` (future) | Relationship graph | Optional check adapter with pinned model/version and explicit consistency contract |
| Nulang Cloud `nlc-identity` | Managed identity + federation | Reuse existing OAuth/OIDC, RBAC, OpenFGA interfaces; do not duplicate service |

**Important:** a language-level authority grant (`src/authority.rs`, e.g. file or secret access) is not an application user permission. Both must be satisfied; neither can manufacture the other.

## Permissively licensed inspiration, ranked

1. **Keycloak (Apache-2.0):** identity brokering, OIDC client/realm boundaries, organization-aware delegated admin, fine-grained org permissions, administrative audit events and credentials/session revocation. Borrow *design* before considering an optional Keycloak OIDC adapter. Keycloak 26.7 adds organization-level fine-grained admin permissions. Do not replicate its full IAM server.
2. **Better Auth (MIT):** composable providers and plugins, ergonomic organization lifecycle and account management; design a stable provider/session adapter API without tying Nulang to JS.
3. **Casbin (Apache-2.0):** fast, in-process, tenant-domain RBAC and role mappings; start with exact match and add role inheritance only after cycle and ambiguity tests.
4. **Cedar (Apache-2.0):** typed policy schema, validation, consistent explicit-deny precedence, policy diagnostics; candidate for later schema-checked native policy syntax.
5. **OpenFGA (Apache-2.0) / SpiceDB (Apache-2.0):** external relationship authorization when access depends on nested organizations, shared resources and cross-organization delegation. Prefer reusing Nulang Cloud's existing OpenFGA over operating both.
6. **OPA (Apache-2.0):** centralized policy authoring, policy bundles, decision logs and test tooling; strong choice for deployment/compliance policy, not required for every end-user request.
7. **Ory Kratos + Hydra (Apache-2.0 core; some vendor offerings under separate terms):** API-first identity lifecycle, recovery, OAuth/OIDC authorization server and machine identity; integrate before implementing a full protocol server.
8. **SPIFFE/SPIRE (Apache-2.0):** workload attestation, short-lived SVIDs, service-to-service authentication for Nulang Cloud; not a replacement for user authorization.

**Licensing caution:** ZITADEL's main server is AGPL-3.0 in current versions with narrower Apache/MIT exceptions. Do not import server code under the assumption that ZITADEL as a whole is permissively licensed. Check individual component licenses, dependency notices, and bundled assets before any code reuse. Authentik likewise needs file/component-level license review; federation via standard protocols does not require embedding provider code.

Sources: [Keycloak](https://github.com/keycloak/keycloak), [Keycloak org FGAP](https://www.keycloak.org/2026/07/keycloak-2670-released), [Better Auth](https://github.com/better-auth/better-auth), [Casbin](https://github.com/apache/casbin), [Cedar](https://github.com/cedar-policy/cedar), [OpenFGA](https://github.com/openfga/openfga), [SpiceDB](https://github.com/authzed/spicedb), [OPA](https://github.com/open-policy-agent/opa), [Ory Hydra](https://github.com/ory/hydra), [SPIRE](https://github.com/spiffe/spire), [ZITADEL licensing](https://github.com/zitadel/zitadel/blob/main/LICENSING.md).

## Shipping sequence and release gates

### P0 — consolidated security baseline

- Select a canonical **single** integration path for #2517/#2630, resolve dependency security issues without downgrades, and run exact-head CI plus negative real HTTP security tests.
- Production deploy must reject demo `login(user)` and no-op auth/CSRF behavior. HTTP form and API mutations must reject forged origins/tokens before side effects.
- No production approval based only on unit tests or checks on an ancestor branch.

### P1 — verified identity and tenant admission

- Introduce a trusted `VerifiedPrincipal` / `VerifiedTenantContext` supplied by the host only after session verification; no public arbitrary-principal constructor.
- Configure an OIDC relying-party adapter for Keycloak or another compatible IdP. Validate discovery issuer, ID-token signature/JWKS, audience, nonce/state, PKCE, expiry, and key rotation.
- Use CSPRNG-backed session IDs, durable revocation, secure `__Host-` cookies, CSRF and rate-limit enforcement. Verify fresh org membership for sensitive operations.
- Adopt tested request admission/RLS components from #2606 rather than reimplementing them.

### P2 — policy enforcement, not just policy evaluation

- Bind `@nulang/permissions` to host-owned principal, tenant and resource identity; evaluate at HTTP, actor and job effect boundaries. Callers may not supply their own role list.
- Define policy revision/version + audit event schema, forbid silent fallback if an external authorization service is unavailable.
- Test cross-tenant reads and writes, membership revocation, owner delegation, deny-vs-allow, concurrent policy changes, and failure/retry paths.

### P3 — enterprise adoption without needless infrastructure

- Keycloak OIDC as an optional configured IdP; support issuer-specific org role/group claim mapping **only** with documented trust, tenant binding and claim freshness.
- Fine-grained delegated organization admin and audit/event retention; add multi-org IdP brokerage only for a verified customer requirement.
- Optional OpenFGA adapter for hierarchical resources. Define fail-closed behavior and consistency/caching (authorization consistency matters after revocation).
- Reserve SCIM provisioning, SAML and service-to-service SPIFFE/SPIRE for Nulang Cloud and enterprise integrations, with compatibility/security suites.

## Risk acceptance

This initial branch is safe to review independently **because it does not modify auth sessions, HTTP handlers, cookie/session parsing, or production deployment behavior**. It does not make existing demo auth safe. Do not ship a SaaS starter claiming end-to-end production identity security merely because `@nulang/permissions` tests pass.
