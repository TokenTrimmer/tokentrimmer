//! 512-token chunks with 64-token overlap. Tokenizer: tiktoken cl100k_base.

use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

const CHUNK_SIZE: usize = 512;
const OVERLAP: usize = 64;

/// Process-wide cached cl100k BPE. `None` if it failed to load (then `chunk`
/// falls back to a single whole-text chunk). Mirrors `tt-tokenize`.
fn cl100k() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}

pub struct Chunk {
    pub text: String,
    pub start_token: usize,
    pub end_token: usize,
}

pub fn chunk(text: &str) -> Vec<Chunk> {
    let Some(bpe) = cl100k() else {
        return vec![Chunk {
            text: text.into(),
            start_token: 0,
            end_token: 0,
        }];
    };
    let tokens = bpe.encode_with_special_tokens(text);
    if tokens.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < tokens.len() {
        let end = (start + CHUNK_SIZE).min(tokens.len());
        let slice = &tokens[start..end];
        let chunk_text = bpe.decode(slice.to_vec()).unwrap_or_default();
        out.push(Chunk {
            text: chunk_text,
            start_token: start,
            end_token: end,
        });
        if end == tokens.len() {
            break;
        }
        start = end - OVERLAP;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_one_chunk() {
        let cs = chunk("Hello world.");
        assert_eq!(cs.len(), 1);
        assert!(cs[0].text.contains("Hello"));
    }

    #[test]
    fn long_text_multiple_chunks_with_overlap() {
        let body = "x ".repeat(600); // > CHUNK_SIZE in tokens
        let cs = chunk(&body);
        assert!(cs.len() >= 2);
        // Overlap: second chunk's start_token = first.end_token - OVERLAP
        assert_eq!(cs[1].start_token, cs[0].end_token - OVERLAP);
    }
}
