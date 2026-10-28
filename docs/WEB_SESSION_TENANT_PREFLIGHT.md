# Cookie-session tenant-authorized Web preflight (experimental)

This draft extends the internal typed request preflight (#2336) and
CSRF synchronizer (#2340). It does not enable HTTP mutation routes, create
sessions, provision PostgreSQL roles, or ship a production authorization
service. Review it as a stacked PR, not an independent main merge.

## Verified library sequence

The web::session_preflight::authorized_session_preflight function:

1. Extracts exactly one opaque __Host-nulang_session cookie from the original
   header list, rejecting duplicate Cookie headers, duplicate session-cookie
   names and invalid handles. This cookie is not itself proof of identity.
2. Resolves the handle through a trusted server-side SessionLookup adapter.
   Rejects missing, expired or malformed principal/tenant sessions, including
   mismatches between the session's tenant and resource tenant.
3. Extracts all raw-form and header CSRF candidates using #2340's
   duplicate-preserving extractor; requires exactly one valid token from
   the authenticated, server-owned session.
4. Calls ResourceAuthorization::can_preflight for this principal, tenant and
   exact resource. Mere tenant membership does not prove resource access.
5. Derives typed query, form, header, cookie and body values from the very same
   raw request already admitted, returning only client-safe scalar diagnostics.
   No handler executes and no state is written.

The principal and tenant come only from the server-owned session, never from
X-Tenant-Id, query strings or form fields. Route captures are still untrusted
values until decoded and authorized.

## Hard integration prerequisites

- The audited Host, Origin, Referer and Fetch Metadata admission rules MUST
  reject unsuitable requests before this function is called. These protections
  are not in this module.
- The caller MUST provision a cryptographically strong, revocable session
  handle and authenticate against a server-side session store. Use Secure,
  HttpOnly, Path=/ and appropriate SameSite cookie attributes, no Domain,
  TLS, rotation on login/privilege change, expiration and logout revocation.
  None of those lifecycle features is implemented here.
- TrustedSession is a data container for a verified session adapter; its
  constructor is NOT an authentication primitive. Do not populate its fields
  from unchecked browser claims or unsafely decoded tokens.
- The resource-policy implementation MUST prove actual resource ownership
  in an authoritative non-BYPASSRLS tenant-scoped database. Tests in this
  draft use local policy fixtures only, not PostgreSQL or production sessions.
- Never directly return or serialize AdmissionError or internal decode errors.
  All Denied outcomes should map to generic 403, Unavailable to generic 503,
  and InvalidPlan to generic 500. Never log cookies or CSRF tokens.
- All security admission, domain validation and concurrency/version checks
  MUST run again in the authoritative transaction on final mutation.
  A preflight is never permission to write.
- No HTTP endpoint is wired to this primitive. Keep the disposable JWT-to-
  PostgreSQL reference route GET-only until separately audited integration.

## Verification

New tests/web_session_tenant_preflight.rs cases cover successful admitted
field diagnostics; forged tenant header; unknown, missing and expired sessions;
duplicate/malformed cookies; missing and duplicate CSRF; denied resources;
backend outages; invalid compiler plans; and request-consistent query/form
capture. Also rerun the existing #2336 and #2340 suites plus web unit tests.

This is a library-level reference and is NOT full-stack security validation.

### Integration with verified Web Contract IR scopes

When a route declares `from scope("principal_id")` or
`from scope("tenant_id")`, the preflight adapter now constructs a
`VerifiedRequestScope` **only after** checking the server-owned session,
expiry, authoritative tenant boundary, synchronizer CSRF token, and resource
authorization. It then binds that verified scope through the same compiler
request plan used by normal request dispatch. Unauthenticated scope requests
abort before returning field-level validation details. Path, query, header,
cookie and form values cannot populate these scope bindings.

This handoff applies to **read-only preflight**, not yet to the production
handler execution path. No production session backend, login flow, cookie
issuer, revocation integration, or tenant database RLS is supplied by this
change. The built-in development HTTP listener continues to deny scoped
handler execution without a verified host provider.

### Experimental read-only HTTP dispatch adapter

`WebDevServer::bind_runtime_with_session_reads` is an explicitly configured
host integration for ordinary **GET/HEAD** handlers that declare
`from scope("principal_id")` or `from scope("tenant_id")`. It requires a
server-configured expected `Host` and a `SessionReadGateway`, backed by
`SessionLookup` and a separate `ResourceReadAuthorization::can_read` policy.
The resource/tenant selector identifies the *requested* resource; the store
still validates session expiry/revocation, membership to the target tenant,
and the independent resource read authorization on every guarded request.

No verified session yields HTTP 401; a denied resource/tenant yields HTTP 403;
a failed store or policy backend yields HTTP 503. The error body does not expose
principal, tenant, or other identity details. Request headers, cookies, form,
path, and query cannot supply the trusted scope values after authorization.
The existing `bind_runtime` / `nula dev` entry points install **no**
credential verifier; guarded routes continue to fail closed.

This is a **read-only integration slice, not production web authentication**:
when the adapter is installed, all HTTP mutations and all SSE subscriptions are
blocked before handlers run. Origin/Fetch Metadata, CSRF enforcement for
ordinary mutations, signed-token authorization integration, production session
issuance/revocation, external ingress/TLS, and full tenant-level RLS remain
independent deployment gates. Crucially, `ResourceAuthorization::can_preflight`
is NOT reused for ordinary data read authorization; hosts must implement
`ResourceReadAuthorization::can_read` separately. Unscoped public routes are
still public and must not be used for tenant-private data.
