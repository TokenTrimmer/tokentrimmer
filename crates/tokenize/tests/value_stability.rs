//! Token-count value-stability pin across `tiktoken-rs` upgrades.
//!
//! Token counts are money inputs (routing cost ceilings, the request-pass
//! token-true gate, savings booking), so a dependency bump of `tiktoken-rs`
//! is acceptable ONLY if the same encoding + same input yields the same
//! count. The expected literals below were captured under tiktoken-rs
//! **0.5.9** via the ignored `capture_counts` harness; any future bump must
//! keep every one of them byte-identical or be rejected.
//!
//! Coverage: `cl100k_base` AND `o200k_base`, exercised through the public
//! `tt-tokenize` API (the exact code paths that price requests), over a
//! diverse corpus: ASCII prose, Unicode/CJK/emoji/RTL, JSON blobs, code,
//! long repetition, empty and whitespace-only inputs, and special-token
//! lookalike text.

use tt_tokenize::{estimate_input_tokens, estimate_input_tokens_for_model, Confidence};

/// Fixed corpus. Order matters: `EXPECTED_CL100K[i]` / `EXPECTED_O200K[i]`
/// pin the counts for `corpus()[i]`.
fn corpus() -> Vec<String> {
    vec![
        // 0: empty
        String::new(),
        // 1: single space
        " ".to_string(),
        // 2: whitespace-only mix (tab, LF, CRLF, NBSP)
        " \t\n \r\n \u{00A0}  ".to_string(),
        // 3: pangram
        "The quick brown fox jumps over the lazy dog.".to_string(),
        // 4: ASCII prose paragraph
        "TokenTrimmer routes each request to the cheapest capable model. \
         Token counts are money inputs: a miscount of one token per request \
         compounds into real dollars at scale."
            .to_string(),
        // 5: short greeting
        "Hello, world.".to_string(),
        // 6: accented Latin
        "Café naïve façade — déjà vu, Søren Kierkegaard, smörgåsbord".to_string(),
        // 7: CJK (Japanese / Chinese / Korean)
        "東京タワーから富士山が見える。中文的分词器很有趣。한국어 토큰 수 계산 테스트입니다。".to_string(),
        // 8: emoji incl. ZWJ family, flags, skin-tone modifier
        "Hello 👋 World 🌍! 🚀🔥💯 family: 👨‍👩‍👧‍👦 flags: 🇺🇸🇯🇵 thumbs: 👍🏽".to_string(),
        // 9: Greek + Cyrillic
        "Καλημέρα κόσμε. Привет, мир! Чёрная кошка.".to_string(),
        // 10: RTL Arabic + Hebrew
        "مرحبا بالعالم، هذا اختبار. שלום עולם, זה מבחן.".to_string(),
        // 11: compact JSON blob
        r#"{"model":"gpt-4o","max_tokens":1024,"stream":true}"#.to_string(),
        // 12: nested JSON with unicode escapes
        r#"{"messages":[{"role":"system","content":"You are terse."},{"role":"user","content":"Summarize: é中😀"}],"temperature":0.7,"metadata":{"trace_id":"abc-123","nested":{"a":[1,2,3],"b":null}}}"#
            .to_string(),
        // 13: Rust code
        "fn main() {\n    let xs: Vec<u32> = (0..10).map(|x| x * x).collect();\n    println!(\"{:?}\", xs);\n}\n"
            .to_string(),
        // 14: Python code
        "def fib(n: int) -> int:\n    a, b = 0, 1\n    for _ in range(n):\n        a, b = b, a + b\n    return a\n"
            .to_string(),
        // 15: SQL
        "SELECT user_id, COUNT(*) AS n FROM requests WHERE created_at >= NOW() - INTERVAL '7 days' GROUP BY 1 ORDER BY n DESC LIMIT 10;"
            .to_string(),
        // 16: long repetition of a short unit
        "abc ".repeat(200),
        // 17: long whitespace run between letters
        format!("x{}y", " ".repeat(400)),
        // 18: newline run
        "\n".repeat(100),
        // 19: repeated word
        "buffalo ".repeat(64),
        // 20: digits and number-like text
        "3.14159265358979323846 1234567890 0xDEADBEEF 1e-9 -42 1_000_000".to_string(),
        // 21: URL + email
        "https://example.com/path/to/resource?query=token%20count&limit=100#frag user.name+tag@example.co.uk"
            .to_string(),
        // 22: Markdown document
        "# Heading\n\n- bullet one\n- bullet two\n\n```rust\nlet x = 1;\n```\n\n**bold** _italic_ [link](https://example.com)\n"
            .to_string(),
        // 23: special-token lookalikes (encode_with_special_tokens path)
        "<|endoftext|> plain text after <|fim_prefix|> and <|im_start|>chat".to_string(),
    ]
}

