# Disposable PostgreSQL tenant RLS acceptance (audited Web stack)

This reference test ports the **database-only** tenant-isolation proof from
[#2203](https://github.com/nulang-org/nulang/pull/2203) and its strengthened
disposable CI runner from [#2316](https://github.com/nulang-org/nulang/pull/2316)
onto the hardened Web security base
[#2185](https://github.com/nulang-org/nulang/pull/2185).

No demo-cookie or in-memory KV authorization handler is imported. The SQL
fixture is **not** a production migration. The absence of a verified
application-identity-to-SQL-role binding remains an explicit release blocker.

## Run

On a Debian/Ubuntu host with `pg_virtualenv`, PostgreSQL server/client,
`openssl`, and Bash installed:

```sh
bash tests/run_tenant_resource_postgres_rls_ci.sh
```

The test wrapper requires a fresh local `pg_virtualenv` database cluster,
explicitly reserves nondefault port 15432, and runs three negative admission
checks before applying the schema: no disposable flag, forged cluster
provenance, and substituted default port. The SQL proof validates the
server's actual `config_file` **before any write**. Each test cluster is
destroyed on completion. Never run this fixture against an existing DB.

## Database invariants checked

- `ENABLE` and `FORCE ROW LEVEL SECURITY` on both tables.
- Protected `SECURITY DEFINER` login-to-tenant mapping based on
  PostgreSQL `session_user`, not client-controlled tenant headers/GUCs.
- Independently authenticated tenant-A and tenant-B PostgreSQL connections
  with randomized, disposable passwords; rejected cross-role credential use.
- Same-ID records in separate tenants cannot be read, updated, or deleted
  across tenant boundaries; unmapped logins fail closed.
- Cross-tenant inserts and changes to `tenant_id` are denied.
- Composite tenant-qualified foreign keys block cross-tenant campaign/face
  references. A failed relationship operation rolls back preceding changes
  within the same transaction; committed records survive reconnection.
- Tenant login cannot read private mappings or assume a foreign tenant role.

The CI workflow `Web Tenant PostgreSQL RLS (disposable)` is a targeted
supplemental PR gate on relevant Web, SQL, and resource changes; it does
not replace normal CI, audited dependencies, or a real authenticated
browser-to-database integration test.

## Not yet proven

Verified OIDC identity and memberships, revocation, exact external
principal-to-PostgreSQL-role mapping, pool isolation and reset, connection
credentials/TLS lifecycle, request-origin and synchronizer CSRF on the
future database-backed handler, and production-safe SQL CRUD are **not**
provided by this fixture. The verified database role is constructed by a
test harness, not supplied by Nulang's WebDevServer.

Promotion requires exact-head security review, complete CI and cargo-audit
without ignores, separate trusted-identity adapter design, and a real HTTP
to SQL boundary whose cross-tenant negative tests run on disposable
PostgreSQL. Do not independently merge overlapping #2203/#2316 and this
port onto main.
