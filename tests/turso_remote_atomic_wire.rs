#![cfg(feature = "turso-remote-probe")]
//! Test-only loopback HTTP/1.1 fixture for the modern serverless protocol.
//! Never call the public HTTPS-only preflight with a localhost HTTP URL.
//! No Turso credentials or durable records are involved.
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::Duration,
};
use turso_serverless::{BatchStatement, TransactionBehavior};

fn read_request(mut socket: &TcpStream) -> (String, Value) {
    socket
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    let (end, expected) = loop {
        let count = socket.read(&mut buf).expect("request must arrive");
        assert!(count > 0, "unexpected EOF before HTTP body");
        bytes.extend_from_slice(&buf[..count]);
        assert!(bytes.len() < 128 * 1024, "oversize fixture request");
        if let Some(headers_end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..headers_end]).unwrap();
            if let Some(len) = headers.lines().find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|s| s.trim().parse::<usize>().ok())
            }) {
                break (headers_end + 4, len);
            }
        }
    };
    while bytes.len() < end + expected {
        let count = socket.read(&mut buf).expect("HTTP body continuation");
        assert!(count > 0, "premature HTTP body EOF");
        bytes.extend_from_slice(&buf[..count]);
    }
    let header = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let value = serde_json::from_slice(&bytes[end..end + expected]).unwrap();
    (header, value)
}

fn respond(mut stream: &TcpStream, body: &Value) {
    let payload = body.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload,
    )
    .unwrap();
    stream.flush().unwrap();
}

/// `fail_at` is the absolute (0-based) step number. In a failed batch
/// the fixture simulates the transactional protocol's error-guarded rollback.
fn server(
    fail_at: Option<usize>,
    disconnect: bool,
) -> (String, thread::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let child = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let (header, body) = read_request(&stream);
        if disconnect {
            return (header, body);
        }
        let steps = body["requests"][0]["batch"]["steps"]
            .as_array()
            .expect("real batch");
        let mut results = Vec::with_capacity(steps.len());
        let mut errors = Vec::with_capacity(steps.len());
        let last = steps.len() - 1;
        for i in 0..steps.len() {
            if fail_at == Some(i) {
                results.push(Value::Null);
                errors.push(
                    json!({"code":"SQLITE_CONSTRAINT","message":"injected constraint failure"}),
                );
            } else if fail_at.is_some() && i > fail_at.unwrap() && i != last {
                results.push(Value::Null);
                errors.push(Value::Null);
            } else if fail_at.is_some() && i == last {
                results.push(json!({"cols":[],"rows":[],"affected_row_count":0}));
                errors.push(Value::Null);
            } else if i == last {
                // Success path: ROLLBACK is conditional on failed COMMIT.
                results.push(Value::Null);
                errors.push(Value::Null);
            } else {
                results.push(json!({"cols":[],"rows":[],"affected_row_count":1}));
                errors.push(Value::Null);
            }
        }
        respond(
            &stream,
            &json!({
              "baton":null,
              "results":[
                {"type":"ok","response":{"type":"batch","result":{"step_results":results,"step_errors":errors}}},
                {"type":"ok","response":{"type":"get_autocommit","is_autocommit":true}}
              ]
            }),
        );
        (header, body)
    });
    (url, child)
}

fn fixture() -> Vec<BatchStatement> {
    vec![
        BatchStatement::new(
            "INSERT INTO durable_transitions(actor_id, sequence, digest) VALUES (?1, ?2, ?3)",
            (41_i64, 2_i64, "synthetic-canonical-digest"),
        )
        .unwrap(),
        BatchStatement::new(
            "UPDATE durable_tails SET sequence=?1 WHERE actor_id=?2 AND sequence=?3",
            (2_i64, 41_i64, 1_i64),
        )
        .unwrap(),
    ]
}

async fn send(url: &str) -> turso_serverless::Result<Vec<turso_serverless::BatchResult>> {
    let db = turso_serverless::Builder::new_remote(url)
        .with_auth_token("synthetic-test-token")
        .build()
        .await?;
    let conn = db.connect()?;
    // Require server-side BEGIN IMMEDIATE, conditional COMMIT/ROLLBACK.
    // This wire proof is NOT yet a durable CAS commit implementation.
    conn.transactional_batch(fixture(), TransactionBehavior::Immediate)
        .await
}

