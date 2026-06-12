//! What a `run_query` caller CAN express — fully bounded.
//!
//! Every field is a bounded newtype ([`BoundedQuery`], [`BoundedStr`]) or a
//! length-capped list, so the deserialized spec is incapable of carrying a
//! dataset. File handles get a structured [`AggregationSpec`] (not free-form
//! SQL): the query surface stays parser-validated and bounded without a
//! heavyweight SQL-on-CSV engine.

use serde::{Deserialize, Serialize};

use super::bounded::{bounded_vec, BoundedQuery, BoundedStr, MAX_GROUP_BY, MAX_PREDICATES};
use super::handle::HandleAlias;

/// Deserialized `run_query` arguments. `query` (SQL, Postgres handles) and
/// `aggregate` (file handles) are mutually exclusive — exactly one must be
/// present (checked by [`RunQueryArgs::into_parts`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunQueryArgs {
    /// Selects an operator-registered dataset by NAME only.
    pub dataset: HandleAlias,
    #[serde(default)]
    query: Option<BoundedQuery>,
    #[serde(default)]
    aggregate: Option<AggregationSpec>,
    /// When true, the query is executed twice (cache bypassed) and the
    /// results compared — the honest catch for silent-wrong-number failures.
    #[serde(default)]
    pub verify: bool,
}

impl RunQueryArgs {
    /// Split into `(dataset, spec, verify)`, enforcing the XOR between
    /// `query` and `aggregate`.
    pub fn into_parts(self) -> Result<(HandleAlias, QuerySpec, bool), String> {
        let spec = match (self.query, self.aggregate) {
            (Some(q), None) => QuerySpec::Sql(q),
            (None, Some(a)) => {
                a.validate()?;
                QuerySpec::Aggregate(a)
            }
            (Some(_), Some(_)) => {
                return Err(
                    "provide exactly one of `query` (SQL, postgres datasets) or \
                            `aggregate` (file datasets), not both"
                        .into(),
                )
            }
            (None, None) => {
                return Err("provide one of `query` (SQL, postgres datasets) or \
                            `aggregate` (file datasets)"
                    .into())
            }
        };
        Ok((self.dataset, spec, self.verify))
    }
}

/// A bounded, validated-at-deserialization query.
#[derive(Debug, Clone)]
pub enum QuerySpec {
    /// Postgres handles: SQL text, byte-capped BEFORE parsing, then
    /// sqlparser-gated (see `pg_exec`).
    Sql(BoundedQuery),
    /// File handles: structured aggregation — NOT free-form SQL.
    Aggregate(AggregationSpec),
}

/// Aggregation operations supported over file datasets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggOp {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    CountDistinct,
}

/// Comparison operators for `where` predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
}

/// One bounded `where` predicate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Predicate {
    pub column: BoundedStr<128>,
    pub op: CmpOp,
    pub value: BoundedStr<256>,
}

/// Structured aggregation over a file dataset. All strings bounded, all
/// lists length-capped at deserialization.
///
/// `Serialize` exists so the result cache can key on the **canonical**
/// serialization (struct field order is fixed, so two JSON inputs that
/// differ only in key order produce the same key).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregationSpec {
    pub op: AggOp,
    #[serde(default)]
    pub column: Option<BoundedStr<128>>,
    #[serde(
        default,
        rename = "where",
        deserialize_with = "bounded_vec::<_, _, MAX_PREDICATES>"
    )]
    pub predicates: Vec<Predicate>,
    #[serde(default, deserialize_with = "bounded_vec::<_, _, MAX_GROUP_BY>")]
    pub group_by: Vec<BoundedStr<128>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl AggregationSpec {
    /// Structural validation that deserialization alone cannot express.
    pub fn validate(&self) -> Result<(), String> {
        match self.op {
            AggOp::Count => Ok(()),
            _ if self.column.is_none() => {
                Err(format!("aggregation op {:?} requires a `column`", self.op))
            }
            _ => Ok(()),
        }
    }

    /// Canonical key material for the result cache: the serde serialization
    /// of the typed spec (field order is struct-defined, hence stable).
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).expect("AggregationSpec serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_args(v: serde_json::Value) -> Result<RunQueryArgs, serde_json::Error> {
        serde_json::from_value(v)
    }

    #[test]
    fn args_require_exactly_one_of_query_or_aggregate() {
        let both = parse_args(json!({
            "dataset": "orders",
            "query": "select 1",
            "aggregate": { "op": "count" }
        }))
        .unwrap();
        assert!(both.into_parts().is_err());

        let neither = parse_args(json!({ "dataset": "orders" })).unwrap();
        assert!(neither.into_parts().is_err());

        let sql = parse_args(json!({ "dataset": "orders", "query": "select 1" })).unwrap();
        let (alias, spec, verify) = sql.into_parts().unwrap();
        assert_eq!(alias.as_str(), "orders");
        assert!(matches!(spec, QuerySpec::Sql(_)));
        assert!(!verify, "verify defaults to false");
    }

    #[test]
    fn args_reject_unknown_fields() {
        // additionalProperties: false — a smuggling side-channel field fails.
        let r = parse_args(json!({
            "dataset": "orders",
            "query": "select 1",
            "data": "a,b,c\n1,2,3"
        }));
        assert!(r.is_err());
    }

    #[test]
    fn aggregate_rejects_too_many_predicates() {
        let preds: Vec<_> = (0..9)
            .map(|i| json!({ "column": "c", "op": "eq", "value": i.to_string() }))
            .collect();
        let r = serde_json::from_value::<AggregationSpec>(json!({
            "op": "count", "where": preds
        }));
        assert!(r.is_err(), "9 predicates must be rejected (max 8)");
    }

    #[test]
    fn aggregate_rejects_too_many_group_by() {
        let r = serde_json::from_value::<AggregationSpec>(json!({
            "op": "count", "group_by": ["a", "b", "c", "d", "e"]
        }));
        assert!(r.is_err(), "5 group_by columns must be rejected (max 4)");
    }

    #[test]
    fn aggregate_rejects_oversize_strings_in_fields() {
        let r = serde_json::from_value::<AggregationSpec>(json!({
            "op": "count",
            "where": [{ "column": "c", "op": "eq", "value": "v".repeat(257) }]
        }));
        assert!(r.is_err(), "predicate value > 256 bytes must be rejected");

        let r = serde_json::from_value::<AggregationSpec>(json!({
            "op": "sum", "column": "c".repeat(129)
        }));
        assert!(r.is_err(), "column > 128 bytes must be rejected");
    }

    #[test]
    fn non_count_ops_require_column() {
        for op in ["sum", "avg", "min", "max", "count_distinct"] {
            let spec = serde_json::from_value::<AggregationSpec>(json!({ "op": op })).unwrap();
            assert!(spec.validate().is_err(), "{op} without column must fail");
        }
        let count = serde_json::from_value::<AggregationSpec>(json!({ "op": "count" })).unwrap();
        assert!(count.validate().is_ok());
    }

    #[test]
    fn canonical_serialization_is_field_order_insensitive() {
        let a = serde_json::from_value::<AggregationSpec>(json!({
            "op": "sum", "column": "amount", "group_by": ["region"]
        }))
        .unwrap();
        let b = serde_json::from_value::<AggregationSpec>(json!({
            "group_by": ["region"], "column": "amount", "op": "sum"
        }))
        .unwrap();
        assert_eq!(a.canonical(), b.canonical());
    }
}
