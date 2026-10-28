#!/usr/bin/env bash
set -euo pipefail

# This harness is intentionally incapable of connecting to an existing DB.
# pg_virtualenv owns and destroys the isolated database cluster.
command -v pg_virtualenv >/dev/null || { echo 'pg_virtualenv required' >&2; exit 2; }
command -v openssl >/dev/null || { echo 'openssl required' >&2; exit 2; }
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

PGPORT=15433 pg_virtualenv bash -euo pipefail -c '
    root="$1"
    [[ "${PGHOST:-}" == localhost || "${PGHOST:-}" == 127.0.0.1 ]] || exit 2
    [[ "${PGPORT:-}" == 15433 ]] || exit 2
    [[ "${PG_CLUSTER_CONF_ROOT:-}" == /tmp/pg_virtualenv.*/postgresql ]] || exit 2
    config_file="$(psql -w -X -A -t -v ON_ERROR_STOP=1 -d postgres -c "SHOW config_file")"
    [[ "$config_file" == "$PG_CLUSTER_CONF_ROOT"/*/regress/postgresql.conf ]] || {
        echo "Refusing non-disposable PostgreSQL before schema writes" >&2
        exit 2
    }
    export NULANG_TEST_POSTGRES_DISPOSABLE=1
    export NULANG_TENANT_TEST_PASSWORD_A="$(openssl rand -hex 24)"
    export NULANG_TENANT_TEST_PASSWORD_B="$(openssl rand -hex 24)"
    psql -w -X -v ON_ERROR_STOP=1 -q -d postgres <<SQL
CREATE ROLE example_tenant_a LOGIN NOINHERIT PASSWORD '\''$NULANG_TENANT_TEST_PASSWORD_A'\'';
CREATE ROLE example_tenant_b LOGIN NOINHERIT PASSWORD '\''$NULANG_TENANT_TEST_PASSWORD_B'\'';
SQL
    psql -w -X -v ON_ERROR_STOP=1 -q -d postgres \
        -f "$root/examples/tenant-resources/sql/001_tenant_rls.sql"
    psql -w -X -v ON_ERROR_STOP=1 -q -d postgres <<SQL
GRANT USAGE ON SCHEMA tenant_example, tenant_example_private TO example_tenant_a, example_tenant_b;
GRANT EXECUTE ON FUNCTION tenant_example_private.current_tenant() TO example_tenant_a, example_tenant_b;
GRANT SELECT, INSERT, UPDATE, DELETE ON tenant_example.billboard_faces, tenant_example.campaigns TO example_tenant_a, example_tenant_b;
INSERT INTO tenant_example_private.login_tenants(login_name,tenant_id) VALUES
  ('\''example_tenant_a'\'', '\''tenant-a'\''), ('\''example_tenant_b'\'', '\''tenant-b'\'');
INSERT INTO tenant_example.billboard_faces(tenant_id,id,name) VALUES
  ('\''tenant-a'\'', '\''shared'\'', '\''Face A'\''),
  ('\''tenant-a'\'', '\''new-face'\'', '\''New face'\''),
  ('\''tenant-b'\'', '\''shared'\'', '\''Face B'\''),
  ('\''tenant-b'\'', '\''only-b'\'', '\''Foreign Face'\'');
INSERT INTO tenant_example.campaigns(tenant_id,id,face_id,title) VALUES
  ('\''tenant-a'\'', '\''shared'\'', '\''shared'\'', '\''Campaign A'\''),
  ('\''tenant-b'\'', '\''shared'\'', '\''shared'\'', '\''Campaign B'\'');
SQL
    export CARGO_TARGET_DIR="${NULANG_TEST_CARGO_TARGET_DIR:-$root/target}"
    cargo test --locked --manifest-path "$root/Cargo.toml" \
        --no-default-features --features postgres \
        --test tenant_pg_request_authority -- --test-threads=1
' bash "$root"
