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