/// Exact `cl100k_base` counts captured under tiktoken-rs 0.5.9.
const EXPECTED_CL100K: [u32; 24] = [
    0, 1, 4, 10, 33, 4, 25, 48, 56, 31, 40, 19, 59, 36, 42, 34, 201, 6, 4, 66, 33, 27, 36, 13,
];

/// Exact `o200k_base` counts captured under tiktoken-rs 0.5.9.
const EXPECTED_O200K: [u32; 24] = [
    0, 1, 4, 10, 33, 4, 20, 30, 40, 18, 15, 19, 60, 36, 42, 35, 201, 6, 7, 66, 33, 27, 36, 18,
];

/// cl100k count through the public API (provider-keyed OpenAI path).
fn cl100k_count(text: &str) -> u32 {
    let e = estimate_input_tokens("openai", text);
    assert_eq!(
        e.confidence,
        Confidence::High,
        "cl100k BPE must be loaded (no heuristic fallback) for a valid pin"
    );
    e.tokens
}

/// o200k count through the public API (model-aware gpt-4o path).
fn o200k_count(text: &str) -> u32 {
    let e = estimate_input_tokens_for_model("openai", "gpt-4o", text);
    assert_eq!(
        e.confidence,
        Confidence::High,
        "o200k BPE must be loaded (no heuristic fallback) for a valid pin"
    );
    e.tokens
}

#[test]
fn cl100k_counts_are_value_stable() {
    for (i, text) in corpus().iter().enumerate() {
        let got = cl100k_count(text);
        assert_eq!(
            got, EXPECTED_CL100K[i],
            "cl100k count drifted for corpus[{i}] (expected {}, got {got}): {text:?}",
            EXPECTED_CL100K[i]
        );
        // Every cl100k-keyed public path must agree: legacy OpenAI model ids
        // and the documented Anthropic proxy use the same encoding.
        assert_eq!(
            estimate_input_tokens_for_model("openai", "gpt-4", text).tokens,
            got,
            "model-aware legacy-OpenAI path disagreed with cl100k for corpus[{i}]"
        );
        assert_eq!(
            estimate_input_tokens("anthropic", text).tokens,
            got,
            "Anthropic cl100k-proxy path disagreed with cl100k for corpus[{i}]"
        );
    }
}

#[test]
fn o200k_counts_are_value_stable() {
    for (i, text) in corpus().iter().enumerate() {
        let got = o200k_count(text);
        assert_eq!(
            got, EXPECTED_O200K[i],
            "o200k count drifted for corpus[{i}] (expected {}, got {got}): {text:?}",
            EXPECTED_O200K[i]
        );
        // The generic-BPE proxy (non-OpenAI/Anthropic providers) uses o200k
        // and must agree exactly.
        assert_eq!(
            estimate_input_tokens_for_model("groq", "llama-3.3-70b", text).tokens,
            got,
            "generic-BPE-proxy path disagreed with o200k for corpus[{i}]"
        );
    }
}

/// One-time capture harness: prints the expected arrays as Rust literals.
/// Run on a known-good tree to (re)generate the pinned counts:
/// `cargo test -p tt-tokenize --test value_stability -- --ignored --nocapture`
#[test]
#[ignore = "capture harness, not a regression test"]
fn capture_counts() {
    let corpus = corpus();
    let cl: Vec<String> = corpus.iter().map(|t| cl100k_count(t).to_string()).collect();
    let o2: Vec<String> = corpus.iter().map(|t| o200k_count(t).to_string()).collect();
    println!("const EXPECTED_CL100K: [u32; {}] = [", corpus.len());
    println!("    {},", cl.join(", "));
    println!("];");
    println!("const EXPECTED_O200K: [u32; {}] = [", corpus.len());
    println!("    {},", o2.join(", "));
    println!("];");
}
