# Nulang Web production security gate

The default Web runtime currently enforces response security headers and
request logging. **It does not provide production authentication or CSRF
middleware.** Deployment IR must list only middleware actually wired into the
default request pipeline. Importing an authentication package does not make it
an automatic authorization boundary.

The bundled @nulang/auth module is a development example: it accepts arbitrary
user identities via login(user) and stores sessions in an ephemeral KV namespace.
This integration adds cryptographically random session identifiers and
development-only synchronizer CSRF tokens, but those improvements do **not**
turn the module into a verified production identity provider. A configured
AUTH_COOKIE_SECRET cannot establish identity or tenant authorization.

The package manager now fails closed for deployments that import or depend on
the known development-only @nulang/auth package. The source scanner uses
Nulang lexer tokens (not literal import lines) and refuses missing/unreadable or
symlinked paths under `src/` rather than silently excluding them. This
also applies to deployment dry runs; local development builds remain available. Deployable
apps must replace that example with a security-reviewed authentication provider
and enforce server-side authorization at every mutating handler.

These checks are intentionally limited: they do not implement production
sessions, CSRF tokens, secure proxy/origin admission, tenant scoping, or
transaction-bound authorization. See issue #2178 for the remaining
production-ready web security acceptance criteria. The pending origin/Host
admission hardening must be reconciled independently against the exact runtime
revision before deployment.
