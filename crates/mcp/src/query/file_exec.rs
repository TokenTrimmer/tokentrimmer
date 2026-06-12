//! Streaming CSV/JSONL aggregation executor.
//!
//! O(1)-per-row memory (group state only), never materializes the file.
//! Re-canonicalizes the path and re-checks containment under the
//! operator-allowlisted root at EVERY call (symlink-swap defense; the
//! residual open-vs-check TOCTOU is accepted residual risk, see module docs
//! in [`crate::query`]). Malformed rows are counted and surfaced as
//! `skipped_rows`, never silently dropped. Error messages never echo the
//! file path (it would leak operator filesystem layout into model context).

use std::collections::{BTreeMap, HashSet};
use std::io::BufRead;
use std::path::Path;

use serde_json::Value;

use crate::error::McpError;

use super::handle::FileFormat;
use super::spec::{AggOp, AggregationSpec, CmpOp, Predicate};
use super::{result_too_large, QueryLimits, QueryOutcome, MAX_RESULT_ROWS_CEILING};

/// Memory guard for `count_distinct` accumulation.
const MAX_DISTINCT_VALUES: usize = 100_000;

/// One parsed input row, as column → string value lookups.
trait Row {
    fn get(&self, column: &str) -> Option<String>;
}

struct CsvRow<'a> {
    headers: &'a csv::StringRecord,
    record: &'a csv::StringRecord,
}

impl Row for CsvRow<'_> {
    fn get(&self, column: &str) -> Option<String> {
        let idx = self.headers.iter().position(|h| h == column)?;
        self.record.get(idx).map(|s| s.to_owned())
    }
}

struct JsonRow<'a>(&'a serde_json::Map<String, Value>);

impl Row for JsonRow<'_> {
    fn get(&self, column: &str) -> Option<String> {
        match self.0.get(column)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Null => None,
            // Nested arrays/objects have no scalar comparison semantics.
            _ => None,
        }
    }
}

/// Compare two scalar strings: numerically when both parse as f64, else
/// lexicographically.
fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => a.cmp(b),
    }
}

fn predicate_matches(p: &Predicate, value: &str) -> bool {
    let ord = compare(value, p.value.as_str());
    match p.op {
        CmpOp::Eq => ord == std::cmp::Ordering::Equal,
        CmpOp::Ne => ord != std::cmp::Ordering::Equal,
        CmpOp::Lt => ord == std::cmp::Ordering::Less,
        CmpOp::Lte => ord != std::cmp::Ordering::Greater,
        CmpOp::Gt => ord == std::cmp::Ordering::Greater,
        CmpOp::Gte => ord != std::cmp::Ordering::Less,
    }
}

/// Per-group accumulator.
enum Accum {
    Count(u64),
    Sum { sum: f64 },
    Avg { sum: f64, n: u64 },
    Min(Option<f64>),
    Max(Option<f64>),
    CountDistinct(HashSet<String>),
}

impl Accum {
    fn new(op: AggOp) -> Self {
        match op {
            AggOp::Count => Self::Count(0),
            AggOp::Sum => Self::Sum { sum: 0.0 },
            AggOp::Avg => Self::Avg { sum: 0.0, n: 0 },
            AggOp::Min => Self::Min(None),
            AggOp::Max => Self::Max(None),
            AggOp::CountDistinct => Self::CountDistinct(HashSet::new()),
        }
    }

