#![cfg(feature = "sqlite")]

use nulang::web::native_permissions::sqlite_store::SqlitePolicyStore;
use nulang::web::native_permissions::{PermissionRule, PolicySnapshot, RoleAssignment};
use std::path::PathBuf;
use std::sync::{Arc, Barrier};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nulang-policy-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn db(&self) -> PathBuf {
        self.0.join("policy.sqlite")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn snapshot(tenant: &str, revision: u64, allowed: bool) -> PolicySnapshot {
    let assignments = if allowed {
        vec![RoleAssignment {
            tenant_id: tenant.into(),
            subject_id: "alice".into(),
            role: "reader".into(),
        }]
    } else {
        vec![]
    };
    PolicySnapshot::new(
        tenant,
        revision,
        assignments,
        vec![PermissionRule {
            tenant_id: tenant.into(),
            role: "reader".into(),
            resource: "document".into(),
            action: "read".into(),
            outcome: "allow".into(),
        }],
    )
    .unwrap()
}

#[test]
fn durable_store_rejects_rollback_same_revision_skips_and_cross_tenant_reuse() {
    let fixture = Fixture::new();
    let path = fixture.db();
    let store = SqlitePolicyStore::open(&path).unwrap();
    assert!(store.current("tenant-a").unwrap().is_none());
    assert!(!store.publish(None, &snapshot("tenant-a", 2, true)).unwrap());
    assert!(store.publish(None, &snapshot("tenant-a", 1, true)).unwrap());
    assert!(store.publish(None, &snapshot("tenant-b", 1, true)).unwrap());
    assert!(store
        .publish(Some(1), &snapshot("tenant-a", 2, false))
        .unwrap());
    assert!(!store
        .publish(Some(1), &snapshot("tenant-a", 2, true))
        .unwrap());
    assert!(!store
        .publish(Some(2), &snapshot("tenant-a", 1, true))
        .unwrap());
    assert!(!store
        .publish(Some(2), &snapshot("tenant-a", 4, true))
        .unwrap());
    assert!(!store.publish(None, &snapshot("tenant-a", 1, true)).unwrap());
    drop(store);
    let reopened = SqlitePolicyStore::open(&path).unwrap();
    assert_eq!(reopened.current("tenant-a").unwrap().unwrap().revision(), 2);
    assert_eq!(reopened.current("tenant-b").unwrap().unwrap().revision(), 1);
    assert!(reopened.current("missing").unwrap().is_none());
}

#[test]
fn concurrent_writers_cannot_replace_a_committed_revision() {
    let fixture = Fixture::new();
    let path = fixture.db();
    let store = SqlitePolicyStore::open(&path).unwrap();
    assert!(store.publish(None, &snapshot("tenant-a", 1, true)).unwrap());
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [true, false]
        .into_iter()
        .map(|allowed| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let writer = SqlitePolicyStore::open(&path).unwrap();
                barrier.wait();
                writer
                    .publish(Some(1), &snapshot("tenant-a", 2, allowed))
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|&&won| won).count(), 1);
    assert_eq!(store.current("tenant-a").unwrap().unwrap().revision(), 2);
}

#[test]
fn malformed_persisted_payload_is_never_returned_as_authoritative_policy() {
    let fixture = Fixture::new();
    let path = fixture.db();
    let store = SqlitePolicyStore::open(&path).unwrap();
    assert!(store.publish(None, &snapshot("tenant-a", 1, true)).unwrap());
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute(
        "UPDATE nulang_policy_current_v1 SET payload='invalid' WHERE tenant_id='tenant-a'",
        [],
    )
    .unwrap();
    assert!(store.current("tenant-a").is_err());
}

#[test]
fn sqlite_authorizer_reads_current_revision_and_denies_revoked_or_corrupt_policy() {
    use nulang::runtime::HttpMethod;
    use nulang::web::native_permissions::{sqlite_store::ResourcePolicyTarget, NativePermissions};
    use nulang::web::tenant_auth::VerifiedTenant;
    let fixture = Fixture::new();
    let path = fixture.db();
    let store = Arc::new(SqlitePolicyStore::open(&path).unwrap());
    let publisher = SqlitePolicyStore::open(&path).unwrap();
    let authorize = NativePermissions::compile().unwrap().sqlite_authorizer(
        store,
        Arc::new(|_, _, _, _| {
            Some(ResourcePolicyTarget {
                resource_tenant_id: "tenant-a".into(),
                resource: "document".into(),
                action: "read".into(),
            })
        }),
    );
    let alice = VerifiedTenant::new("alice", "tenant-a").unwrap();
    let foreign = VerifiedTenant::new("alice", "tenant-b").unwrap();
    let params = std::collections::HashMap::new();
    let check =
        |principal: &VerifiedTenant| authorize(principal, HttpMethod::Get, "/secure", &params);
    assert!(!check(&alice));
    assert!(publisher
        .publish(None, &snapshot("tenant-a", 1, true))
        .unwrap());
    assert!(check(&alice));
    assert!(!check(&foreign));
    assert!(publisher
        .publish(Some(1), &snapshot("tenant-a", 2, false))
        .unwrap());
    assert!(!check(&alice));
    assert!(!publisher
        .publish(Some(1), &snapshot("tenant-a", 2, true))
        .unwrap());
    assert!(!check(&alice));
    assert!(publisher
        .publish(Some(2), &snapshot("tenant-a", 3, true))
        .unwrap());
    assert!(check(&alice));
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute(
        "UPDATE nulang_policy_current_v1 SET payload='invalid' WHERE tenant_id='tenant-a'",
        [],
    )
    .unwrap();
    assert!(!check(&alice));
}
