//! Size-capped caller input types — the anti-smuggling gate.
//!
//! Every string a caller can put in `run_query` arguments is a bounded
//! newtype whose ONLY constructor enforces a byte cap; `Deserialize`
//! delegates to that constructor, so an oversized value is rejected during
//! deserialization, before any parsing or I/O. The caps are sized so the
//! parameters are useless as a data channel: a useful CSV cannot fit in
//! 4 KiB of query text, and the whole serialized arguments object is gated
//! at [`MAX_ARGS_BYTES`] before deserialization even starts (see
//! `RunQueryTool`). This is the enforcement half of the inline-offload gate;
//! the other half is [`crate::query::handle::DatasetHandle`] not
//! implementing `Deserialize` at all.

use serde::Deserialize;

/// Max byte length of SQL query text. A useful dataset cannot fit.
pub const MAX_QUERY_BYTES: usize = 4096;

/// Outer gate on the whole serialized `arguments` object of a `run_query`
/// call. Checked before typed deserialization, so a smuggled blob never
/// reaches field handling or an executor. (The JSON-RPC transport has
/// already parsed the request envelope by then — that earlier parse is
/// bounded by the transport-level caps: 1 MiB per stdio line, 1 MiB per
/// HTTP body.)
pub const MAX_ARGS_BYTES: usize = 8192;

/// Max number of `where` predicates in an aggregation spec.
pub const MAX_PREDICATES: usize = 8;

/// Max number of `group_by` columns in an aggregation spec.
pub const MAX_GROUP_BY: usize = 4;

/// Size-capped query text (SQL for Postgres handles).
///
/// INVARIANT: `len() <= MAX_QUERY_BYTES`, enforced in the only constructor
/// (`TryFrom<&str>`); `Deserialize` delegates to it. The cap is the
/// anti-smuggling gate: the parameter is useless as a data channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedQuery(String);

impl BoundedQuery {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for BoundedQuery {
    type Error = String;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        if s.len() > MAX_QUERY_BYTES {
            return Err(format!(
                "query text is {} bytes; max {MAX_QUERY_BYTES} — `run_query` accepts a \
                 short query, never inline data (register the data as an operator \
                 dataset instead)",
                s.len()
            ));
        }
        Ok(Self(s.to_owned()))
    }
}

impl<'de> Deserialize<'de> for BoundedQuery {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::try_from(s.as_str()).map_err(serde::de::Error::custom)
    }
}

/// Size-capped string for `AggregationSpec` fields (column names, predicate
/// values). INVARIANT: `len() <= N` bytes, enforced in the only constructor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct BoundedStr<const N: usize>(String);

impl<const N: usize> BoundedStr<N> {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const N: usize> TryFrom<&str> for BoundedStr<N> {
    type Error = String;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        if s.len() > N {
            return Err(format!("string is {} bytes; max {N}", s.len()));
        }
        Ok(Self(s.to_owned()))
    }
}

impl<'de, const N: usize> Deserialize<'de> for BoundedStr<N> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::try_from(s.as_str()).map_err(serde::de::Error::custom)
    }
}

/// Deserialize a `Vec<T>` and reject it when it has more than `MAX` items.
/// Used for the predicate / group_by caps in `AggregationSpec`.
pub(crate) fn bounded_vec<'de, D, T, const MAX: usize>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    let v = Vec::<T>::deserialize(d)?;
    if v.len() > MAX {
        return Err(serde::de::Error::custom(format!(
            "list has {} items; max {MAX}",
            v.len()
        )));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE smuggling test: a ~10 KiB CSV pasted into `query` fails before any
    /// parse or I/O.
    #[test]
    fn bounded_query_rejects_pasted_csv() {
        let csv_blob = "region,amount\n".repeat(800); // ~11 KiB
        assert!(csv_blob.len() > MAX_QUERY_BYTES);
        let err = BoundedQuery::try_from(csv_blob.as_str()).unwrap_err();
        assert!(err.contains("never inline data"), "err was: {err}");
    }

    #[test]
    fn bounded_query_accepts_up_to_cap() {
        let at_cap = "x".repeat(MAX_QUERY_BYTES);
        assert!(BoundedQuery::try_from(at_cap.as_str()).is_ok());
        let over = "x".repeat(MAX_QUERY_BYTES + 1);
        assert!(BoundedQuery::try_from(over.as_str()).is_err());
    }

    /// The cap is in bytes; multi-byte UTF-8 must not panic or slip through.
    #[test]
    fn bounded_query_cap_is_utf8_boundary_safe() {
        // 'é' is 2 bytes: 2049 of them = 4098 bytes > 4096.
        let s = "é".repeat(MAX_QUERY_BYTES / 2 + 1);
        assert!(s.len() > MAX_QUERY_BYTES);
        assert!(BoundedQuery::try_from(s.as_str()).is_err());
        let ok = "é".repeat(MAX_QUERY_BYTES / 2); // exactly 4096 bytes
        assert!(BoundedQuery::try_from(ok.as_str()).is_ok());
    }

    #[test]
    fn bounded_query_deserialize_delegates_to_ctor() {
        let over = serde_json::json!("x".repeat(MAX_QUERY_BYTES + 1));
        assert!(serde_json::from_value::<BoundedQuery>(over).is_err());
        let ok = serde_json::json!("select 1");
        assert_eq!(
            serde_json::from_value::<BoundedQuery>(ok).unwrap().as_str(),
            "select 1"
        );
    }

    #[test]
    fn bounded_str_rejects_oversize_on_deserialize() {
        let over = serde_json::json!("c".repeat(129));
        assert!(serde_json::from_value::<BoundedStr<128>>(over).is_err());
        let ok = serde_json::json!("c".repeat(128));
        assert!(serde_json::from_value::<BoundedStr<128>>(ok).is_ok());
    }

    #[derive(Deserialize)]
    struct CapList {
        #[serde(deserialize_with = "bounded_vec::<_, _, 3>")]
        items: Vec<u32>,
    }

    #[test]
    fn bounded_vec_rejects_over_max_items() {
        let over = serde_json::json!({ "items": [1, 2, 3, 4] });
        assert!(serde_json::from_value::<CapList>(over).is_err());
        let ok = serde_json::json!({ "items": [1, 2, 3] });
        assert_eq!(
            serde_json::from_value::<CapList>(ok).unwrap().items.len(),
            3
        );
    }
}