    /// Feed one row's target value (`None` for plain `count`, which needs no
    /// column). Returns `false` when the value was unusable (non-numeric for
    /// a numeric op) so the caller can count the row as skipped.
    fn feed(&mut self, value: Option<&str>) -> Result<bool, McpError> {
        match self {
            Self::Count(n) => {
                *n += 1;
                Ok(true)
            }
            Self::Sum { sum } => match value.and_then(|v| v.trim().parse::<f64>().ok()) {
                Some(x) => {
                    *sum += x;
                    Ok(true)
                }
                None => Ok(false),
            },
            Self::Avg { sum, n } => match value.and_then(|v| v.trim().parse::<f64>().ok()) {
                Some(x) => {
                    *sum += x;
                    *n += 1;
                    Ok(true)
                }
                None => Ok(false),
            },
            Self::Min(cur) => match value.and_then(|v| v.trim().parse::<f64>().ok()) {
                Some(x) => {
                    *cur = Some(cur.map_or(x, |c| c.min(x)));
                    Ok(true)
                }
                None => Ok(false),
            },
            Self::Max(cur) => match value.and_then(|v| v.trim().parse::<f64>().ok()) {
                Some(x) => {
                    *cur = Some(cur.map_or(x, |c| c.max(x)));
                    Ok(true)
                }
                None => Ok(false),
            },
            Self::CountDistinct(set) => match value {
                Some(v) => {
                    if set.len() >= MAX_DISTINCT_VALUES && !set.contains(v) {
                        return Err(McpError::InvalidParams(format!(
                            "count_distinct cardinality exceeds {MAX_DISTINCT_VALUES}; \
                             narrow the query with `where` predicates"
                        )));
                    }
                    set.insert(v.to_owned());
                    Ok(true)
                }
                None => Ok(false),
            },
        }
    }

    fn finish(&self) -> Value {
        match self {
            Self::Count(n) => Value::from(*n),
            Self::Sum { sum } => Value::from(*sum),
            Self::Avg { sum, n } => {
                if *n == 0 {
                    Value::Null
                } else {
                    Value::from(*sum / *n as f64)
                }
            }
            Self::Min(v) | Self::Max(v) => v.map_or(Value::Null, Value::from),
            Self::CountDistinct(set) => Value::from(set.len() as u64),
        }
    }
}

fn agg_column_name(spec: &AggregationSpec) -> String {
    let op = match spec.op {
        AggOp::Count => "count",
        AggOp::Sum => "sum",
        AggOp::Avg => "avg",
        AggOp::Min => "min",
        AggOp::Max => "max",
        AggOp::CountDistinct => "count_distinct",
    };
    match &spec.column {
        Some(c) => format!("{op}_{}", c.as_str()),
        None => op.to_owned(),
    }
}

/// Run a bounded aggregation over the file at `path` (canonicalized at boot)
/// under `root`. Re-canonicalizes and re-checks containment first.
pub(crate) fn run(
    root: &Path,
    path: &Path,
    format: FileFormat,
    spec: &AggregationSpec,
    limits: &QueryLimits,
) -> Result<QueryOutcome, McpError> {
    // Canonicalize-at-call: the boot-time path may have been swapped for a
    // symlink since. Never echo the path in errors.
    let canonical = path.canonicalize().map_err(|e| {
        McpError::Internal(format!(
            "dataset file is unavailable ({kind}); ask the operator to check the \
             dataset registration",
            kind = e.kind()
        ))
    })?;
    if !canonical.starts_with(root) {
        return Err(McpError::Internal(
            "dataset file no longer resolves under the operator-allowlisted root; \
             refusing to read it"
                .into(),
        ));
    }
    let meta = std::fs::metadata(&canonical)
        .map_err(|e| McpError::Internal(format!("dataset file is unavailable ({})", e.kind())))?;
    if !meta.is_file() {
        return Err(McpError::Internal(
            "dataset path is not a regular file; refusing to read it".into(),
        ));
    }

    let mut state = AggState::new(spec, limits);
    match format {
        FileFormat::Csv => {
            let mut rdr = csv::ReaderBuilder::new()
                .flexible(true)
                .from_path(&canonical)
                .map_err(|_| McpError::Internal("dataset file could not be opened".into()))?;
            let headers = rdr
                .headers()
                .map_err(|_| McpError::Internal("dataset CSV header is unreadable".into()))?
                .clone();
            for record in rdr.records() {
                match record {
                    Ok(rec) => state.feed(&CsvRow {
                        headers: &headers,
                        record: &rec,
                    })?,
                    Err(_) => state.skip(),
                }
            }
        }
        FileFormat::Jsonl => {
            let f = std::fs::File::open(&canonical)
                .map_err(|_| McpError::Internal("dataset file could not be opened".into()))?;
            for line in std::io::BufReader::new(f).lines() {
                let line =
                    line.map_err(|_| McpError::Internal("dataset file read failed".into()))?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(Value::Object(map)) => state.feed(&JsonRow(&map))?,
                    _ => state.skip(),
                }
            }
        }
    }
    state.finish(spec, limits)
}

