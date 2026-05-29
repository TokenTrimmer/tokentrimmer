//! Parse `<retrievable corpus="X" k="N">...</retrievable>` tags from message
//! text. Returns each tag's corpus, k, and span in the text.

use regex::Regex;

use crate::error::RetrievalError;
use crate::types::RetrievableTag;

pub fn parse(text: &str) -> Result<Vec<RetrievableTag>, RetrievalError> {
    // Non-greedy match of the open tag + payload + close tag.
    let re =
        Regex::new(r#"(?ms)<retrievable\s+corpus="([^"]+)"(?:\s+k="(\d+)")?>(.*?)</retrievable>"#)
            .map_err(|e| RetrievalError::Tag(e.to_string()))?;
    let mut out = Vec::new();
    for m in re.captures_iter(text) {
        let full = m.get(0).unwrap();
        let corpus = m.get(1).unwrap().as_str().to_string();
        let k = m
            .get(2)
            .and_then(|x| x.as_str().parse::<u32>().ok())
            .unwrap_or(5);
        out.push(RetrievableTag {
            corpus,
            k,
            span: (full.start(), full.end()),
        });
    }
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
    }

    #[test]
    fn default_k_when_missing() {
        let t = parse(r#"<retrievable corpus="x">y</retrievable>"#).unwrap();
        assert_eq!(t[0].k, 5);
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
}
