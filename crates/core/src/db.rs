//! Postgres pool builder and migration runner.
//!
//! The [`MIGRATOR`] static embeds all SQL migration files from `migrations/`
//! at compile time via [`sqlx::migrate!`]. Call [`migrate`] once on startup
//! after acquiring a pool to bring the schema up to date.

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
/// # Errors
///
/// Returns [`sqlx::Error`] if the connection string is malformed or the
/// initial connection attempt fails.
pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
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
/// Returns [`sqlx::migrate::MigrateError`] if any migration fails or if the
/// migration table cannot be created.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

/// Connect to `database_url` and run all pending migrations, then return.
///
/// Strict counterpart to the best-effort boot-time `migrate`: any connect or
/// migration error propagates (callers exit non-zero). Used by
/// `tt gateway --migrate-only` as the explicit, gated migration step in the
/// deploy pipeline. `database_url` MUST be Neon's direct (non-pooled) endpoint
/// — the migrator needs session-mode advisory locks.
pub async fn migrate_only(database_url: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let pool = crate::connect(database_url, 2)
        .await
        .context("migrate-only: connect failed")?;
    migrate(&pool)
        .await
        .context("migrate-only: migration failed")?;
    Ok(())
}
