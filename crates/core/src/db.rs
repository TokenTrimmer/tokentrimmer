//! Postgres pool builder and migration runner.
//!
//! The [`MIGRATOR`] static embeds all SQL migration files from `migrations/`
//! at compile time via [`sqlx::migrate!`]. Call [`migrate`] once on startup
//! after acquiring a pool to bring the schema up to date.

use std::time::Duration;

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
