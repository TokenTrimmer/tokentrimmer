//! Postgres execution: layered hardening for the `run_query` SQL surface.
//!
//! 1. **Parser allowlist** ([`validate_sql`]): exactly one statement, and it
//!    must be a `SELECT` / `WITH..SELECT`. The whole AST (CTEs, subqueries,
//!    function args, table-valued functions) is walked and a denylist of
//!    escape functions (`dblink*`, `pg_read_file`, `pg_read_binary_file`,
//!    `pg_ls_dir`, `lo_import`, `lo_export`) rejected. The output type
//!    [`ValidatedSql`] has a private constructor — only the validator
//!    produces one, so unvalidated SQL is unrepresentable at the execution
//!    seam.
//! 2. **Read-only transaction** ([`run`]): `SET TRANSACTION READ ONLY` +
//!    `SET LOCAL statement_timeout` → stream → **always ROLLBACK** (never
//!    commit). READ ONLY blocks writes/`nextval()` even if a parser gap is
//!    found.
//! 3. **Result caps while streaming**: at most `max_result_rows + 1` rows
//!    fetched; a running serialized-byte counter — exceeding either is a
//!    hard `result_too_large` error, never truncation.
//!
//! The REAL boundary is the operator-granted role behind `dsn_env`: grant a
//! dedicated read-only role (`default_transaction_read_only = on`, minimal
//! grants). Layers 1–3 are defense in depth, not a substitute.
//!
//! Postgres results are NEVER cached (no honest content fingerprint exists
//! in the MVP — a stale-serving cache would be a silent-wrong-number
//! machine) and `rows_scanned` is `None` (no honest number without
//! pg_stat_statements; we don't fabricate one).

use std::ops::ControlFlow;

use futures::TryStreamExt;
use serde_json::Value;
use sqlparser::ast::{Expr, ObjectName, Select, Statement, Visit, Visitor};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlx::postgres::PgRow;
use sqlx::{Column, Row, TypeInfo, ValueRef};

use crate::error::McpError;

use super::bounded::BoundedQuery;
use super::{result_too_large, QueryLimits, QueryOutcome};

/// SQL that passed the parser allowlist. Private constructor: ONLY
/// [`validate_sql`] produces one, so the executor cannot receive unvalidated
/// SQL.
#[derive(Debug, Clone)]
pub(crate) struct ValidatedSql(String);

impl ValidatedSql {
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Escape functions that read files / dial out / write large objects.
fn denied_function(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("dblink")
        || matches!(
            n.as_str(),
            "pg_read_file" | "pg_read_binary_file" | "pg_ls_dir" | "lo_import" | "lo_export"
        )
}

fn check_object_name(name: &ObjectName) -> ControlFlow<String> {
    for part in &name.0 {
        let ident = match part {
            sqlparser::ast::ObjectNamePart::Identifier(i) => &i.value,
            sqlparser::ast::ObjectNamePart::Function(f) => &f.name.value,
        };
        if denied_function(ident) {
            return ControlFlow::Break(format!("function `{ident}` is not allowed"));
        }
    }
    ControlFlow::Continue(())
}

/// Walks the full AST (including CTEs and subqueries) rejecting denylisted
/// functions and `SELECT INTO`.
struct DenyVisitor;

impl Visitor for DenyVisitor {
    type Break = String;

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<String> {
        if let Expr::Function(f) = expr {
            check_object_name(&f.name)?;
        }
        ControlFlow::Continue(())
    }

    /// Catches table-valued escape functions: `FROM dblink(...)`.
    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<String> {
        check_object_name(relation)
    }