/// Streaming aggregation state: predicate filter → group key → accumulator.
struct AggState<'a> {
    spec: &'a AggregationSpec,
    groups: BTreeMap<Vec<String>, Accum>,
    rows_scanned: u64,
    skipped: u64,
}

impl<'a> AggState<'a> {
    fn new(spec: &'a AggregationSpec, _limits: &QueryLimits) -> Self {
        Self {
            spec,
            groups: BTreeMap::new(),
            rows_scanned: 0,
            skipped: 0,
        }
    }

    fn skip(&mut self) {
        self.rows_scanned += 1;
        self.skipped += 1;
    }

    fn feed(&mut self, row: &dyn Row) -> Result<(), McpError> {
        self.rows_scanned += 1;

        // Predicates: a row missing a predicate column is malformed for this
        // query — counted as skipped, not silently unmatched.
        let mut matches = true;
        for p in &self.spec.predicates {
            match row.get(p.column.as_str()) {
                Some(v) => {
                    if !predicate_matches(p, &v) {
                        matches = false;
                        break;
                    }
                }
                None => {
                    self.skipped += 1;
                    return Ok(());
                }
            }
        }
        if !matches {
            return Ok(());
        }

        // Group key.
        let mut key = Vec::with_capacity(self.spec.group_by.len());
        for g in &self.spec.group_by {
            match row.get(g.as_str()) {
                Some(v) => key.push(v),
                None => {
                    self.skipped += 1;
                    return Ok(());
                }
            }
        }

        // Memory guard on group cardinality (hard ceiling, not the operator
        // row cap: `limit` may legitimately head a larger group set).
        if !self.groups.contains_key(&key) && self.groups.len() >= MAX_RESULT_ROWS_CEILING {
            return Err(result_too_large("rows", MAX_RESULT_ROWS_CEILING));
        }

        let value = match &self.spec.column {
            Some(c) => match row.get(c.as_str()) {
                Some(v) => Some(v),
                None => {
                    self.skipped += 1;
                    return Ok(());
                }
            },
            None => None,
        };

        let acc = self
            .groups
            .entry(key)
            .or_insert_with(|| Accum::new(self.spec.op));
        if !acc.feed(value.as_deref())? {
            // Non-numeric value for a numeric op: surfaced, not dropped.
            self.skipped += 1;
        }
        Ok(())
    }

