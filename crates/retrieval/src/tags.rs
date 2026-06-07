//! Parse `<retrievable corpus="X" k="N">...</retrievable>` tags from message
//! text. Returns each tag's corpus, k, and span in the text.
//!
//! Two caps bound the work an untrusted message can request: a single tag's `k`
//! is clamped to [`MAX_RETRIEVAL_K`], and at most [`MAX_RETRIEVABLE_TAGS`] tags
//! are honored per message (the rest are ignored).

use regex::Regex;

use crate::error::RetrievalError;
use crate::types::RetrievableTag;

/// Maximum chunks a single `<retrievable>` tag may request. Caps the pgvector
/// `LIMIT` so an untrusted `k="4000000000"` cannot force an unbounded scan.
pub const MAX_RETRIEVAL_K: u32 = 50;

/// Maximum number of `<retrievable>` tags honored per message. Bounds the
/// per-message fan-out (one embedding search per tag). Tags beyond this are
/// ignored (the first `MAX_RETRIEVABLE_TAGS` in document order are kept).
pub const MAX_RETRIEVABLE_TAGS: usize = 16;

pub fn parse(text: &str) -> Result<Vec<RetrievableTag>, RetrievalError> {
    // Match opening tag attributes and payload. We capture the full attribute
    // string so we can extract `k` and `min_similarity` regardless of order.
    //
    // Pattern: <retrievable ATTRS>PAYLOAD</retrievable>
    // ATTRS is a non-greedy blob of attribute text (captured as group 1).
    // PAYLOAD is captured as group 2.
    let re = Regex::new(r#"(?ms)<retrievable\s+([^>]+?)>(.*?)</retrievable>"#)
        .map_err(|e| RetrievalError::Tag(e.to_string()))?;
    let k_re = Regex::new(r#"k="(\d+)""#).map_err(|e| RetrievalError::Tag(e.to_string()))?;
    let corpus_re =
        Regex::new(r#"corpus="([^"]+)""#).map_err(|e| RetrievalError::Tag(e.to_string()))?;
    let sim_re = Regex::new(r#"min_similarity="([^"]+)""#)
        .map_err(|e| RetrievalError::Tag(e.to_string()))?;

    let mut out = Vec::new();
    for m in re.captures_iter(text) {
        let full = m.get(0).unwrap();
        let attrs = m.get(1).unwrap().as_str();

        let corpus = corpus_re
            .captures(attrs)
            .and_then(|c| c.get(1))
            .map(|s| s.as_str().to_string())
            .ok_or_else(|| RetrievalError::Tag("missing corpus attribute".into()))?;

        let k = k_re
            .captures(attrs)
            .and_then(|c| c.get(1))
            .and_then(|s| s.as_str().parse::<u32>().ok())
            .unwrap_or(5)
            .min(MAX_RETRIEVAL_K);

        let min_similarity = sim_re
            .captures(attrs)
            .and_then(|c| c.get(1))
            .and_then(|s| s.as_str().parse::<f32>().ok());

        out.push(RetrievableTag {
            corpus,
            k,
            min_similarity,
            span: (full.start(), full.end()),
        });
    }
    out.truncate(MAX_RETRIEVABLE_TAGS);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_tag() {
        let t = parse(r#"Pre<retrievable corpus="docs" k="3">payload</retrievable>Post"#).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].corpus, "docs");
        assert_eq!(t[0].k, 3);
        assert_eq!(t[0].min_similarity, None);
    }

    #[test]
    fn default_k_when_missing() {
        let t = parse(r#"<retrievable corpus="x">y</retrievable>"#).unwrap();
        assert_eq!(t[0].k, 5);
        assert_eq!(t[0].min_similarity, None);
    }

    #[test]
    fn per_tag_min_similarity_parsed() {
        let t =
            parse(r#"<retrievable corpus="x" k="3" min_similarity="0.75">payload</retrievable>"#)
                .unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].corpus, "x");
        assert_eq!(t[0].k, 3);
        assert_eq!(t[0].min_similarity, Some(0.75));
    }

    #[test]
    fn multiple_tags_in_order() {
        let body =
            r#"a<retrievable corpus="x">1</retrievable>b<retrievable corpus="y">2</retrievable>c"#;
        let t = parse(body).unwrap();
        assert_eq!(t.len(), 2);
        assert!(t[0].span.0 < t[1].span.0);
    }

    #[test]
    fn no_tags_is_empty() {
        let t = parse("plain text").unwrap();
        assert!(t.is_empty());
    }

    #[test]
    fn k_is_clamped_to_max() {
        let t = parse(r#"<retrievable corpus="x" k="4000000000">y</retrievable>"#).unwrap();
        assert_eq!(t[0].k, MAX_RETRIEVAL_K);
    }

    #[test]
    fn k_under_cap_is_unchanged() {
        let t = parse(r#"<retrievable corpus="x" k="10">y</retrievable>"#).unwrap();
        assert_eq!(t[0].k, 10);
    }

    #[test]
    fn tag_count_is_capped() {
        let mut body = String::new();
        for i in 0..(MAX_RETRIEVABLE_TAGS + 1) {
            body.push_str(&format!(r#"<retrievable corpus="c{i}">p</retrievable>"#));
        }
        let t = parse(&body).unwrap();
        assert_eq!(t.len(), MAX_RETRIEVABLE_TAGS);
        assert_eq!(t[0].corpus, "c0");
        assert_eq!(
            t[MAX_RETRIEVABLE_TAGS - 1].corpus,
            format!("c{}", MAX_RETRIEVABLE_TAGS - 1)
        );
    }

    #[test]
    fn tag_count_at_cap_all_kept() {
        let mut body = String::new();
        for i in 0..MAX_RETRIEVABLE_TAGS {
            body.push_str(&format!(r#"<retrievable corpus="c{i}">p</retrievable>"#));
        }
        let t = parse(&body).unwrap();
        assert_eq!(t.len(), MAX_RETRIEVABLE_TAGS);
    }
}
