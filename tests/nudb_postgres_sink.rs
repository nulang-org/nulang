#![cfg(feature = "postgres")]
use nulang::database::consumer_postgres::{PgSinkError, PostgresConsumerSink};
use nulang::database::retention::RetentionClass;
use nulang::database::retention_store::DurableRetentionRegistry;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
const TABLET: u64 = 201;
const EPOCH: u64 = 4;
static NEXT: AtomicU64 = AtomicU64::new(1);
fn url() -> String {
    std::env::var("NUDB_POSTGRES_TEST_URL")
        .expect("postgres integration tests need a disposable NUDB_POSTGRES_TEST_URL")
}
struct Sandbox {
    root: PathBuf,
    schema: String,
    url: String,
}
impl Sandbox {
    fn new() -> Self {
        let schema = format!(
            "nudb_it_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(&schema);
        fs::create_dir(&root).unwrap();
        let url = url();
        let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        db.batch_execute(&format!("CREATE SCHEMA \"{schema}\""))
            .unwrap();
        Self { root, schema, url }
    }
    fn reg(&self) -> DurableRetentionRegistry {
        DurableRetentionRegistry::open(self.root.join("tablet.pins"), TABLET, EPOCH).unwrap()
    }
    fn sink(&self) -> PostgresConsumerSink {
        PostgresConsumerSink::connect(&self.url, &self.schema, TABLET, EPOCH).unwrap()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let Ok(mut db) = postgres::Client::connect(&self.url, postgres::NoTls) else {
            return;
        };
        let _ = db.batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE",
            self.schema
        ));
    }
}

#[test]
fn transactional_effect_and_idempotency_receipt_survive_new_connection() {
    let s = Sandbox::new();
    {
        let mut r = s.reg();
        r.register_receipt_managed(RetentionClass::Replica, "peer", 2)
            .unwrap();
        let mut sink = s.sink();
        sink.deliver_and_ack(
            &mut r,
            RetentionClass::Replica,
            "peer",
            6,
            "txn-1",
            b"remote-payload",
        )
        .unwrap();
        assert_eq!(
            r.consumer_cursor(RetentionClass::Replica, "peer").unwrap(),
            Some(6)
        );
    }
    let mut sink = s.sink();
    assert_eq!(
        sink.read_effect(RetentionClass::Replica, "peer", "txn-1")
            .unwrap(),
        Some(b"remote-payload".to_vec())
    );
    let receipt = sink
        .apply(
            RetentionClass::Replica,
            "peer",
            6,
            "txn-1",
            b"remote-payload",
        )
        .unwrap();
    let mut reg = s.reg();
    let rev = reg.revision();
    reg.commit_delivery(RetentionClass::Replica, "peer", 6, "txn-1", &receipt)
        .unwrap();
    assert_eq!(reg.revision(), rev);
    assert_eq!(sink.effect_count().unwrap(), 1);
}

#[test]
fn conflicting_delivery_id_or_cursor_never_modifies_postgres_effect() {
    let s = Sandbox::new();
    let mut sink = s.sink();
    let original = sink
        .apply(RetentionClass::Changefeed, "sink", 4, "job", b"original")
        .unwrap();
    assert!(matches!(
        sink.apply(RetentionClass::Changefeed, "sink", 5, "job", b"original"),
        Err(PgSinkError::Conflict)
    ));
    assert!(matches!(
        sink.apply(RetentionClass::Changefeed, "sink", 4, "job", b"tampered"),
        Err(PgSinkError::Conflict)
    ));
    assert_eq!(
        sink.apply(RetentionClass::Changefeed, "sink", 4, "job", b"original")
            .unwrap(),
        original
    );
    assert_eq!(
        sink.read_effect(RetentionClass::Changefeed, "sink", "job")
            .unwrap(),
        Some(b"original".to_vec())
    );
    assert_eq!(sink.effect_count().unwrap(), 1);
}

