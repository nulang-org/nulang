#!/usr/bin/env bash
set -euo pipefail

# Exercise the reference tenant RLS schema only in a fresh, disposable cluster.
# The underlying test verifies server config_file before issuing any writes.
command -v pg_virtualenv >/dev/null || { echo 'pg_virtualenv is required' >&2; exit 2; }
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
proof="$root/tests/tenant_resource_postgres_rls.sh"
bash -n "$proof"

# Negative guard: never accept an ordinary invocation, regardless of caller env.
if env -u NULANG_TEST_POSTGRES_DISPOSABLE bash "$proof" >/dev/null 2>&1; then
    echo 'RLS proof accepted a non-disposable invocation' >&2
    exit 1
fi

# pg_virtualenv destroys its isolated cluster when the child process exits.
pg_virtualenv bash -euo pipefail -c '
    proof="$1"
    if env NULANG_TEST_POSTGRES_DISPOSABLE=1 \
      PG_CLUSTER_CONF_ROOT=/tmp/pg_virtualenv.forged/postgresql \
      bash "$proof" >/dev/null 2>&1; then
        echo "RLS proof accepted forged cluster provenance" >&2
        exit 1
    fi
    if env NULANG_TEST_POSTGRES_DISPOSABLE=1 PGPORT=5432 \
      bash "$proof" >/dev/null 2>&1; then
        echo "RLS proof accepted the default PostgreSQL port" >&2
        exit 1
    fi
    NULANG_TEST_POSTGRES_DISPOSABLE=1 bash "$proof"
' bash "$proof"
