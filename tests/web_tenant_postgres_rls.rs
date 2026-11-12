//! Negative PostgreSQL row-level security checks against an isolated test DB.
//! Run with `--features postgres` and `NU_TEST_POSTGRES_URL`; never production.
#![cfg(feature = "postgres")]

use postgres::{Client, NoTls};

#[test]
fn postgres_rls_enforces_tenant_read_write_and_transaction_scope() {
    let url = match std::env::var("NU_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => url,
        _ if std::env::var("NU_TEST_POSTGRES_REQUIRED").as_deref() == Ok("1") => {
            panic!("NU_TEST_POSTGRES_URL is mandatory for tenant RLS CI")
        }
        _ => {
            eprintln!(
                "SKIPPED: no disposable NU_TEST_POSTGRES_URL provided; RLS has NOT been tested"
            );
            return;
        }
    };
    let mut client = Client::connect(&url, NoTls).expect("connect disposable tenant test database");
    let role = client
        .query_one(
            "SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
            &[],
        )
        .expect("inspect database role");
    assert!(
        !role.get::<_, bool>(0) && !role.get::<_, bool>(1),
        "tenant RLS must be tested using a nonsuperuser without BYPASSRLS"
    );

    let schema = format!(
        "nulang_rls_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    assert!(schema
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    let fixture = include_str!("fixtures/web_tenant_rls.sql").replace("__SCHEMA__", &schema);
    client
        .batch_execute(&fixture)
        .expect("create RLS-enabled disposable schema");
    let table = format!("{schema}.records");

    // A transaction-scoped setting cannot leak into the next pooled request.
    for (tenant, id) in [("tenant-a", 1_i64), ("tenant-b", 2_i64)] {
        let mut tx = client.transaction().unwrap();
        tx.query_one(
            "SELECT set_config('nulang.tenant_id', $1, true)",
            &[&tenant],
        )
        .unwrap();
        tx.execute(
            &format!("INSERT INTO {table} (id, tenant_id, payload) VALUES ($1, $2, $3)"),
            &[&id, &tenant, &format!("payload-{tenant}")],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    let visible = |client: &mut Client| -> i64 {
        client
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .unwrap()
            .get(0)
    };
    assert_eq!(
        visible(&mut client),
        0,
        "without tenant context, RLS denies reads"
    );
    {
        let mut tx = client.transaction().unwrap();
        tx.query_one(
            "SELECT set_config('nulang.tenant_id', $1, true)",
            &[&"tenant-a"],
        )
        .unwrap();
        let records = tx
            .query(&format!("SELECT id, payload FROM {table} ORDER BY id"), &[])
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].get::<_, i64>(0), 1);
        assert_eq!(
            tx.execute(
                &format!("UPDATE {table} SET payload = 'illegal' WHERE id = 2"),
                &[]
            )
            .unwrap(),
            0
        );
        assert_eq!(
            tx.execute(&format!("DELETE FROM {table} WHERE id = 2"), &[])
                .unwrap(),
            0
        );
        tx.commit().unwrap();
    }
    assert_eq!(
        visible(&mut client),
        0,
        "tenant setting must reset after COMMIT"
    );
    {
        let mut tx = client.transaction().unwrap();
        tx.query_one(
            "SELECT set_config('nulang.tenant_id', $1, true)",
            &[&"tenant-b"],
        )
        .unwrap();
        let records = tx.query(&format!("SELECT id FROM {table}"), &[]).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].get::<_, i64>(0), 2);
        tx.commit().unwrap();
    }
    // Check both INSERT and UPDATE WITH CHECK policies, in isolated txns.
    for forbidden_sql in [
        format!("INSERT INTO {table} (id, tenant_id, payload) VALUES (3, 'tenant-b', 'wrong')"),
        format!("UPDATE {table} SET tenant_id = 'tenant-b' WHERE id = 1"),
    ] {
        let mut tx = client.transaction().unwrap();
        tx.query_one(
            "SELECT set_config('nulang.tenant_id', $1, true)",
            &[&"tenant-a"],
        )
        .unwrap();
        assert!(
            tx.batch_execute(&forbidden_sql).is_err(),
            "cross-tenant write unexpectedly succeeded: {forbidden_sql}"
        );
    }
    {
        let mut tx = client.transaction().unwrap();
        tx.query_one(
            "SELECT set_config('nulang.tenant_id', $1, true)",
            &[&"tenant-a' OR 1=1 --"],
        )
        .unwrap();
        assert_eq!(
            tx.query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .unwrap()
                .get::<_, i64>(0),
            0
        );
    }
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .expect("remove disposable fixture");
}
