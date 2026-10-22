//! Explicitly invoked, destructive, disposable-database-only certification.
//! Never run this against production or with a long-lived/global token.
#[path = "../tests/libsql_live_turso_certification.rs"]
mod certification;

fn main() {
    certification::authenticated_turso_sql_round_trip();
}