    /// `SELECT .. INTO tbl` creates a table — rejected at the parser layer
    /// too (READ ONLY would also block it).
    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<String> {
        if select.into.is_some() {
            return ControlFlow::Break("SELECT INTO is not allowed".into());
        }
        ControlFlow::Continue(())
    }
}

/// Parser allowlist gate. The ONLY producer of [`ValidatedSql`].
pub(crate) fn validate_sql(query: &BoundedQuery) -> Result<ValidatedSql, McpError> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, query.as_str())
        .map_err(|e| McpError::InvalidParams(format!("SQL parse error: {e}")))?;
    let statement = match statements.as_slice() {
        [s] => s,
        [] => return Err(McpError::InvalidParams("empty SQL".into())),
        _ => {
            return Err(McpError::InvalidParams(format!(
                "exactly one SQL statement is allowed (got {})",
                statements.len()
            )))
        }
    };
    if !matches!(statement, Statement::Query(_)) {
        return Err(McpError::InvalidParams(
            "only a single SELECT (or WITH .. SELECT) statement is allowed".into(),
        ));
    }
    if let ControlFlow::Break(reason) = statement.visit(&mut DenyVisitor) {
        return Err(McpError::InvalidParams(format!("SQL rejected: {reason}")));
    }
    Ok(ValidatedSql(query.as_str().to_owned()))
}

/// Decode one Postgres column to JSON. Unsupported types surface as a
/// clearly-marked string (never a silent null — that would be a wrong
/// number).
fn decode_col(row: &PgRow, i: usize) -> Value {
    if let Ok(raw) = row.try_get_raw(i) {
        if raw.is_null() {
            return Value::Null;
        }
    }
    let type_name = row.columns()[i].type_info().name().to_owned();
    let decoded = match type_name.as_str() {
        "BOOL" => row.try_get::<bool, _>(i).map(Value::from),
        "INT2" => row.try_get::<i16, _>(i).map(Value::from),
        "INT4" => row.try_get::<i32, _>(i).map(Value::from),
        "INT8" => row.try_get::<i64, _>(i).map(Value::from),
        "FLOAT4" => row.try_get::<f32, _>(i).map(|v| Value::from(f64::from(v))),
        "FLOAT8" => row.try_get::<f64, _>(i).map(Value::from),
        "TEXT" | "VARCHAR" | "BPCHAR" | "CHAR" | "NAME" => {
            row.try_get::<String, _>(i).map(Value::from)
        }
        "UUID" => row
            .try_get::<uuid::Uuid, _>(i)
            .map(|v| Value::from(v.to_string())),
        "TIMESTAMPTZ" => row
            .try_get::<chrono::DateTime<chrono::Utc>, _>(i)
            .map(|v| Value::from(v.to_rfc3339())),
        "TIMESTAMP" => row
            .try_get::<chrono::NaiveDateTime, _>(i)
            .map(|v| Value::from(v.to_string())),
        "DATE" => row
            .try_get::<chrono::NaiveDate, _>(i)
            .map(|v| Value::from(v.to_string())),
        "TIME" => row
            .try_get::<chrono::NaiveTime, _>(i)
            .map(|v| Value::from(v.to_string())),
        "JSON" | "JSONB" => row.try_get::<Value, _>(i),
        other => return Value::String(format!("<unsupported:{other}>")),
    };
    decoded.unwrap_or_else(|_| Value::String(format!("<decode_error:{type_name}>")))
}