fn assert_wire(header: &str, body: &Value) {
    assert!(
        header.starts_with("POST /v3/pipeline HTTP/1.1"),
        "must use protocol v3"
    );
    assert!(
        header
            .lines()
            .any(|x| x.eq_ignore_ascii_case("Authorization: Bearer synthetic-test-token")),
        "driver must send bearer auth"
    );
    let steps = body["requests"][0]["batch"]["steps"].as_array().unwrap();
    let sql = steps
        .iter()
        .map(|v| v["stmt"]["sql"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    assert_eq!(
        sql,
        vec![
            "BEGIN IMMEDIATE",
            "INSERT INTO durable_transitions(actor_id, sequence, digest) VALUES (?1, ?2, ?3)",
            "UPDATE durable_tails SET sequence=?1 WHERE actor_id=?2 AND sequence=?3",
            "COMMIT",
            "ROLLBACK"
        ]
    );
    assert_eq!(steps[1]["condition"], json!({"type":"ok","step":0}));
    assert_eq!(steps[2]["condition"], json!({"type":"ok","step":1}));
    assert_eq!(steps[3]["condition"], json!({"type":"ok","step":2}));
    assert_eq!(
        steps[4]["condition"],
        json!({
            "type":"and",
            "conds":[{"type":"ok","step":0},{"type":"not","cond":{"type":"ok","step":3}}]
        })
    );
    assert_eq!(
        steps[1]["stmt"]["args"][2]["value"],
        json!("synthetic-canonical-digest")
    );
}

#[tokio::test]
async fn atomic_batch_serializes_immediate_commit_and_guarded_rollback() {
    let (url, child) = server(None, false);
    let result = send(&url).await;
    let (header, body) = child.join().unwrap();
    assert_wire(&header, &body);
    assert_eq!(result.unwrap().len(), 2);
}

#[tokio::test]
async fn remote_constraint_error_must_never_look_like_success() {
    let (url, child) = server(Some(2), false);
    let result = send(&url).await;
    let (header, body) = child.join().unwrap();
    assert_wire(&header, &body);
    assert!(
        result.is_err(),
        "constraint failure cannot certify a durable commit"
    );
}

#[tokio::test]
async fn transport_disconnect_cannot_certify_a_commit() {
    let (url, child) = server(None, true);
    let result = send(&url).await;
    let (header, body) = child.join().unwrap();
    assert_wire(&header, &body);
    assert!(result.is_err(), "unknown commit outcome must fail closed");
}

/// An interactive transaction can inspect CAS result *before* committing.
/// The loopback fixture records four separate HTTP requests, with batons
/// retained across the logical stream. It does not emulate SQLite storage.
fn interactive_server(cas_affected: u64) -> (String, thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut messages = Vec::new();
        for i in 0..4 {
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "missing transaction request {i}"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("mock accept: {e}"),
                }
            };
            let (_, body) = read_request(&socket);
            let sql = body["requests"][0]["stmt"]["sql"].as_str().unwrap();
            let affected = if sql.starts_with("UPDATE") {
                cas_affected
            } else {
                1
            };
            let autocommit = sql == "COMMIT" || sql == "ROLLBACK";
            let response = json!({
                "baton": if autocommit { Value::Null } else { json!(format!("step-{i}")) },
                "results":[
                    {"type":"ok","response":{"type":"execute","result":{
                        "cols":[],"rows":[],"affected_row_count":affected
                    }}},
                    {"type":"ok","response":{"type":"get_autocommit","is_autocommit":autocommit}}
                ]
            });
            respond(&socket, &response);
            messages.push(body);
        }
        messages
    });
    (url, worker)
}

/// Test-only control-flow proof, not a production durable actor writer.
async fn interactive_claim(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let db = turso_serverless::Builder::new_remote(url)
        .with_auth_token("synthetic-test-token")
        .build()
        .await?;
    let mut conn = db.connect()?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    tx.execute(
        "INSERT INTO durable_transitions(actor_id,sequence,digest) VALUES (?1,?2,?3)",
        (41_i64, 2_i64, "synthetic-canonical-digest"),
    )
    .await?;
    // If the old tail does not match, 0 rows change. An atomic transaction
    // alone does NOT make this an accepted compare-and-swap!
    let changed = tx
        .execute(
            "UPDATE durable_tails SET sequence=?1 WHERE actor_id=?2 AND sequence=?3",
            (2_i64, 41_i64, 1_i64),
        )
        .await?;
    if changed != 1 {
        tx.rollback().await?;
        return Err("durable tail CAS must affect exactly one record".into());
    }
    tx.commit().await?;
    Ok(())
}

fn assert_interactive_sql(messages: &[Value], final_sql: &str) {
    assert_eq!(messages.len(), 4);
    let sql = messages
        .iter()
        .map(|v| v["requests"][0]["stmt"]["sql"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        sql,
        vec![
            "BEGIN IMMEDIATE",
            "INSERT INTO durable_transitions(actor_id,sequence,digest) VALUES (?1,?2,?3)",
            "UPDATE durable_tails SET sequence=?1 WHERE actor_id=?2 AND sequence=?3",
            final_sql,
        ]
    );
    assert!(messages[0]["baton"].is_null());
    for (i, body) in messages.iter().enumerate().skip(1) {
        assert_eq!(body["baton"], json!(format!("step-{}", i - 1)));
    }
}

#[tokio::test]
async fn zero_row_cas_must_roll_back_before_claiming_commit() {
    let (url, worker) = interactive_server(0);
    let outcome = interactive_claim(&url).await;
    let messages = worker.join().unwrap();
    assert_interactive_sql(&messages, "ROLLBACK");
    assert!(
        outcome.is_err(),
        "zero-row CAS must never be reported as committed"
    );
}

#[tokio::test]
async fn matching_cas_may_commit_after_affected_row_check() {
    let (url, worker) = interactive_server(1);
    let outcome = interactive_claim(&url).await;
    let messages = worker.join().unwrap();
    assert_interactive_sql(&messages, "COMMIT");
    assert!(outcome.is_ok(), "matching CAS should be allowed to commit");
}

#[tokio::test]
async fn multi_row_cas_is_also_refused() {
    let (url, worker) = interactive_server(2);
    let outcome = interactive_claim(&url).await;
    let messages = worker.join().unwrap();
    assert_interactive_sql(&messages, "ROLLBACK");
    assert!(
        outcome.is_err(),
        "multi-row tail modification must be rejected"
    );
}
