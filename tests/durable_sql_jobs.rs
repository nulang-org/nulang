#![cfg(feature = "sqlite")]

use libsql::{Builder, TransactionBehavior};
use nulang::durable_sql_jobs::{DurableSqlJobs, JobRequest};

async fn memory_db() -> libsql::Connection {
    let db = Builder::new_local(":memory:").build().await.unwrap();
    let conn = db.connect().unwrap();
    DurableSqlJobs::install(&conn).await.unwrap();
    conn
}

#[tokio::test]
async fn enqueue_joins_business_transaction_and_rolls_back_together() {
    let conn = memory_db().await;
    conn.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY)", ())
        .await
        .unwrap();

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .unwrap();
    tx.execute("INSERT INTO orders (id) VALUES (1)", ())
        .await
        .unwrap();
    DurableSqlJobs::enqueue_in(
        &tx,
        JobRequest::new("fulfillment", "ProcessOrder", r#"{"order_id":1}"#, 1000),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();

    let mut rows = conn.query("SELECT COUNT(*) FROM orders", ()).await.unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        0
    );
    assert!(DurableSqlJobs::claim(&conn, "fulfillment", 1000, 100)
        .await
        .unwrap()
        .is_none());

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .unwrap();
    tx.execute("INSERT INTO orders (id) VALUES (2)", ())
        .await
        .unwrap();
    DurableSqlJobs::enqueue_in(
        &tx,
        JobRequest::new("fulfillment", "ProcessOrder", r#"{"order_id":2}"#, 1000),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(DurableSqlJobs::claim(&conn, "fulfillment", 1000, 100)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn unique_jobs_are_deduplicated_and_invalid_requests_rejected() {
    let conn = memory_db().await;
    let a = JobRequest::new("fulfillment", "ProcessOrder", "{}", 10).with_unique_key("order:1");
    let b = JobRequest::new("fulfillment", "ProcessOrder", "{}", 10).with_unique_key("order:1");
    let id = DurableSqlJobs::enqueue(&conn, a).await.unwrap();
    assert_eq!(DurableSqlJobs::enqueue(&conn, b).await.unwrap(), id);
    assert!(DurableSqlJobs::enqueue(
        &conn,
        JobRequest::new("fulfillment", "ProcessOrder", r#"{"order_id":99}"#, 10)
            .with_unique_key("order:1"),
    )
    .await
    .is_err());
    assert!(
        DurableSqlJobs::enqueue(&conn, JobRequest::new("", "Worker", "{}", 10))
            .await
            .is_err()
    );
    assert!(
        DurableSqlJobs::enqueue(&conn, JobRequest::new("q", "Worker", "{invalid", 10))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn failed_jobs_retry_and_stale_claims_cannot_acknowledge() {
    let conn = memory_db().await;
    let req = JobRequest::new("mail", "Send", "{}", 0).with_max_attempts(2);
    DurableSqlJobs::enqueue(&conn, req).await.unwrap();
    let first = DurableSqlJobs::claim(&conn, "mail", 0, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.attempt, 1);
    assert!(DurableSqlJobs::claim(&conn, "mail", 99, 100)
        .await
        .unwrap()
        .is_none());
    // Simulate crash and lease expiration.
    let second = DurableSqlJobs::claim(&conn, "mail", 101, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.attempt, 2);
    assert_eq!(first.id, second.id);
    assert!(
        !DurableSqlJobs::complete(&conn, first.id, first.attempt, 102)
            .await
            .unwrap()
    );
    assert!(
        DurableSqlJobs::fail(&conn, second.id, second.attempt, 102, 10)
            .await
            .unwrap()
    );
    assert!(DurableSqlJobs::claim(&conn, "mail", 200, 100)
        .await
        .unwrap()
        .is_none());
    assert!(
        !DurableSqlJobs::complete(&conn, second.id, second.attempt, 200)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn successful_completion_is_idempotently_fenced() {
    let conn = memory_db().await;
    DurableSqlJobs::enqueue(&conn, JobRequest::new("mail", "Send", "{}", 0))
        .await
        .unwrap();
    let job = DurableSqlJobs::claim(&conn, "mail", 0, 100)
        .await
        .unwrap()
        .unwrap();
    assert!(DurableSqlJobs::complete(&conn, job.id, job.attempt, 25)
        .await
        .unwrap());
    assert!(!DurableSqlJobs::complete(&conn, job.id, job.attempt, 25)
        .await
        .unwrap());
    assert!(DurableSqlJobs::claim(&conn, "mail", 200, 100)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn queued_job_survives_database_reopen() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "nulang-jobs-reopen-{}-{unique}.sqlite",
        std::process::id()
    ));
    let path_text = path.to_string_lossy().into_owned();
    {
        let db = Builder::new_local(&path_text).build().await.unwrap();
        let conn = db.connect().unwrap();
        DurableSqlJobs::install(&conn).await.unwrap();
        DurableSqlJobs::enqueue(
            &conn,
            JobRequest::new("uploads", "ProcessUpload", r#"{"file_id":7}"#, 0),
        )
        .await
        .unwrap();
    }
    {
        let db = Builder::new_local(&path_text).build().await.unwrap();
        let conn = db.connect().unwrap();
        let job = DurableSqlJobs::claim(&conn, "uploads", 10, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.worker, "ProcessUpload");
        assert_eq!(job.payload_json, r#"{"file_id":7}"#);
        assert!(DurableSqlJobs::complete(&conn, job.id, job.attempt, 20)
            .await
            .unwrap());
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_text}-wal"));
    let _ = std::fs::remove_file(format!("{path_text}-shm"));
}
