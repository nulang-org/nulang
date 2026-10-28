#!/usr/bin/env bash
set -euo pipefail
# Disposable, loopback-only PostgreSQL. No existing cluster is used.
for executable in initdb pg_ctl postgres psql openssl python3; do
  command -v "$executable" >/dev/null || exit 2
done
repo="$(cd "$(dirname "$0")/.." && pwd)"
umask 077
root="$(mktemp -d /var/tmp/nulang-web-session-pg.XXXXXXXX)"
[[ "$root" == /var/tmp/nulang-web-session-pg.* ]] || exit 2
running=0
cleanup() {
  if ((running)); then pg_ctl -D "$root/data" -m immediate -w stop >/dev/null 2>&1 || :; fi
  if [[ "$root" == /var/tmp/nulang-web-session-pg.* ]] && [[ -d "$root" ]]; then
    rm -rf -- "$root"
  fi
}
trap cleanup EXIT
export NULANG_TEST_POSTGRES_DISPOSABLE=1
export NULANG_TEST_POSTGRES_DATA_DIR="$root/data"
export NULANG_PG_ADMIN_USER=nulang_pg_test_owner
export NULANG_TEST_POSTGRES_NONCE="$(openssl rand -hex 32)"
export NULANG_PG_ADMIN_PASSWORD="$(openssl rand -hex 24)"
export NULANG_PG_SESSION_PASSWORD="$(openssl rand -hex 24)"
export NULANG_PG_TENANT_PASSWORD_A="$(openssl rand -hex 24)"
export NULANG_PG_TENANT_PASSWORD_B="$(openssl rand -hex 24)"
printf '%s\n' "$NULANG_PG_ADMIN_PASSWORD" > "$root/admin-password"
export PGPORT="$(python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(('127.0.0.1', 0))
    print(s.getsockname()[1])
PY
)"
[[ "$PGPORT" != 5432 && "$PGPORT" -gt 1024 ]] || exit 2
initdb -D "$root/data" --no-instructions --auth-local=trust \
  --auth-host=scram-sha-256 --username="$NULANG_PG_ADMIN_USER" \
  --pwfile="$root/admin-password" > "$root/init.log" 2>&1
pg_ctl -D "$root/data" -w -l "$root/postgres.log" \
  -o "-h 127.0.0.1 -p $PGPORT -k $root -c unix_socket_permissions=0700" start > "$root/start.log" 2>&1
running=1
actual="$(psql -X -A -t -v ON_ERROR_STOP=1 -h "$root" -p "$PGPORT" -U "$NULANG_PG_ADMIN_USER" -d postgres -c 'SHOW data_directory')"
[[ "$actual" == "$NULANG_TEST_POSTGRES_DATA_DIR" ]] || { echo "Refusing non-disposable server" >&2; exit 2; }
psql -X -v ON_ERROR_STOP=1 -q -h "$root" -p "$PGPORT" \
  -U "$NULANG_PG_ADMIN_USER" -d postgres <<SQL
CREATE ROLE nulang_session_reader LOGIN NOSUPERUSER NOBYPASSRLS NOINHERIT PASSWORD '$NULANG_PG_SESSION_PASSWORD';
CREATE ROLE nulang_tenant_a LOGIN NOSUPERUSER NOBYPASSRLS NOINHERIT PASSWORD '$NULANG_PG_TENANT_PASSWORD_A';
CREATE ROLE nulang_tenant_b LOGIN NOSUPERUSER NOBYPASSRLS NOINHERIT PASSWORD '$NULANG_PG_TENANT_PASSWORD_B';
-- The disposable-cluster proof uses a random database nonce, not elevated settings access.
CREATE SCHEMA nulang_private;
CREATE TABLE nulang_private.cluster_identity (nonce text NOT NULL PRIMARY KEY);
INSERT INTO nulang_private.cluster_identity VALUES ('$NULANG_TEST_POSTGRES_NONCE');
CREATE TABLE nulang_private.sessions (
 handle_digest text PRIMARY KEY, principal_id text NOT NULL, tenant_id text NOT NULL,
 expires_at bigint NOT NULL, csrf_hex text NOT NULL, revoked boolean NOT NULL DEFAULT false
);
CREATE TABLE nulang_private.role_tenants (login_name text PRIMARY KEY, tenant_id text NOT NULL);
INSERT INTO nulang_private.role_tenants VALUES ('nulang_tenant_a','tenant-a'),('nulang_tenant_b','tenant-b');
REVOKE ALL ON SCHEMA nulang_private FROM PUBLIC;
GRANT USAGE ON SCHEMA nulang_private TO nulang_session_reader, nulang_tenant_a, nulang_tenant_b;
GRANT SELECT ON nulang_private.sessions TO nulang_session_reader;
GRANT SELECT ON nulang_private.cluster_identity TO nulang_session_reader, nulang_tenant_a, nulang_tenant_b;
CREATE FUNCTION nulang_private.current_tenant() RETURNS text
 LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog
 AS 'SELECT tenant_id FROM nulang_private.role_tenants WHERE login_name = session_user::text';
REVOKE ALL ON FUNCTION nulang_private.current_tenant() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION nulang_private.current_tenant() TO nulang_tenant_a, nulang_tenant_b;
CREATE SCHEMA nulang_example;
CREATE TABLE nulang_example.orders (
 tenant_id text NOT NULL, id text NOT NULL, owner_id text NOT NULL,
 PRIMARY KEY (tenant_id,id)
);
ALTER TABLE nulang_example.orders ENABLE ROW LEVEL SECURITY;
ALTER TABLE nulang_example.orders FORCE ROW LEVEL SECURITY;
CREATE POLICY visible_tenant ON nulang_example.orders FOR SELECT
 TO nulang_tenant_a, nulang_tenant_b
 USING (tenant_id = nulang_private.current_tenant());
GRANT USAGE ON SCHEMA nulang_example TO nulang_tenant_a, nulang_tenant_b;
GRANT SELECT ON nulang_example.orders TO nulang_tenant_a, nulang_tenant_b;
INSERT INTO nulang_example.orders VALUES
 ('tenant-a','order-4','user-a'),('tenant-a','sibling','user-a'),
 ('tenant-b','order-4','user-b'),('tenant-b','only-b','user-b');
SQL
export CARGO_TARGET_DIR="/home/davidporkka/nulang/target"
# Read-only privilege probes against this newly created disposable cluster.
# Print only status, never credentials or the random cluster nonce.
for role in nulang_session_reader nulang_tenant_a nulang_tenant_b; do
  case "$role" in
    nulang_session_reader) role_pw="$NULANG_PG_SESSION_PASSWORD" ;;
    nulang_tenant_a) role_pw="$NULANG_PG_TENANT_PASSWORD_A" ;;
    nulang_tenant_b) role_pw="$NULANG_PG_TENANT_PASSWORD_B" ;;
  esac
  if PGPASSWORD="$role_pw" psql -w -X -A -t -v ON_ERROR_STOP=1 -h 127.0.0.1 -p "$PGPORT" \
    -U "$role" -d postgres -c "SELECT count(*) FROM nulang_private.cluster_identity" > /dev/null; then
    echo "Disposable role probe: $role SELECT identity OK"
  else
    echo "Disposable role probe: $role SELECT identity FAIL" >&2
    exit 2
  fi
done
echo "Testing real disposable PostgreSQL session/RLS adapters (loopback only)"
cargo test --locked --offline --manifest-path "$repo/Cargo.toml" \
 --no-default-features --features postgres \
 --test web_session_postgres_disposable -- --test-threads=1