    fn finish(
        mut self,
        spec: &AggregationSpec,
        limits: &QueryLimits,
    ) -> Result<QueryOutcome, McpError> {
        // No group_by and no matching rows: still emit the single aggregate
        // row (count 0 / null), like SQL aggregates without GROUP BY.
        if spec.group_by.is_empty() && self.groups.is_empty() {
            self.groups.insert(Vec::new(), Accum::new(spec.op));
        }

        let mut columns: Vec<String> =
            spec.group_by.iter().map(|g| g.as_str().to_owned()).collect();
        columns.push(agg_column_name(spec));

        // BTreeMap iteration is key-sorted: deterministic output order.
        let mut rows: Vec<Vec<Value>> = self
            .groups
            .iter()
            .map(|(key, acc)| {
                let mut row: Vec<Value> =
                    key.iter().map(|k| Value::from(k.as_str())).collect();
                row.push(acc.finish());
                row
            })
            .collect();

        // Caller-explicit head (not silent truncation: the caller asked).
        if let Some(limit) = spec.limit {
            rows.truncate(limit as usize);
        }

        if rows.len() > limits.max_result_rows {
            return Err(result_too_large("rows", limits.max_result_rows));
        }
        let bytes = serde_json::to_vec(&rows).map_err(|e| McpError::Internal(e.to_string()))?;
        if bytes.len() > limits.max_result_bytes {
            return Err(result_too_large("bytes", limits.max_result_bytes));
        }

        Ok(QueryOutcome {
            columns,
            rows,
            skipped_rows: self.skipped,
            rows_scanned: Some(self.rows_scanned),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(v: serde_json::Value) -> AggregationSpec {
        serde_json::from_value(v).expect("valid spec")
    }

    /// Write `content` into a tempdir and return (dir, canonical root, path).
    fn dataset(content: &str, name: &str) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(name);
        std::fs::write(&path, content).unwrap();
        (dir, root, path)
    }

    const ORDERS_CSV: &str = "region,amount\nemea,10.5\napac,2\nemea,4.5\nus,1\n";
    const ORDERS_JSONL: &str = r#"{"region":"emea","amount":10.5}
{"region":"apac","amount":2}
{"region":"emea","amount":4.5}
{"region":"us","amount":1}
"#;

    fn run_csv(content: &str, s: AggregationSpec, limits: &QueryLimits) -> Result<QueryOutcome, McpError> {
        let (_dir, root, path) = dataset(content, "orders.csv");
        run(&root, &path, FileFormat::Csv, &s, limits)
    }

    fn run_jsonl(content: &str, s: AggregationSpec, limits: &QueryLimits) -> Result<QueryOutcome, McpError> {
        let (_dir, root, path) = dataset(content, "orders.jsonl");
        run(&root, &path, FileFormat::Jsonl, &s, limits)
    }

    #[test]
    fn csv_count_all() {
        let out = run_csv(ORDERS_CSV, spec(json!({"op":"count"})), &QueryLimits::default()).unwrap();
        assert_eq!(out.columns, vec!["count"]);
        assert_eq!(out.rows, vec![vec![json!(4)]]);
        assert_eq!(out.rows_scanned, Some(4));
        assert_eq!(out.skipped_rows, 0);
    }

    #[test]
    fn csv_sum_avg_min_max_with_where() {
        let s = spec(json!({"op":"sum","column":"amount","where":[{"column":"region","op":"eq","value":"emea"}]}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.columns, vec!["sum_amount"]);
        assert_eq!(out.rows, vec![vec![json!(15.0)]]);

        let s = spec(json!({"op":"avg","column":"amount"}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(4.5)]]);

        let s = spec(json!({"op":"min","column":"amount"}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(1.0)]]);

        let s = spec(json!({"op":"max","column":"amount"}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(10.5)]]);
    }

    #[test]
    fn csv_group_by_is_sorted_and_correct() {
        let s = spec(json!({"op":"sum","column":"amount","group_by":["region"]}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.columns, vec!["region", "sum_amount"]);
        assert_eq!(
            out.rows,
            vec![
                vec![json!("apac"), json!(2.0)],
                vec![json!("emea"), json!(15.0)],
                vec![json!("us"), json!(1.0)],
            ]
        );
    }

    #[test]
    fn csv_count_distinct() {
        let s = spec(json!({"op":"count_distinct","column":"region"}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(3)]]);
    }

