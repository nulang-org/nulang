# Experimental Web production deployment gate

Nulang's `@nulang/auth` module is a development example, not an audited
authentication, identity, session, authorization, or tenant isolation provider.

`nula deploy` fails closed when the manifest depends on `nulang-auth` /
`@nulang/auth`, or any `.nula` source file imports `@nulang/auth`
(or a submodule). This occurs before Web artifact generation and applies
to every deployment adapter **including dry-run**. Local Web builds
remain available for development.

The Web HTTP runtime currently installs response security headers and
request logging. Neither importing `@nulang/auth` nor having an
`AUTH_COOKIE_SECRET` installs production auth middleware. The experimental
session-CSRF and origin-admission changes in other draft PRs are not
automatically installed by this change. Do not deploy state-changing
routes that rely on unaudited session or CSRF protections.

Separately, the public `compile_runtime_routes_from_contracts` entrypoint
reruns authoritative route diagnostics before it attaches potentially
unvalidated compiler contracts to executable HTTP routes. This covers
the case where callers bypass source-tree validation.


Path contract validation also rejects repeated path-capture names (for example,
`/accounts/{id}/children/{id}`). The runtime captures parameters by name; a
duplicate could otherwise overwrite the first identifier. The compiler and
runtime use one path segment parser so named reverse URLs, route admission,
and the HTTP matcher agree on this rejection.

Imports using other legal whitespace after `import` (including tabs) are
covered by the development-auth deployment guard.
