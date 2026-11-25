//! Experimental local authoritative policy store. The database path and
//! administrative publisher are trusted host configuration, never HTTP input.
//! A rollback/restore of the database itself is outside this threat model.
use super::{
    NativePermissions, PermissionRule, PolicySnapshot, RoleAssignment, SnapshotPolicyInput,
    MAX_FIELD_BYTES,
};
use crate::runtime::HttpMethod;
use crate::web::tenant_auth::{TenantRouteAuthorizer, VerifiedTenant};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

// JSON encoding may expand control characters to six bytes. Bound the stored
// representation and revalidate decoded values before constructing snapshots.
const MAX_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;

pub struct SqlitePolicyStore {
    conn: Mutex<Connection>,
}

impl SqlitePolicyStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|_| "cannot open policy store")?;
        conn.busy_timeout(Duration::from_secs(2))
            .map_err(|_| "cannot configure policy store")?;
        conn.execute_batch(
            "PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS nulang_policy_current_v1 (
                tenant_id TEXT PRIMARY KEY NOT NULL,
                revision INTEGER NOT NULL CHECK(revision > 0),
                payload TEXT NOT NULL
             );",
        )
        .map_err(|_| "cannot initialize policy store")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Atomically initialize revision 1 or replace the exact expected revision
    /// with its successor. False denotes a stale or invalid publication.
    /// There is no delete/reset operation: revoke with a successor snapshot.
    pub fn publish(
        &self,
        expected: Option<u64>,
        snapshot: &PolicySnapshot,
    ) -> Result<bool, String> {
        let next = expected.unwrap_or(0).checked_add(1);
        if next != Some(snapshot.revision()) || expected == Some(0) {
            return Ok(false);
        }
        let revision =
            i64::try_from(snapshot.revision()).map_err(|_| "policy revision out of range")?;
        let payload = serde_json::to_string(&(&snapshot.assignments, &snapshot.rules))
            .map_err(|_| "cannot encode policy snapshot")?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err("policy payload exceeds storage budget".into());
        }
        let conn = self.conn.lock().map_err(|_| "policy store unavailable")?;
        let affected = match expected {
            None => conn.execute(
                "INSERT INTO nulang_policy_current_v1 (tenant_id,revision,payload)
                 VALUES (?1,?2,?3) ON CONFLICT(tenant_id) DO NOTHING",
                params![snapshot.tenant_id(), revision, payload],
            ),
            Some(previous) => conn.execute(
                "UPDATE nulang_policy_current_v1 SET revision=?2,payload=?3
                 WHERE tenant_id=?1 AND revision=?4",
                params![
                    snapshot.tenant_id(),
                    revision,
                    payload,
                    i64::try_from(previous).map_err(|_| "policy revision out of range")?
                ],
            ),
        }
        .map_err(|_| "policy publication failed")?;
        Ok(affected == 1)
    }

    /// Read current revision and both policy collections from a single row.
    /// No in-memory caching, fallback to earlier versions, or revision chosen
    /// by the caller. Corrupt policy state fails closed.
    pub fn current(&self, tenant_id: &str) -> Result<Option<Arc<PolicySnapshot>>, String> {
        if tenant_id.is_empty() || tenant_id.len() > MAX_FIELD_BYTES {
            return Err("invalid policy tenant".into());
        }
        let conn = self.conn.lock().map_err(|_| "policy store unavailable")?;
        let row = conn.query_row(
            "SELECT revision, CASE WHEN length(CAST(payload AS BLOB)) <= ?2 THEN payload ELSE NULL END
             FROM nulang_policy_current_v1 WHERE tenant_id=?1",
            params![tenant_id, MAX_PAYLOAD_BYTES as i64],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
        ).optional().map_err(|_| "policy read failed")?;
        let Some((revision, payload)) = row else {
            return Ok(None);
        };
        let payload = payload.ok_or("stored policy exceeds storage budget")?;
        let (assignments, rules): (Vec<RoleAssignment>, Vec<PermissionRule>) =
            serde_json::from_str(&payload).map_err(|_| "invalid stored policy payload")?;
        let revision = u64::try_from(revision).map_err(|_| "invalid stored policy revision")?;
        PolicySnapshot::new(tenant_id, revision, assignments, rules)
            .map(Arc::new)
            .map(Some)
    }
}

/// Trusted canonical resource lookup, separate from the policy store.
/// The client must not choose these values directly.
pub struct ResourcePolicyTarget {
    pub resource_tenant_id: String,
    pub resource: String,
    pub action: String,
}

pub type ResourcePolicyLoader = dyn Fn(&VerifiedTenant, HttpMethod, &str, &HashMap<String, String>) -> Option<ResourcePolicyTarget>
    + Send
    + Sync;

impl NativePermissions {
    /// Every authenticated check loads the verified tenant's current policy
    /// directly from the durable store. Resource mutation still requires
    /// transactional authorization at the data boundary.
    pub fn sqlite_authorizer(
        self,
        store: Arc<SqlitePolicyStore>,
        resources: Arc<ResourcePolicyLoader>,
    ) -> Arc<TenantRouteAuthorizer> {
        self.snapshot_authorizer(Arc::new(move |principal, method, route, params| {
            let target = resources(principal, method, route, params)?;
            let snapshot = store.current(principal.tenant_id()).ok()??;
            Some(SnapshotPolicyInput {
                resource_tenant_id: target.resource_tenant_id,
                resource: target.resource,
                action: target.action,
                snapshot,
            })
        }))
    }
}