#[test]
fn aborted_postgres_transaction_cannot_expose_effect_or_receipt() {
    let s = Sandbox::new();
    // Initialize only the fixture table; the transaction below must roll back.
    drop(s.sink());
    let mut db = postgres::Client::connect(&s.url, postgres::NoTls).unwrap();
    let mut tx = db.transaction().unwrap();
    tx.execute(&format!("INSERT INTO \"{}\".nudb_consumer_effects (tablet_id, owner_epoch, class, consumer, delivery_id, next_needed, effect, receipt) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",s.schema),
        &[&(TABLET as i64),&(EPOCH as i64),&"changefeed",&"sink",&"rollback",&4_i64,&b"not-committed".to_vec(),&vec![2_u8;32]]
    ).unwrap();
    tx.rollback().unwrap();
    let mut sink = s.sink();
    assert_eq!(sink.effect_count().unwrap(), 0);
    assert_eq!(
        sink.read_effect(RetentionClass::Changefeed, "sink", "rollback")
            .unwrap(),
        None
    );
    sink.apply(RetentionClass::Changefeed, "sink", 4, "rollback", b"real")
        .unwrap();
    assert_eq!(sink.effect_count().unwrap(), 1);
}

#[test]
fn incorrect_registry_identity_or_invalid_inputs_cannot_begin_delivery() {
    let s = Sandbox::new();
    let mut reg =
        DurableRetentionRegistry::open(s.root.join("other.pins"), TABLET + 1, EPOCH).unwrap();
    reg.register_receipt_managed(RetentionClass::Backup, "archive", 2)
        .unwrap();
    let mut sink = s.sink();
    assert!(matches!(
        sink.deliver_and_ack(
            &mut reg,
            RetentionClass::Backup,
            "archive",
            5,
            "batch",
            b"payload"
        ),
        Err(PgSinkError::IdentityMismatch)
    ));
    assert_eq!(
        reg.delivery_intent(RetentionClass::Backup, "archive")
            .unwrap(),
        None
    );
    assert!(matches!(
        sink.apply(RetentionClass::Snapshot, "archive", 5, "bad", b"x"),
        Err(PgSinkError::InvalidDelivery)
    ));
    assert!(matches!(
        sink.apply(
            RetentionClass::Backup,
            "archive",
            i64::MAX as u64 + 1,
            "big",
            b"x"
        ),
        Err(PgSinkError::InvalidDelivery)
    ));
    assert!(matches!(
        PostgresConsumerSink::connect(&s.url, "public;DROP TABLE abc", TABLET, EPOCH),
        Err(PgSinkError::InvalidSchema)
    ));
    assert_eq!(sink.effect_count().unwrap(), 0);
}

#[test]
fn sink_commit_before_local_ack_is_crash_replay_safe() {
    let s = Sandbox::new();
    {
        let mut r = s.reg();
        r.register_receipt_managed(RetentionClass::Changefeed, "feed", 3)
            .unwrap();
        r.begin_delivery(RetentionClass::Changefeed, "feed", 8, "batch")
            .unwrap();
        s.sink()
            .apply(
                RetentionClass::Changefeed,
                "feed",
                8,
                "batch",
                b"remote-commit",
            )
            .unwrap();
        assert_eq!(
            r.consumer_cursor(RetentionClass::Changefeed, "feed")
                .unwrap(),
            Some(3)
        );
    }
    let mut r = s.reg();
    assert_eq!(
        r.delivery_intent(RetentionClass::Changefeed, "feed")
            .unwrap(),
        Some((8, "batch".to_string()))
    );
    let mut sink = s.sink();
    sink.deliver_and_ack(
        &mut r,
        RetentionClass::Changefeed,
        "feed",
        8,
        "batch",
        b"remote-commit",
    )
    .unwrap();
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "feed")
            .unwrap(),
        Some(8)
    );
    assert_eq!(sink.effect_count().unwrap(), 1);
}

