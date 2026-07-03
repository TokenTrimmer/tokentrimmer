//! The live content classifier for the `content_compress` dispatcher.
//!
//! Re-exports the shared, allocation-light classifier ([`tt_shared::content_kind`])
//! so the spec-named `content_compress::classify` path holds. The canonical
//! implementation lives in `tt-shared` so `tt-routing`'s `content_type` condition
//! can reuse the SAME heuristics (via
//! [`tt_shared::capability_check::request_dominant_content_kind`]) without a
//! `tt-core` → `tt-routing` dependency cycle.

pub use tt_shared::content_kind::{classify, ContentKind, MIN_BLOB_CHARS};

#[cfg(test)]
mod tests {
    use super::*;

    // The plan's per-kind acceptance set (mirrors the canonical tt-shared suite)
    // so `cargo test -p tt-core content_compress::classify` exercises the live
    // re-export path the dispatcher consumes.

    #[test]
    fn json_block() {
        assert_eq!(
            classify(&format!("{{{}}}", "\"k\":1,".repeat(20))),
            Some(ContentKind::Json)
        );
    }

    #[test]
    fn log_block() {
        let l = "2026-07-03 10:00:00 INFO x\n".repeat(12);
        assert_eq!(classify(&l), Some(ContentKind::Log));
    }

    #[test]
    fn diff_block() {
        let d = "@@ -1 +1 @@\n-old line here\n+new line here\n".repeat(6);
        assert_eq!(classify(&d), Some(ContentKind::Diff));
    }

    #[test]
    fn code_block() {
        let c = "fn a() {\n  let x = 1;\n}\n".repeat(20);
        assert_eq!(classify(&c), Some(ContentKind::Code));
    }

    #[test]
    fn prose_block() {
        let p = "The quick brown fox jumps over the lazy dog. ".repeat(40);
        assert_eq!(classify(&p), Some(ContentKind::Prose));
    }

    #[test]
    fn tiny_is_none() {
        assert_eq!(classify("hi"), None);
    }
}
