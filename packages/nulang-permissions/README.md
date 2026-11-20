# nulang-permissions (experimental)

A pure, **deny-by-default** tenant-scoped RBAC decision helper for Nulang applications. No network service, database, secrets, or runtime authority is required to evaluate a policy.

**This is not an authentication implementation or a production authorization enforcement point.** It does not establish a user identity, verify a session, load tenant membership, prevent stale permissions, or enforce protected request/DB/worker boundaries.

## Local usage

```nulang
import lib

let memberships = [role_assignment("acme", "user-17", "billing")];
let policies = [
  permission_rule("acme", "billing", "invoice", "read", "allow"),
  permission_rule("acme", "billing", "invoice", "delete", "deny")
];

let may_read = is_allowed(
  "acme", "user-17", "acme", "invoice", "read", memberships, policies
);
```

This example demonstrates the policy function only. In a real application, all of the following must be supplied and enforced by trusted server components:

1. The principal and active tenant from a verified session or identity-provider token with checked issuer, audience, expiry, signature, and current membership.
2. Memberships from an authoritative tenant-scoped store, **never** from a request body, cookie claim alone, or browser-generated role list.
3. Resource tenant from the canonical database record, **never** from the user-supplied resource identifier or a form field.
4. The decision checked before every protected action, including HTTP requests, background jobs, actor messages, and database operations.
5. Database-level tenant isolation / RLS as defense in depth, plus audited policy updates and fresh membership data.

If these preconditions are not met, this library can be misused to return `true` for fabricated roles. It is deliberately independent of `@nulang/auth` while the production auth/CSRF stack is under review.

## Semantics

- Exact tenant, subject, role, resource, and action matching; no implicit wildcard, owner bypass, or role inheritance.
- Empty identities/resources/actions, unknown roles and missing policies => **deny**.
- `deny` always overrides `allow`, including across multiple roles.
- Unknown policy outcomes also deny if the rule matches.
- Policies from a different tenant never affect this tenant.
- The caller must refresh assignments and rules after revocation; there is no internal cache.

`role_assignment(tenant, subject, role)` creates a record. `permission_rule(tenant, role, resource, action, outcome)` creates a record, where outcome is `allow` or `deny`. `is_allowed(tenant, subject, resource_tenant, resource, action, assignments, rules)` returns a boolean.

No role storage, trust delegation, UI, remote OpenFGA connector, or policy language is included in this initial package.

## Validation

From this directory with a compatible Nulang compiler:

```bash
nulang nula test
```

The test file checks legitimate allow paths and cross-tenant, impersonation, invalid-policy, revoked-membership, malformed-rule, and implicit-wildcard rejection.

## Next steps

Integrate with a **verified** tenant resolver (see draft Nulang PR #2606), enforce decisions before handler dispatch, add a shared policy-store interface with revisions, and test database/HTTP E2E tenant boundaries before making production-security claims.