#[test]
fn no_tls_connector_refuses_non_loopback_host_and_hostaddr_before_dial() {
    let s = Sandbox::new();
    let remote = "host=192.0.2.1 port=5432 user=nudb dbname=nudb connect_timeout=1";
    assert!(matches!(
        PostgresConsumerSink::connect(remote, &s.schema, TABLET, EPOCH),
        Err(PgSinkError::InsecureTransport)
    ));
    let hostaddr_override = "host=127.0.0.1 hostaddr=192.0.2.1 port=5432 connect_timeout=1";
    assert!(matches!(
        PostgresConsumerSink::connect(hostaddr_override, &s.schema, TABLET, EPOCH),
        Err(PgSinkError::InsecureTransport)
    ));
}

#[test]
fn concurrent_connections_create_exactly_one_effect_without_overwrites() {
    use std::thread;
    let s = Sandbox::new();
    // Isolate row idempotency from concurrent DDL creation.
    drop(s.sink());
    let url = s.url.clone();
    let schema = s.schema.clone();
    let a = thread::spawn({
        let url = url.clone();
        let schema = schema.clone();
        move || {
            let mut pg = PostgresConsumerSink::connect(&url, &schema, TABLET, EPOCH).unwrap();
            pg.apply(RetentionClass::Replica, "peer", 3, "shared", b"one")
        }
    });
    let b = thread::spawn(move || {
        let mut pg = PostgresConsumerSink::connect(&url, &schema, TABLET, EPOCH).unwrap();
        pg.apply(RetentionClass::Replica, "peer", 3, "shared", b"two")
    });
    let outcomes = [a.join().unwrap(), b.join().unwrap()];
    assert_eq!(outcomes.iter().filter(|v| v.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|v| matches!(v, Err(PgSinkError::Conflict)))
            .count(),
        1
    );
    assert_eq!(s.sink().effect_count().unwrap(), 1);
}

#[test]
fn child_fixture_exits_after_postgres_commit_without_registry_ack() {
    let Ok(root) = std::env::var("NUDB_PG_CRASH_ROOT") else {
        return;
    };
    let schema = std::env::var("NUDB_PG_CRASH_SCHEMA").unwrap();
    let mut reg =
        DurableRetentionRegistry::open(PathBuf::from(root).join("tablet.pins"), TABLET, EPOCH)
            .unwrap();
    reg.register_receipt_managed(RetentionClass::Changefeed, "child", 2)
        .unwrap();
    reg.begin_delivery(RetentionClass::Changefeed, "child", 5, "crash")
        .unwrap();
    let mut sink = PostgresConsumerSink::connect(&url(), &schema, TABLET, EPOCH).unwrap();
    sink.apply(
        RetentionClass::Changefeed,
        "child",
        5,
        "crash",
        b"committed-before-exit",
    )
    .unwrap();
    std::process::exit(55);
}
#[test]
fn actual_subprocess_death_after_pg_commit_retains_old_pin_until_replay() {
    use std::process::Command;
    let s = Sandbox::new();
    // Initialize table before the child attempts its transaction.
    s.sink();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_fixture_exits_after_postgres_commit_without_registry_ack",
        ])
        .env("NUDB_POSTGRES_TEST_URL", &s.url)
        .env("NUDB_PG_CRASH_ROOT", &s.root)
        .env("NUDB_PG_CRASH_SCHEMA", &s.schema)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(55));
    let mut r = s.reg();
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "child")
            .unwrap(),
        Some(2)
    );
    let mut sink = s.sink();
    sink.deliver_and_ack(
        &mut r,
        RetentionClass::Changefeed,
        "child",
        5,
        "crash",
        b"committed-before-exit",
    )
    .unwrap();
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "child")
            .unwrap(),
        Some(5)
    );
    assert_eq!(sink.effect_count().unwrap(), 1);
}
