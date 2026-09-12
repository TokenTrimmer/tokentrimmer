#![cfg(feature = "postgres")]
//! Real PostgreSQL acceptance for caller-owned audit transactions and readiness.
//! Each test owns a UUID-named schema; run only against a disposable local DB.
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    Executor, PgPool,
};
use std::{str::FromStr, sync::Arc, time::Duration};
use tt_telemetry::audit::{
    postgres::PostgresAuditWriter, verify_chain, Actor, AuditStorageReadiness, AuditWriter,
    InMemoryAuditWriter,
};
use uuid::Uuid;

struct Fixture {
    admin: PgPool,
    pool: PgPool,
    options: PgConnectOptions,
    schema: String,
}
impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
        let admin = PgPool::connect(&url).await.unwrap();
        let schema = format!("audit_tx_{}", Uuid::new_v4().simple());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
        let options = PgConnectOptions::from_str(&url)
            .unwrap()
            .options([("search_path", schema.clone())]);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        pool.execute("CREATE TABLE business_state (org_id UUID PRIMARY KEY, value INT NOT NULL)")
            .await
            .unwrap();
        pool.execute("CREATE TABLE audit_entries (
            id UUID PRIMARY KEY, org_id UUID NOT NULL REFERENCES business_state(org_id),
            ts TIMESTAMPTZ NOT NULL, actor JSONB NOT NULL, event TEXT NOT NULL CHECK (event <> 'reject'),
            payload JSONB NOT NULL, prev_hash TEXT NOT NULL, hash TEXT NOT NULL,
            signature TEXT NOT NULL, seq BIGINT NOT NULL, UNIQUE(org_id, seq))").await.unwrap();
        Self {
            admin,
            pool,
            options,
            schema,
        }
    }
    fn writer(&self) -> PostgresAuditWriter {
        PostgresAuditWriter::new(
            self.pool.clone(),
            ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
        )
    }
    async fn cleanup(self) {
        self.pool.close().await;
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL; isolated UUID schema"]
async fn audit_and_business_state_commit_together_without_a_second_connection() {
    let f = Fixture::new().await;
    let org = Uuid::new_v4();
    let writer = f.writer();
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO business_state VALUES ($1, 1)")
        .bind(org)
        .execute(&mut *tx)
        .await
        .unwrap();
    let entry = tokio::time::timeout(
        Duration::from_secs(2),
        writer.write_in_transaction(
            &mut tx,
            org,
            Actor::System,
            "committed".into(),
            serde_json::json!({"value": 1}),
        ),
    )
    .await
    .expect("one-connection pool must not deadlock")
    .unwrap();
    assert_eq!(entry.seq, 0);
    tx.commit().await.unwrap();
    assert!(matches!(
        writer.storage_readiness().await.unwrap(),
        AuditStorageReadiness::Postgres { .. }
    ));
    let entries = writer.list(org).await.unwrap();
    assert_eq!(entries.len(), 1);
    verify_chain(&entries, &writer.verifying_key()).unwrap();

    // Reconstruct both pool and signer: no process-local chain state is needed.
    f.pool.close().await;
    let reopened = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(f.options.clone())
        .await
        .unwrap();
    let after_restart = PostgresAuditWriter::new(
        reopened.clone(),
        ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
    );
    let persisted = after_restart.list(org).await.unwrap();
    assert_eq!(persisted[0].hash, entry.hash);
    verify_chain(&persisted, &after_restart.verifying_key()).unwrap();
    reopened.close().await;
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL; isolated UUID schema"]
async fn failed_or_rolled_back_audit_cannot_leave_business_state_or_a_chain_entry() {
    let f = Fixture::new().await;
    let writer = f.writer();
    for reject in [false, true] {
        let org = Uuid::new_v4();
        let mut tx = f.pool.begin().await.unwrap();
        sqlx::query("INSERT INTO business_state VALUES ($1, 1)")
            .bind(org)
            .execute(&mut *tx)
            .await
            .unwrap();
        let result = writer
            .write_in_transaction(
                &mut tx,
                org,
                Actor::System,
                if reject { "reject" } else { "rollback" }.into(),
                serde_json::json!({}),
            )
            .await;
        assert_eq!(result.is_err(), reject);
        tx.rollback().await.unwrap();
        assert!(writer.list(org).await.unwrap().is_empty());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM business_state WHERE org_id=$1")
            .bind(org)
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    // A signed in-memory append must not masquerade as a transaction participant.
    let mut tx = f.pool.begin().await.unwrap();
    assert!(InMemoryAuditWriter::new()
        .write_in_transaction(
            &mut tx,
            Uuid::new_v4(),
            Actor::System,
            "unsupported".into(),
            serde_json::json!({})
        )
        .await
        .is_err());
    tx.rollback().await.unwrap();
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL; isolated UUID schema"]
async fn readiness_detects_missing_columns_read_only_and_store_outage() {
    let f = Fixture::new().await;
    let writer = f.writer();
    assert!(matches!(
        writer.storage_readiness().await.unwrap(),
        AuditStorageReadiness::Postgres { .. }
    ));
    let reader_role = format!("audit_reader_{}", Uuid::new_v4().simple());
    f.admin
        .execute(format!("CREATE ROLE {reader_role}").as_str())
        .await
        .unwrap();
    f.admin
        .execute(format!("GRANT USAGE ON SCHEMA {} TO {reader_role}", f.schema).as_str())
        .await
        .unwrap();
    f.pool
        .execute(format!("GRANT SELECT ON audit_entries TO {reader_role}").as_str())
        .await
        .unwrap();
    let reader = PgPoolOptions::new()
        .connect_with(f.options.clone().options([("role", reader_role.clone())]))
        .await
        .unwrap();
    sqlx::query("SELECT id FROM audit_entries LIMIT 0")
        .execute(&reader)
        .await
        .unwrap();
    assert!(
        PostgresAuditWriter::new(
            reader.clone(),
            ed25519_dalek::SigningKey::from_bytes(&[7; 32])
        )
        .storage_readiness()
        .await
        .is_err(),
        "SELECT alone cannot prove audit write eligibility"
    );
    reader.close().await;
    f.pool
        .execute(format!("REVOKE ALL ON audit_entries FROM {reader_role}").as_str())
        .await
        .unwrap();
    f.admin
        .execute(format!("REVOKE USAGE ON SCHEMA {} FROM {reader_role}", f.schema).as_str())
        .await
        .unwrap();
    f.admin
        .execute(format!("DROP ROLE {reader_role}").as_str())
        .await
        .unwrap();
    let read_only = PgPoolOptions::new()
        .connect_with(
            f.options
                .clone()
                .options([("default_transaction_read_only", "on")]),
        )
        .await
        .unwrap();
    assert!(PostgresAuditWriter::new(
        read_only.clone(),
        ed25519_dalek::SigningKey::from_bytes(&[7; 32])
    )
    .storage_readiness()
    .await
    .is_err());
    read_only.close().await;
    f.pool
        .execute("ALTER TABLE audit_entries RENAME COLUMN signature TO wrong_signature")
        .await
        .unwrap();
    assert!(writer.storage_readiness().await.is_err());
    f.pool.close().await;
    assert!(writer.storage_readiness().await.is_err());
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL; isolated UUID schema"]
async fn concurrent_writers_keep_one_gap_free_chain() {
    let f = Fixture::new().await;
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO business_state VALUES ($1, 1)")
        .bind(org)
        .execute(&f.pool)
        .await
        .unwrap();
    let writer = Arc::new(f.writer());
    let other_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(f.options.clone())
        .await
        .unwrap();
    let other = Arc::new(PostgresAuditWriter::new(
        other_pool.clone(),
        ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
    ));
    let mut tasks = Vec::new();
    for i in 0..12 {
        let w = if i % 2 == 0 {
            writer.clone()
        } else {
            other.clone()
        };
        tasks.push(tokio::spawn(async move {
            w.write(
                org,
                Actor::System,
                "concurrent".into(),
                serde_json::json!({"i": i}),
            )
            .await
            .unwrap()
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let entries = writer.list(org).await.unwrap();
    assert_eq!(entries.len(), 12);
    verify_chain(&entries, &writer.verifying_key()).unwrap();
    other_pool.close().await;
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL; isolated UUID schema"]
async fn readiness_requires_actual_row_lock_permission_and_accepts_column_level_update() {
    let f = Fixture::new().await;
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO business_state VALUES ($1, 1)")
        .bind(org)
        .execute(&f.pool)
        .await
        .unwrap();
    f.writer()
        .write(org, Actor::System, "seed".into(), serde_json::json!({}))
        .await
        .unwrap();

    let role = format!("audit_append_{}", Uuid::new_v4().simple());
    f.admin
        .execute(format!("CREATE ROLE {role} NOLOGIN").as_str())
        .await
        .unwrap();
    f.admin
        .execute(format!("GRANT USAGE ON SCHEMA {} TO {role}", f.schema).as_str())
        .await
        .unwrap();
    f.pool
        .execute(format!("GRANT SELECT, INSERT ON audit_entries TO {role}").as_str())
        .await
        .unwrap();
    let limited_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(f.options.clone().options([("role", role.clone())]))
        .await
        .unwrap();
    let limited = PostgresAuditWriter::new(
        limited_pool.clone(),
        ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
    );
    let acl: (bool, bool) = sqlx::query_as(
        "SELECT has_table_privilege(current_user, 'audit_entries', 'INSERT'),
                has_any_column_privilege(current_user, 'audit_entries', 'UPDATE')",
    )
    .fetch_one(&limited_pool)
    .await
    .unwrap();
    assert_eq!(acl, (true, false));
    assert!(
        limited
            .write(org, Actor::System, "refused".into(), serde_json::json!({}))
            .await
            .is_err(),
        "the real writer must fail without its row-lock privilege"
    );
    assert!(
        limited.storage_readiness().await.is_err(),
        "readiness must not accept SELECT+INSERT when the actual FOR UPDATE cannot run"
    );
    assert_eq!(f.writer().list(org).await.unwrap().len(), 1);

    // PostgreSQL row locks require UPDATE on at least one column, not the
    // entire table. This fixture is not a recommended production ACL grant.
    f.pool
        .execute(format!("GRANT UPDATE (id) ON audit_entries TO {role}").as_str())
        .await
        .unwrap();
    let broad_update: bool =
        sqlx::query_scalar("SELECT has_table_privilege(current_user, 'audit_entries', 'UPDATE')")
            .fetch_one(&limited_pool)
            .await
            .unwrap();
    assert!(
        !broad_update,
        "do not require or silently grant table-wide UPDATE"
    );
    assert!(matches!(
        limited.storage_readiness().await.unwrap(),
        AuditStorageReadiness::Postgres { .. }
    ));
    assert_eq!(
        f.writer().list(org).await.unwrap().len(),
        1,
        "readiness writes no evidence rows"
    );
    limited
        .write(org, Actor::System, "allowed".into(), serde_json::json!({}))
        .await
        .unwrap();
    let entries = limited.list(org).await.unwrap();
    assert_eq!(entries.len(), 2);
    verify_chain(&entries, &limited.verifying_key()).unwrap();

    // The eligibility check is live, not cached after a successful append.
    f.pool
        .execute(format!("REVOKE UPDATE (id) ON audit_entries FROM {role}").as_str())
        .await
        .unwrap();
    assert!(limited.storage_readiness().await.is_err());
    assert_eq!(f.writer().list(org).await.unwrap().len(), 2);
    limited_pool.close().await;
    f.pool
        .execute(format!("REVOKE ALL ON audit_entries FROM {role}").as_str())
        .await
        .unwrap();
    f.admin
        .execute(format!("REVOKE USAGE ON SCHEMA {} FROM {role}", f.schema).as_str())
        .await
        .unwrap();
    f.admin
        .execute(format!("DROP ROLE {role}").as_str())
        .await
        .unwrap();
    f.cleanup().await;
}
