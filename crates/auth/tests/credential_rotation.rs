//! Real PostgreSQL acceptance for the actual rotation APIs (not copied SQL).
//! Run with TEST_DATABASE_URL pointing at a disposable local database and
//! --include-ignored. Each test owns an isolated schema, never a shared table.
#![cfg(feature = "postgres")]

use std::time::Duration;

use sqlx::{postgres::PgPoolOptions, PgPool};
use tt_auth::{
    postgres::{CredentialStoreError, PostgresProviderCredentialStore},
    ProviderCredentialStore,
};
use uuid::Uuid;

const OLD: [u8; 32] = [31; 32];
const NEW: [u8; 32] = [47; 32];

struct Fixture {
    admin: PgPool,
    pool: PgPool,
    schema: String,
}

impl Fixture {
    async fn new() -> Self {
        let url =
            std::env::var("TEST_DATABASE_URL").expect("explicit disposable TEST_DATABASE_URL");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("rotation_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let name = schema.clone();
        let pool = PgPoolOptions::new().max_connections(4).after_connect(move |conn, _| {
            let name = name.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false), set_config('application_name', $1, false)")
                    .bind(name).execute(conn).await?;
                Ok(())
            })
        }).connect(&url).await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE provider_credentials (
            id uuid PRIMARY KEY DEFAULT gen_random_uuid(), org_id uuid NOT NULL,
            provider text NOT NULL, label text NOT NULL, secret_enc bytea NOT NULL,
            base_url text, extra_headers jsonb NOT NULL, rotated_at timestamptz,
            UNIQUE(org_id, provider)
        )",
        )
        .execute(&pool)
        .await
        .unwrap();
        Self {
            admin,
            pool,
            schema,
        }
    }

    fn store(&self, master: [u8; 32]) -> PostgresProviderCredentialStore {
        PostgresProviderCredentialStore::new(self.pool.clone(), master)
    }

    async fn seed(&self, index: u128, master: [u8; 32]) -> Uuid {
        let org = Uuid::new_v4();
        let id = self
            .store(master)
            .put(
                org,
                "openai",
                "original",
                &format!("secret-{index}"),
                None,
                &[],
            )
            .await
            .unwrap();
        sqlx::query("UPDATE provider_credentials SET id=$1 WHERE id=$2")
            .bind(Uuid::from_u128(index))
            .bind(id)
            .execute(&self.pool)
            .await
            .unwrap();
        org
    }

    async fn blob(&self, index: u128) -> Vec<u8> {
        sqlx::query_scalar("SELECT secret_enc FROM provider_credentials WHERE id=$1")
            .bind(Uuid::from_u128(index))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn secret(&self, master: [u8; 32], org: Uuid) -> String {
        self.store(master)
            .get(org, "openai")
            .await
            .unwrap()
            .unwrap()
            .api_key
            .expose()
            .to_owned()
    }

    async fn wait_for_locks(&self, count: i64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let actual: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock'")
                    .bind(&self.schema).fetch_one(&self.admin).await.unwrap();
                if actual >= count { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("actual rotation/racing write must reach database lock barrier");
    }

    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn bounded_mixed_generations_resume_without_rewriting_current_rows() {
    let f = Fixture::new().await;
    let a = f.seed(0, OLD).await; // nil UUID must not be skipped by the initial cursor
    let b = f.seed(1, NEW).await;
    let c = f.seed(2, OLD).await;
    let current_blob = f.blob(1).await;
    let stats = f.store(OLD).reencrypt_all_batched(&NEW, 1).await.unwrap();
    assert_eq!(
        (
            stats.scanned,
            stats.reencrypted,
            stats.already_current,
            stats.batches
        ),
        (3, 2, 1, 3)
    );
    for (index, org) in [(0, a), (1, b), (2, c)] {
        assert_eq!(f.secret(NEW, org).await, format!("secret-{index}"));
    }
    assert_eq!(f.blob(1).await, current_blob);
    let blobs = [f.blob(0).await, f.blob(1).await, f.blob(2).await];
    let repeat = f.store(OLD).reencrypt_all_batched(&NEW, 2).await.unwrap();
    assert_eq!(
        (
            repeat.scanned,
            repeat.reencrypted,
            repeat.already_current,
            repeat.batches
        ),
        (3, 0, 3, 2)
    );
    assert_eq!([f.blob(0).await, f.blob(1).await, f.blob(2).await], blobs);
    // A complete reverse pass is a local primitive, NOT a hosted rollback drill.
    assert_eq!(
        f.store(NEW)
            .reencrypt_all_batched(&OLD, 2)
            .await
            .unwrap()
            .reencrypted,
        3
    );
    assert_eq!(f.secret(OLD, a).await, "secret-0");
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn corrupt_row_rolls_back_its_page_but_prior_commits_resume() {
    let f = Fixture::new().await;
    let a = f.seed(1, OLD).await;
    f.seed(2, OLD).await;
    let c = f.seed(3, OLD).await;
    let d = f.seed(4, OLD).await;
    let original_c = f.blob(3).await;
    let original_d = f.blob(4).await;
    sqlx::query("UPDATE provider_credentials SET secret_enc='\\x01' WHERE id=$1")
        .bind(Uuid::from_u128(4))
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f.store(OLD).reencrypt_all_batched(&NEW, 2).await.is_err());
    assert_eq!(f.secret(NEW, a).await, "secret-1");
    assert_eq!(f.secret(OLD, c).await, "secret-3");
    assert_eq!(
        f.blob(3).await,
        original_c,
        "preceding good row in failed page must roll back"
    );
    // Restore this fixture's exact authentic ciphertext, not a production repair strategy.
    sqlx::query("UPDATE provider_credentials SET secret_enc=$1 WHERE id=$2")
        .bind(original_d)
        .bind(Uuid::from_u128(4))
        .execute(&f.pool)
        .await
        .unwrap();
    let resumed = f.store(OLD).reencrypt_all_batched(&NEW, 2).await.unwrap();
    assert_eq!((resumed.reencrypted, resumed.already_current), (2, 2));
    assert_eq!(f.secret(NEW, d).await, "secret-4");
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn suppressed_update_is_an_error_and_rolls_back_entire_page() {
    let f = Fixture::new().await;
    let a = f.seed(1, OLD).await;
    f.seed(2, OLD).await;
    sqlx::raw_sql("CREATE FUNCTION refuse_rotation() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.id = '00000000-0000-0000-0000-000000000002'::uuid THEN RETURN NULL; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_rotation BEFORE UPDATE ON provider_credentials FOR EACH ROW EXECUTE FUNCTION refuse_rotation();")
        .execute(&f.pool).await.unwrap();
    assert!(matches!(
        f.store(OLD).reencrypt_all_batched(&NEW, 2).await,
        Err(CredentialStoreError::ConcurrentModification)
    ));
    assert_eq!(f.secret(OLD, a).await, "secret-1");
    sqlx::query("DROP TRIGGER refuse_rotation ON provider_credentials")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.store(OLD)
            .reencrypt_all_batched(&NEW, 2)
            .await
            .unwrap()
            .reencrypted,
        2
    );
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn actual_rotation_serializes_with_old_root_writer_without_losing_its_update() {
    let f = Fixture::new().await;
    let org = f.seed(1, OLD).await;
    // A DB trigger parks the actual rotation API after SELECT FOR UPDATE,
    // while it holds the row lock. The racing put skips the trigger barrier.
    let lock = 123_456_789_i64;
    let mut blocker = f.admin.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(lock)
        .execute(&mut *blocker)
        .await
        .unwrap();
    sqlx::raw_sql(&format!("CREATE FUNCTION park_rotation() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.label <> 'racing' THEN PERFORM pg_advisory_xact_lock({lock}); END IF; RETURN NEW; END $$;
        CREATE TRIGGER park_rotation BEFORE UPDATE ON provider_credentials FOR EACH ROW EXECUTE FUNCTION park_rotation();"))
        .execute(&f.pool).await.unwrap();
    let store = f.store(OLD);
    let rotation = tokio::spawn(async move { store.reencrypt_all_batched(&NEW, 1).await });
    f.wait_for_locks(1).await;
    let writer = f.store(OLD);
    let racer = tokio::spawn(async move {
        writer
            .put(org, "openai", "racing", "latest-secret", None, &[])
            .await
    });
    f.wait_for_locks(2).await;
    assert!(!rotation.is_finished());
    assert!(!racer.is_finished());
    blocker.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), rotation)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), racer)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        f.secret(OLD, org).await,
        "latest-secret",
        "old-root writer remains old-root; never claim convergence"
    );
    assert_eq!(
        f.store(OLD)
            .reencrypt_all_batched(&NEW, 1)
            .await
            .unwrap()
            .reencrypted,
        1
    );
    assert_eq!(f.secret(NEW, org).await, "latest-secret");
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn task_cancellation_rolls_back_inflight_page_and_restarts_from_beginning() {
    let f = Fixture::new().await;
    let a = f.seed(1, OLD).await;
    let b = f.seed(2, OLD).await;
    let lock = 123_456_790_i64;
    let mut blocker = f.admin.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(lock)
        .execute(&mut *blocker)
        .await
        .unwrap();
    sqlx::raw_sql(&format!("CREATE FUNCTION park_second() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.id = '00000000-0000-0000-0000-000000000002'::uuid THEN PERFORM pg_advisory_xact_lock({lock}); END IF; RETURN NEW; END $$;
        CREATE TRIGGER park_second BEFORE UPDATE ON provider_credentials FOR EACH ROW EXECUTE FUNCTION park_second();"))
        .execute(&f.pool).await.unwrap();
    let store = f.store(OLD);
    let task = tokio::spawn(async move { store.reencrypt_all_batched(&NEW, 1).await });
    f.wait_for_locks(1).await;
    assert_eq!(
        f.secret(NEW, a).await,
        "secret-1",
        "first page already committed"
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    blocker.commit().await.unwrap();
    // SQLx rolls the cancelled transaction back before reusing its connection.
    sqlx::query("DROP TRIGGER park_second ON provider_credentials")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(f.secret(OLD, b).await, "secret-2");
    let stats = f.store(OLD).reencrypt_all_batched(&NEW, 1).await.unwrap();
    assert_eq!((stats.reencrypted, stats.already_current), (1, 1));
    assert_eq!(f.secret(NEW, b).await, "secret-2");
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn legacy_api_retains_whole_transaction_rollback() {
    let f = Fixture::new().await;
    let a = f.seed(1, OLD).await;
    f.seed(2, NEW).await; // legacy API intentionally accepts old-only
    assert!(f.store(OLD).reencrypt_all(&NEW).await.is_err());
    assert_eq!(f.secret(OLD, a).await, "secret-1");
    f.cleanup().await;
}

#[tokio::test]
async fn invalid_batch_sizes_refuse_before_database_io() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    let store = PostgresProviderCredentialStore::new(pool, OLD);
    for size in [0, 1001, usize::MAX] {
        assert!(matches!(
            store.reencrypt_all_batched(&NEW, size).await,
            Err(CredentialStoreError::InvalidRotationBatchSize)
        ));
    }
}
