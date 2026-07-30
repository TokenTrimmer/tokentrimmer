//! Postgres pool builder and migration runner.
//!
//! The [`MIGRATOR`] static embeds all SQL migration files from `migrations/`
//! at compile time via [`sqlx::migrate!`]. Call [`migrate`] once on startup
//! after acquiring a pool to bring the schema up to date.

use std::time::Duration;

use anyhow::{bail, Context as _};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Embedded migrator. Compile-time scans `crates/core/migrations/`.
///
/// Embed all `*.sql` migration files relative to the crate root so they are
/// part of the binary and available without any filesystem access at runtime.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Build a Postgres connection pool.
///
/// The caller is responsible for running migrations via [`migrate`] before
/// issuing any queries.
///
/// # Arguments
///
/// * `url` — Postgres connection string, e.g. `postgres://user:pass@host/db`.
/// * `max_connections` — upper bound on pool size; keep below Postgres
///   `max_connections` (default 100) minus headroom for other clients.
///
/// # Pool tuning
///
/// Beyond `max_connections` the builder sets explicit lifetimes/timeouts
/// rather than relying on sqlx's defaults (which leave `max_lifetime` /
/// `idle_timeout` unbounded and `acquire_timeout` at 30s):
///
/// * `min_connections(1)` — keep one connection warm so the request after an
///   idle lull doesn't pay full TCP+TLS+auth latency.
/// * `max_lifetime(30m)` — recycle connections well before common
///   server/pooler idle cutoffs (PgBouncer, Neon) so we never hand out a
///   half-dead socket.
/// * `idle_timeout(5m)` — release surplus connections back to Postgres / the
///   pooler during a traffic lull.
/// * `acquire_timeout(5s)` — fail fast: a request should error promptly when
///   the pool is exhausted, not block for 30s.
///
/// # Errors
///
/// Returns [`sqlx::Error`] if the connection string is malformed or the
/// initial connection attempt fails.
pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .min_connections(1)
        .max_lifetime(Duration::from_secs(30 * 60))
        .idle_timeout(Duration::from_secs(5 * 60))
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await
}

/// Run all pending migrations on the given pool.
///
/// Idempotent: already-applied migrations are skipped. Meant to be called once
/// at application startup after [`connect`].
///
/// # Errors
///
/// Returns an error if any migration fails, if the ledger cannot be read back,
/// or if its exact successful version/checksum set differs from this binary.
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    MIGRATOR
        .run(pool)
        .await
        .context("apply gateway migrations")?;
    verify_migration_ledger(pool).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MigrationLedgerRow {
    version: i64,
    checksum: Vec<u8>,
    success: bool,
}

fn validate_migration_ledger(mut rows: Vec<MigrationLedgerRow>) -> anyhow::Result<()> {
    let expected: Vec<_> = MIGRATOR
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .collect();
    rows.sort_by_key(|row| row.version);
    if rows.len() != expected.len() {
        bail!(
            "gateway migration ledger has {} rows; binary embeds {} up migrations",
            rows.len(),
            expected.len()
        );
    }
    for (row, migration) in rows.iter().zip(expected) {
        if row.version != migration.version {
            bail!(
                "gateway migration ledger version mismatch: found {}, expected {}",
                row.version,
                migration.version
            );
        }
        if !row.success {
            bail!(
                "gateway migration ledger records failed migration {}",
                row.version
            );
        }
        if row.checksum.as_slice() != migration.checksum.as_ref() {
            bail!(
                "gateway migration ledger checksum mismatch for migration {}",
                row.version
            );
        }
    }
    Ok(())
}

/// Prove the request-serving database exposes the exact successful gateway
/// migration ledger embedded in this binary.
pub async fn verify_migration_ledger(pool: &PgPool) -> anyhow::Result<()> {
    let rows: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(
        "SELECT version, checksum, success \
         FROM public._sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .context("read gateway migration ledger")?;
    validate_migration_ledger(
        rows.into_iter()
            .map(|(version, checksum, success)| MigrationLedgerRow {
                version,
                checksum,
                success,
            })
            .collect(),
    )
}

/// Connect to `database_url` and run all pending migrations, then return.
///
/// Any connect, migration, or ledger-readback error propagates (callers exit
/// non-zero), matching configured normal boot. Used by
/// `tt gateway --migrate-only` as the explicit, gated migration step in the
/// deploy pipeline. `database_url` MUST be Neon's direct (non-pooled) endpoint
/// — the migrator needs session-mode advisory locks.
pub async fn migrate_only(database_url: &str) -> anyhow::Result<()> {
    let pool = crate::connect(database_url, 2)
        .await
        .context("migrate-only: connect failed")?;
    migrate(&pool)
        .await
        .context("migrate-only: migration failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_migration_ledger, MigrationLedgerRow, MIGRATOR};

    fn embedded_rows() -> Vec<MigrationLedgerRow> {
        MIGRATOR
            .iter()
            .filter(|migration| migration.migration_type.is_up_migration())
            .map(|migration| MigrationLedgerRow {
                version: migration.version,
                checksum: migration.checksum.as_ref().to_vec(),
                success: true,
            })
            .collect()
    }

    #[test]
    fn exact_embedded_gateway_ledger_is_accepted() {
        validate_migration_ledger(embedded_rows()).unwrap();
    }

    #[test]
    fn missing_changed_or_failed_gateway_ledger_is_rejected() {
        let mut missing = embedded_rows();
        missing.pop();
        assert!(validate_migration_ledger(missing).is_err());

        let mut changed = embedded_rows();
        changed[0].checksum[0] ^= 0xff;
        assert!(validate_migration_ledger(changed).is_err());

        let mut failed = embedded_rows();
        failed[0].success = false;
        assert!(validate_migration_ledger(failed).is_err());
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL"]
    async fn configured_database_migrates_and_reads_back_exact_gateway_ledger() {
        let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
        let pool = super::connect(&url, 2)
            .await
            .expect("connect test database");
        super::migrate(&pool)
            .await
            .expect("migrate and verify exact gateway ledger");
        super::verify_migration_ledger(&pool)
            .await
            .expect("request pool sees exact gateway ledger");
    }
}