/// Execute validated SQL inside a READ ONLY transaction with a local
/// statement timeout and streaming row/byte caps; ALWAYS rolls back.
pub(crate) async fn run(
    pool: &sqlx::PgPool,
    sql: &ValidatedSql,
    limits: &QueryLimits,
) -> Result<QueryOutcome, McpError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| McpError::Internal(format!("db begin failed: {e}")))?;

    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(|e| McpError::Internal(format!("db read-only setup failed: {e}")))?;
    // statement_timeout is operator-config-validated (1..=60000); SET cannot
    // take bind parameters, so the integer is formatted in.
    sqlx::query(&format!(
        "SET LOCAL statement_timeout = {}",
        limits.statement_timeout_ms
    ))
    .execute(&mut *tx)
    .await
    .map_err(|e| McpError::Internal(format!("db timeout setup failed: {e}")))?;

    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut bytes: usize = 0;
    let result = async {
        let mut stream = sqlx::query(sql.as_str()).fetch(&mut *tx);
        while let Some(row) = stream
            .try_next()
            .await
            .map_err(|e| McpError::Internal(format!("query failed: {e}")))?
        {
            if rows.len() >= limits.max_result_rows {
                return Err(result_too_large("rows", limits.max_result_rows));
            }
            if columns.is_empty() {
                columns = row.columns().iter().map(|c| c.name().to_owned()).collect();
            }
            let jrow: Vec<Value> = (0..row.columns().len())
                .map(|i| decode_col(&row, i))
                .collect();
            bytes += serde_json::to_vec(&jrow)
                .map(|b| b.len())
                .unwrap_or(usize::MAX);
            if bytes > limits.max_result_bytes {
                return Err(result_too_large("bytes", limits.max_result_bytes));
            }
            rows.push(jrow);
        }
        Ok(())
    }
    .await;

    // ALWAYS roll back — success or error, nothing is ever committed.
    if let Err(e) = tx.rollback().await {
        tracing::warn!(error = %e, "query-offload: rollback failed (connection will be discarded)");
    }
    result?;

    Ok(QueryOutcome {
        columns,
        rows,
        skipped_rows: 0,
        rows_scanned: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(sql: &str) -> BoundedQuery {
        BoundedQuery::try_from(sql).unwrap()
    }

    fn validate(sql: &str) -> Result<ValidatedSql, McpError> {
        validate_sql(&q(sql))
    }

    // ── parser allowlist (no DB needed) ──────────────────────────────────

    #[test]
    fn accepts_single_select_and_with_select() {
        for ok in [
            "SELECT 1",
            "SELECT region, sum(amount) FROM orders GROUP BY region",
            "WITH t AS (SELECT 1 AS x) SELECT * FROM t",
            "SELECT * FROM a JOIN b ON a.id = b.id WHERE a.v > 3 ORDER BY a.id LIMIT 5",
            "SELECT 1; -- trailing comment",
        ] {
            assert!(validate(ok).is_ok(), "{ok} should pass");
        }
    }

    #[test]
    fn rejects_multi_statement_including_comment_obfuscated() {
        for bad in [
            "SELECT 1; SELECT 2",
            "SELECT 1; DROP TABLE users",
            "SELECT 1 /* hide */; DELETE FROM users",
            "SELECT 1;\n-- comment\nUPDATE t SET a = 1",
        ] {
            let err = validate(bad).unwrap_err();
            assert!(
                matches!(err, McpError::InvalidParams(_)),
                "{bad} must be InvalidParams"
            );
        }
    }

    #[test]
    fn rejects_non_select_statements() {
        for bad in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "CREATE TABLE t (a int)",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN b int",
            "TRUNCATE t",
            "COPY t FROM '/etc/passwd'",
            "SET search_path TO public",
            "CALL my_proc()",
            "EXPLAIN SELECT 1",
            "EXPLAIN ANALYZE SELECT 1",
            "GRANT ALL ON t TO public",
        ] {
            assert!(validate(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn rejects_empty_sql() {
        assert!(validate("").is_err());
        assert!(validate("  -- just a comment").is_err());
    }

    #[test]
    fn rejects_denylisted_functions_everywhere_in_the_ast() {
        for bad in [
            // plain call
            "SELECT pg_read_file('/etc/passwd')",
            "SELECT pg_read_binary_file('/etc/passwd')",
            "SELECT lo_import('/etc/passwd')",
            "SELECT lo_export(1234, '/tmp/out')",
            "SELECT pg_ls_dir('.')",
            // dblink family (incl. suffixed variants), as a table function
            "SELECT * FROM dblink('host=evil', 'select 1') AS t(a int)",
            "SELECT * FROM dblink_connect('host=evil')",
            // inside a CTE
            "WITH c AS (SELECT pg_ls_dir('.') AS f) SELECT * FROM c",
            // inside a scalar subquery
            "SELECT (SELECT pg_read_file('/etc/passwd'))",
            // inside a where clause / function args
            "SELECT 1 WHERE length(pg_read_file('/x')) > 0",
            // case-insensitive
            "SELECT PG_READ_FILE('/x')",
            // quoted identifier
            r#"SELECT "pg_read_file"('/x')"#,
            // schema-qualified
            "SELECT pg_catalog.pg_read_file('/x')",
        ] {
            let err = validate(bad).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("not allowed") || msg.contains("rejected"),
                "{bad} must be denylisted, got: {msg}"
            );
        }
    }

    #[test]
    fn rejects_select_into() {
        let err = validate("SELECT * INTO newtab FROM t").unwrap_err();
        assert!(err.to_string().contains("INTO"), "{err}");
    }

    #[test]
    fn allows_innocent_functions() {
        for ok in [
            "SELECT count(*), sum(x), avg(y) FROM t",
            "SELECT length('abc'), now(), coalesce(a, b) FROM t",
        ] {
            assert!(validate(ok).is_ok(), "{ok} should pass");
        }
    }

    // ── integration (require TEST_DATABASE_URL; run with --include-ignored) ──

    async fn test_pool() -> sqlx::PgPool {
        let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect TEST_DATABASE_URL")
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL (Postgres) — run with --include-ignored"]
    async fn select_works_inside_read_only_tx() {
        let pool = test_pool().await;
        let sql = validate("SELECT 1 AS one, 'a' AS letter").unwrap();
        let out = run(&pool, &sql, &QueryLimits::default()).await.unwrap();
        assert_eq!(out.columns, vec!["one", "letter"]);
        assert_eq!(
            out.rows,
            vec![vec![serde_json::json!(1), serde_json::json!("a")]]
        );
        assert_eq!(out.rows_scanned, None, "no fabricated pg scan numbers");
    }

    /// Even if a parser bug let a write through, the READ ONLY transaction
    /// blocks it. `ValidatedSql` is constructed directly (test-only, same
    /// module) to bypass the parser gate on purpose.
    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL (Postgres) — run with --include-ignored"]
    async fn read_only_tx_blocks_writes_even_past_the_parser() {
        let pool = test_pool().await;
        sqlx::query("CREATE TABLE IF NOT EXISTS tt_mcp_ro_probe (id int)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE SEQUENCE IF NOT EXISTS tt_mcp_ro_seq")
            .execute(&pool)
            .await
            .unwrap();

        for bypass in [
            "INSERT INTO tt_mcp_ro_probe VALUES (1)",
            "SELECT nextval('tt_mcp_ro_seq')",
            "CREATE TABLE tt_mcp_ro_probe2 (id int)",
        ] {
            let sql = ValidatedSql(bypass.to_owned());
            let err = run(&pool, &sql, &QueryLimits::default()).await.unwrap_err();
            assert!(
                err.to_string().contains("read-only"),
                "{bypass} must be blocked by the read-only tx: {err}"
            );
        }
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tt_mcp_ro_probe")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "nothing was committed");

        sqlx::query("DROP TABLE tt_mcp_ro_probe")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DROP SEQUENCE tt_mcp_ro_seq")
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL (Postgres) — run with --include-ignored"]
    async fn statement_timeout_aborts_pg_sleep() {
        let pool = test_pool().await;
        let limits = QueryLimits {
            statement_timeout_ms: 200,
            ..QueryLimits::default()
        };
        let sql = validate("SELECT pg_sleep(5)").unwrap();
        let err = run(&pool, &sql, &limits).await.unwrap_err();
        assert!(
            err.to_string().contains("timeout") || err.to_string().contains("cancel"),
            "pg_sleep must be aborted by statement_timeout: {err}"
        );
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL (Postgres) — run with --include-ignored"]
    async fn row_cap_stops_the_stream_with_a_hard_error() {
        let pool = test_pool().await;
        let limits = QueryLimits {
            max_result_rows: 10,
            ..QueryLimits::default()
        };
        let sql = validate("SELECT generate_series(1, 100000)").unwrap();
        let err = run(&pool, &sql, &limits).await.unwrap_err();
        assert!(err.to_string().contains("result_too_large"), "{err}");
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL (Postgres) — run with --include-ignored"]
    async fn byte_cap_is_enforced_while_streaming() {
        let pool = test_pool().await;
        let limits = QueryLimits {
            max_result_bytes: 256,
            ..QueryLimits::default()
        };
        let sql = validate("SELECT repeat('x', 1000) FROM generate_series(1, 10)").unwrap();
        let err = run(&pool, &sql, &limits).await.unwrap_err();
        assert!(err.to_string().contains("result_too_large"), "{err}");
    }
}