    #[test]
    fn csv_numeric_comparison_predicates() {
        // "10.5" > "2" numerically though not lexicographically.
        let s = spec(json!({"op":"count","where":[{"column":"amount","op":"gt","value":"2"}]}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(2)]], "10.5 and 4.5 are > 2");
    }

    #[test]
    fn jsonl_equivalents() {
        let s = spec(json!({"op":"sum","column":"amount","group_by":["region"]}));
        let out = run_jsonl(ORDERS_JSONL, s, &QueryLimits::default()).unwrap();
        assert_eq!(
            out.rows,
            vec![
                vec![json!("apac"), json!(2.0)],
                vec![json!("emea"), json!(15.0)],
                vec![json!("us"), json!(1.0)],
            ]
        );
        let s = spec(json!({"op":"count"}));
        let out = run_jsonl(ORDERS_JSONL, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(4)]]);
    }

    #[test]
    fn malformed_rows_are_counted_not_dropped() {
        // Row 2 has a non-numeric amount; row 4 is junk JSON.
        let content = r#"{"region":"emea","amount":1}
{"region":"emea","amount":"oops"}
{"region":"apac","amount":2}
this is not json
"#;
        let s = spec(json!({"op":"sum","column":"amount"}));
        let out = run_jsonl(content, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(3.0)]]);
        assert_eq!(out.skipped_rows, 2, "non-numeric + junk row both surfaced");
        assert_eq!(out.rows_scanned, Some(4));
    }

    #[test]
    fn csv_missing_column_rows_are_skipped() {
        // Second record has only one field (flexible CSV).
        let content = "region,amount\nemea,1\napac\n";
        let s = spec(json!({"op":"sum","column":"amount"}));
        let out = run_csv(content, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(1.0)]]);
        assert_eq!(out.skipped_rows, 1);
    }

    #[test]
    fn row_cap_exceeded_is_hard_error_advising_tighter_aggregation() {
        let mut csv = String::from("id,v\n");
        for i in 0..5 {
            csv.push_str(&format!("{i},1\n"));
        }
        let limits = QueryLimits {
            max_result_rows: 3,
            ..QueryLimits::default()
        };
        let s = spec(json!({"op":"count","group_by":["id"]}));
        let err = run_csv(&csv, s, &limits).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("result_too_large"), "{msg}");
        assert!(msg.contains("tighten the aggregation"), "{msg}");
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    #[test]
    fn byte_cap_exceeded_is_hard_error() {
        let mut csv = String::from("id,v\n");
        for i in 0..50 {
            csv.push_str(&format!("group-with-a-long-name-{i},1\n"));
        }
        let limits = QueryLimits {
            max_result_bytes: 64,
            ..QueryLimits::default()
        };
        let s = spec(json!({"op":"count","group_by":["id"]}));
        let err = run_csv(&csv, s, &limits).unwrap_err();
        assert!(err.to_string().contains("result_too_large"), "{err}");
    }

    #[test]
    fn caller_limit_heads_the_sorted_groups() {
        let s = spec(json!({"op":"sum","column":"amount","group_by":["region"],"limit":2}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.rows[0][0], json!("apac"));
    }

    #[test]
    fn empty_match_still_emits_one_aggregate_row() {
        let s = spec(json!({"op":"sum","column":"amount","where":[{"column":"region","op":"eq","value":"nowhere"}]}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(0.0)]]);

        let s = spec(json!({"op":"avg","column":"amount","where":[{"column":"region","op":"eq","value":"nowhere"}]}));
        let out = run_csv(ORDERS_CSV, s, &QueryLimits::default()).unwrap();
        assert_eq!(out.rows, vec![vec![json!(null)]], "avg of nothing is null, not a fabricated 0");
    }

    /// Symlink-swap defense: a symlink INSIDE the root pointing OUTSIDE it is
    /// rejected at call time, with no path echoed.
    #[cfg(unix)]
    #[test]
    fn symlink_escaping_root_is_rejected_at_call() {
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.csv");
        std::fs::write(&secret, "a,b\n1,2\n").unwrap();

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let link = root.join("orders.csv");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let err = run(
            &root,
            &link,
            FileFormat::Csv,
            &spec(json!({"op":"count"})),
            &QueryLimits::default(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("allowlisted root"), "{msg}");
        assert!(
            !msg.contains(outside.path().to_str().unwrap()),
            "must not leak the target path: {msg}"
        );
    }

    /// File deleted between boot and call → clean Internal error, no panic,
    /// no path leak.
    #[test]
    fn deleted_file_is_clean_internal_error() {
        let (dir, root, path) = dataset(ORDERS_CSV, "orders.csv");
        std::fs::remove_file(&path).unwrap();
        let err = run(
            &root,
            &path,
            FileFormat::Csv,
            &spec(json!({"op":"count"})),
            &QueryLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(err, McpError::Internal(_)));
        assert!(
            !err.to_string().contains(dir.path().to_str().unwrap()),
            "must not leak the path: {err}"
        );
    }
}
