#!/usr/bin/env bash
set -euo pipefail

# Fail closed: never even connect unless explicitly inside disposable local PG.
[[ "${NULANG_TEST_POSTGRES_DISPOSABLE:-}" == "1" ]] || { echo 'Requires NULANG_TEST_POSTGRES_DISPOSABLE=1' >&2; exit 2; }
[[ "${PG_CLUSTER_CONF_ROOT:-}" == /tmp/pg_virtualenv.*/postgresql ]] || { echo 'Must run inside a pg_virtualenv throwaway cluster' >&2; exit 2; }
[[ "${PGHOST:-}" == "localhost" || "${PGHOST:-}" == "127.0.0.1" ]] || { echo 'Requires local disposable PGHOST' >&2; exit 2; }
[[ "${PGPORT:-}" =~ ^[0-9]+$ && "${PGPORT:-}" != "5432" ]] || { echo 'Requires an explicit non-default disposable PGPORT' >&2; exit 2; }
command -v psql >/dev/null || { echo 'psql required' >&2; exit 2; }
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pg=(psql -X -v ON_ERROR_STOP=1 -q -d postgres)
# Read-only server check, BEFORE any CREATE/DROP. Setting PGHOST/PGPORT by hand
# must never be enough to authorize writes against an existing local service.
config_file="$(psql -X -v ON_ERROR_STOP=1 -A -t -d postgres -c 'SHOW config_file')"
[[ "$config_file" == "$PG_CLUSTER_CONF_ROOT"/*/regress/postgresql.conf ]] || {
    echo "Refusing non-pg_virtualenv server configuration" >&2
    exit 2
}

# These names exist only inside pg_virtualenv's freshly created throwaway DB.
"${pg[@]}" <<'SQL'
CREATE ROLE example_tenant_a LOGIN NOINHERIT;
CREATE ROLE example_tenant_b LOGIN NOINHERIT;
CREATE ROLE example_unmapped LOGIN NOINHERIT;
SQL
"${pg[@]}" -f "$root/examples/tenant-resources/sql/001_tenant_rls.sql"
"${pg[@]}" <<'SQL'
GRANT USAGE ON SCHEMA tenant_example, tenant_example_private TO example_tenant_a, example_tenant_b, example_unmapped;
GRANT EXECUTE ON FUNCTION tenant_example_private.current_tenant() TO example_tenant_a, example_tenant_b, example_unmapped;
GRANT SELECT, INSERT, UPDATE, DELETE ON tenant_example.billboard_faces, tenant_example.campaigns TO example_tenant_a, example_tenant_b, example_unmapped;
DO $test$
BEGIN
    IF (SELECT count(*) FROM pg_catalog.pg_class
        WHERE oid IN ('tenant_example.billboard_faces'::regclass,
                      'tenant_example.campaigns'::regclass)
          AND relrowsecurity AND relforcerowsecurity) <> 2 THEN
        RAISE EXCEPTION 'Both resource tables must have forced RLS';
    END IF;
END $test$;
INSERT INTO tenant_example_private.login_tenants (login_name, tenant_id) VALUES
    ('example_tenant_a', 'tenant-a'), ('example_tenant_b', 'tenant-b');
INSERT INTO tenant_example.billboard_faces(tenant_id,id,name) VALUES
    ('tenant-a','shared','Face A'), ('tenant-b','shared','Face B'), ('tenant-b','only-b','Foreign Face');
INSERT INTO tenant_example.campaigns(tenant_id,id,face_id,title) VALUES
    ('tenant-a','shared','shared','Campaign A'),('tenant-b','shared','shared','Campaign B');
SQL

"${pg[@]}" <<'SQL'
SET SESSION AUTHORIZATION example_tenant_a;
DO $test$
BEGIN
    IF tenant_example_private.current_tenant() IS DISTINCT FROM 'tenant-a' THEN
        RAISE EXCEPTION 'Tenant A did not resolve to its own role';
    END IF;
    IF (SELECT count(*) FROM tenant_example.billboard_faces) != 1 THEN
        RAISE EXCEPTION 'RLS exposed foreign faces';
    END IF;
    IF (SELECT name FROM tenant_example.billboard_faces WHERE id='shared') IS DISTINCT FROM 'Face A' THEN
        RAISE EXCEPTION 'Duplicate face ID returned foreign tenant data';
    END IF;
    IF (SELECT title FROM tenant_example.campaigns WHERE id='shared') IS DISTINCT FROM 'Campaign A' THEN
        RAISE EXCEPTION 'Duplicate campaign ID returned foreign tenant data';
    END IF;
END $test$;
RESET SESSION AUTHORIZATION;
SQL
# PostgreSQL functions and policies must be checked as actual non-owner
# database logins. No client-controlled app.tenant_id setting establishes scope.
"${pg[@]}" <<'SQL'
SET SESSION AUTHORIZATION example_tenant_a;
DO $test$
DECLARE
    affected integer;
    rejected boolean;
BEGIN
    PERFORM set_config('app.tenant_id', 'tenant-b', false);
    IF tenant_example_private.current_tenant() IS DISTINCT FROM 'tenant-a' THEN
        RAISE EXCEPTION 'Client-set tenant GUC spoofed the database identity';
    END IF;
    UPDATE tenant_example.billboard_faces SET name='Stolen' WHERE tenant_id='tenant-b';
    GET DIAGNOSTICS affected = ROW_COUNT;
    IF affected != 0 THEN RAISE EXCEPTION 'Foreign face UPDATE succeeded'; END IF;
    UPDATE tenant_example.campaigns SET title='Stolen' WHERE tenant_id='tenant-b';
    GET DIAGNOSTICS affected = ROW_COUNT;
    IF affected != 0 THEN RAISE EXCEPTION 'Foreign campaign UPDATE succeeded'; END IF;
    DELETE FROM tenant_example.billboard_faces WHERE tenant_id='tenant-b';
    GET DIAGNOSTICS affected = ROW_COUNT;
    IF affected != 0 THEN RAISE EXCEPTION 'Foreign face DELETE succeeded'; END IF;
    DELETE FROM tenant_example.campaigns WHERE tenant_id='tenant-b';
    GET DIAGNOSTICS affected = ROW_COUNT;
    IF affected != 0 THEN RAISE EXCEPTION 'Foreign campaign DELETE succeeded'; END IF;
    rejected := false;
    BEGIN
        INSERT INTO tenant_example.billboard_faces(tenant_id,id,name)
        VALUES ('tenant-b','forged','Forged');
    EXCEPTION WHEN insufficient_privilege THEN rejected := true;
    END;
    IF NOT rejected THEN RAISE EXCEPTION 'Foreign face INSERT succeeded'; END IF;
    rejected := false;
    BEGIN
        UPDATE tenant_example.billboard_faces
        SET tenant_id='tenant-b' WHERE tenant_id='tenant-a' AND id='shared';
    EXCEPTION WHEN insufficient_privilege THEN rejected := true;
    END;
    IF NOT rejected THEN RAISE EXCEPTION 'Tenant-changing UPDATE succeeded'; END IF;
    rejected := false;
    BEGIN
        INSERT INTO tenant_example.campaigns (tenant_id,id,face_id,title)
        VALUES ('tenant-a','bad-link','only-b','Cross-tenant reference');
    EXCEPTION WHEN foreign_key_violation THEN rejected := true;
    END;
    IF NOT rejected THEN RAISE EXCEPTION 'Foreign face FK accepted'; END IF;
    rejected := false;
    BEGIN
        EXECUTE 'SELECT count(*) FROM tenant_example_private.login_tenants';
    EXCEPTION WHEN insufficient_privilege THEN rejected := true;
    END;
    IF NOT rejected THEN RAISE EXCEPTION 'Tenant role could access protected role mappings'; END IF;
    -- A failed relationship must roll back earlier mutations in its unit.
    rejected := false;
    BEGIN
        UPDATE tenant_example.billboard_faces SET name='Partially updated'
            WHERE tenant_id='tenant-a' AND id='shared';
        INSERT INTO tenant_example.campaigns(tenant_id,id,face_id,title)
            VALUES ('tenant-a','invalid-transaction','only-b','Broken FK');
    EXCEPTION WHEN foreign_key_violation THEN rejected := true;
    END;
    IF NOT rejected THEN RAISE EXCEPTION 'Broken campaign reference was accepted'; END IF;
    IF (SELECT name FROM tenant_example.billboard_faces WHERE id='shared') IS DISTINCT FROM 'Face A' THEN
        RAISE EXCEPTION 'Denied operation modified own record';
    END IF;
END $test$;
-- Verify SQL transactions commit together and survive reconnects.
BEGIN;
INSERT INTO tenant_example.billboard_faces (tenant_id,id,name)
VALUES ('tenant-a','new-face','New face');
INSERT INTO tenant_example.campaigns(tenant_id,id,face_id,title)
VALUES ('tenant-a','new-campaign','new-face','New campaign');
COMMIT;
RESET SESSION AUTHORIZATION;
SQL
# A login role cannot assume another tenant's login role.
if "${pg[@]}" -c 'SET SESSION AUTHORIZATION example_tenant_a; SET ROLE example_tenant_b' > /dev/null 2>&1; then
    echo 'Tenant A unexpectedly assumed tenant B role' >&2
    exit 1
fi
"${pg[@]}" <<'SQL'
SET SESSION AUTHORIZATION example_tenant_b;
DO $test$
BEGIN
    IF tenant_example_private.current_tenant() IS DISTINCT FROM 'tenant-b' THEN
        RAISE EXCEPTION 'Tenant B role mapping invalid';
    END IF;
    IF (SELECT count(*) FROM tenant_example.billboard_faces) <> 2 THEN
        RAISE EXCEPTION 'Tenant B saw wrong number of faces';
    END IF;
    IF (SELECT name FROM tenant_example.billboard_faces WHERE id='shared') IS DISTINCT FROM 'Face B' THEN
        RAISE EXCEPTION 'Duplicate ID leaked tenant A face';
    END IF;
    IF EXISTS(SELECT 1 FROM tenant_example.billboard_faces WHERE id='new-face') THEN
        RAISE EXCEPTION 'Committed tenant A face leaked to tenant B';
    END IF;
    IF EXISTS(SELECT 1 FROM tenant_example.campaigns WHERE id='new-campaign') THEN
        RAISE EXCEPTION 'Committed tenant A campaign leaked to tenant B';
    END IF;
END $test$;
RESET SESSION AUTHORIZATION;
SET SESSION AUTHORIZATION example_unmapped;
DO $test$
DECLARE rejected boolean := false;
BEGIN
    IF tenant_example_private.current_tenant() IS NOT NULL THEN
        RAISE EXCEPTION 'Unmapped login unexpectedly resolved to tenant';
    END IF;
    IF EXISTS(SELECT 1 FROM tenant_example.billboard_faces) OR
       EXISTS(SELECT 1 FROM tenant_example.campaigns) THEN
        RAISE EXCEPTION 'Unmapped role read tenant data';
    END IF;
    BEGIN
        INSERT INTO tenant_example.billboard_faces(tenant_id,id,name)
        VALUES ('tenant-a','forged','Forged');
    EXCEPTION WHEN insufficient_privilege THEN rejected := true;
    END;
    IF NOT rejected THEN RAISE EXCEPTION 'Unmapped login inserted a row'; END IF;
END $test$;
RESET SESSION AUTHORIZATION;
SQL
# Check the durable two-record transaction using a NEW database session.
"${pg[@]}" <<'SQL'
SET SESSION AUTHORIZATION example_tenant_a;
DO $test$
BEGIN
    IF (SELECT name FROM tenant_example.billboard_faces WHERE id='new-face') IS DISTINCT FROM 'New face' OR
       (SELECT title FROM tenant_example.campaigns WHERE id='new-campaign') IS DISTINCT FROM 'New campaign' THEN
        RAISE EXCEPTION 'Committed face/campaign pair was not durable across sessions';
    END IF;
END $test$;
SQL
printf 'SQL tenant RLS assertions passed\n'
