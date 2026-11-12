//! Verify that the bundled SQLite engine meets the WAL disclosure fix floor.
//! RUSTSEC-2026-0459 / CVE-2026-119375 was fixed in SQLite 3.54.0.
use rusqlite::Connection;

#[test]
fn bundled_sqlite_is_at_least_3_54_0() {
    let version = rusqlite::version_number();
    assert!(
        version >= 3_054_000,
        "bundled SQLite must be >= 3.54.0 (found {}, version number {})",
        rusqlite::version(),
        version
    );

    // Keep a small real-connection check so the version assertion is paired
    // with the SQLite engine used by Nulang's local AI store.
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE sample(value TEXT NOT NULL); INSERT INTO sample VALUES ('ok');",
        )
        .unwrap();
    let value: String = connection
        .query_row("SELECT value FROM sample", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "ok");
}
