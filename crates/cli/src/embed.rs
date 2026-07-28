//! `tt embed` — embed text via the gateway and print a cost summary (or --json
//! vectors). Thin glue over `tt_client::Client::embeddings()`; the network path
//! is covered by tt-client's own tests, so the tested surface here is the two
//! pure helpers below.

use anyhow::Context as _;
use tt_client::{EmbedOutcome, EmbeddingInput, RequestDeltaEstimate};

use crate::context::ResolvedContext;
use crate::ui;

const DEFAULT_MODEL: &str = "text-embedding-3-small";

/// Build the embeddings input: 1 arg → Single, >1 → Batch; no args → the trimmed
/// stdin text as Single. Returns None when there is nothing to embed.
fn assemble_input(args: &[String], stdin_text: Option<&str>) -> Option<EmbeddingInput> {
    match args {
        [] => {
            let text = stdin_text?.trim();
            if text.is_empty() {
                None
            } else {
                Some(EmbeddingInput::Single(text.to_string()))
            }
        }
        [one] => Some(EmbeddingInput::Single(one.clone())),
        many => Some(EmbeddingInput::Batch(many.to_vec())),
    }
}

/// One-line styled summary, e.g.
/// "text-embedding-3-small · 2 embeddings × 3 dims · $0.0002 · request delta +$0.000200 (positive estimate)".
fn format_signed_usd(value: f64) -> String {
    if value < 0.0 {
        format!("−${:.6}", -value)
    } else {
        format!("+${value:.6}")
    }
}

fn format_embed_summary(out: &EmbedOutcome, requested_model: &str) -> String {
    let model = out.cost.model_used.as_deref().unwrap_or(requested_model);
    let count = out.response.data.len();
    let noun = if count == 1 {
        "embedding"
    } else {
        "embeddings"
    };

    let mut parts = vec![model.to_string(), format!("{count} {noun}")];
    if let Some(dims) = out.response.data.first().map(|d| d.embedding.len()) {
        parts.push(format!("× {dims} dims"));
    }
    if let Some(cost) = out.cost.cost_usd {
        parts.push(format!("${cost:.4}"));
    }
    // Do not render the legacy positive-only `saved_usd` header as a savings
    // percentage. The strict client estimate requires every raw component and
    // leaves partial/old responses explicitly unmeasured.
    match out.cost.request_delta_estimate() {
        RequestDeltaEstimate::Measured {
            signed_usd,
            positive_usd,
            ..
        } if positive_usd > 0.0 => {
            parts.push(format!(
                "request delta {} (positive estimate)",
                format_signed_usd(signed_usd)
            ));
        }
        RequestDeltaEstimate::Measured {
            signed_usd,
            regression_usd,
            ..
        } if regression_usd > 0.0 => {
            parts.push(format!(
                "request delta {} (regression)",
                format_signed_usd(signed_usd)
            ));
        }
        RequestDeltaEstimate::Measured { .. } => {
            parts.push("request delta $0.000000 (neutral estimate)".to_string());
        }
        RequestDeltaEstimate::Unmeasured => parts.push("request delta not measured".to_string()),
    }
    parts.push("request delta excludes judge/shadow taxes + invoice reconciliation".to_string());
    ui::muted()
        .apply_to(parts.join(&format!(" {} ", ui::BULLET)))
        .to_string()
}

/// Embed `input` (or stdin) and print a cost summary — or, with `--json`, the
/// full `EmbeddingsResponse` to stdout and the summary to stderr.
///
/// # Errors
/// Surfaces a missing API key, a `402` cost-limit rejection, or any transport /
/// gateway error from the SDK.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    input: Vec<String>,
    model: Option<String>,
    dimensions: Option<u32>,
    encoding_format: Option<String>,
    cost_limit: Option<f64>,
    json: bool,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let client = tt_client::Client::new(base, key);

    let stdin_text = if input.is_empty() {
        Some(std::io::read_to_string(std::io::stdin()).context("failed to read stdin")?)
    } else {
        None
    };
    let assembled = assemble_input(&input, stdin_text.as_deref())
        .context("no input — pass text as an argument or pipe it on stdin")?;

    let requested_model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let mut builder = client
        .embeddings()
        .model(requested_model.clone())
        .input(assembled);
    if let Some(n) = dimensions {
        builder = builder.dimensions(n);
    }
    if let Some(f) = encoding_format {
        builder = builder.encoding_format(f);
    }
    if let Some(c) = cost_limit {
        builder = builder.cost_limit(c);
    }
    let out = builder.send().await?;

    let summary = format_embed_summary(&out, &requested_model);
    if json {
        println!("{}", serde_json::to_string_pretty(&out.response)?);
        ui::note(&summary);
    } else {
        println!("{summary}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_client::{CostInfo, EmbeddingData, EmbeddingsResponse, Usage};

    fn outcome(rows: usize, dims: usize, cost: CostInfo) -> EmbedOutcome {
        let data = (0..rows)
            .map(|i| EmbeddingData {
                object: "embedding".to_string(),
                index: i as u32,
                embedding: vec![0.0_f32; dims],
            })
            .collect();
        EmbedOutcome {
            response: EmbeddingsResponse {
                object: "list".to_string(),
                data,
                model: "srv-model".to_string(),
                usage: Usage::default(),
            },
            cost,
        }
    }

    #[test]
    fn assemble_input_single_arg() {
        let got = assemble_input(&["hi".to_string()], None);
        assert!(matches!(got, Some(EmbeddingInput::Single(s)) if s == "hi"));
    }

    #[test]
    fn assemble_input_multi_arg_batch() {
        let got = assemble_input(&["a".to_string(), "b".to_string()], None);
        assert!(
            matches!(got, Some(EmbeddingInput::Batch(v)) if v == vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn assemble_input_stdin_fallback_trims() {
        let got = assemble_input(&[], Some(" hi \n"));
        assert!(matches!(got, Some(EmbeddingInput::Single(s)) if s == "hi"));
    }

    #[test]
    fn assemble_input_empty_is_none() {
        assert!(assemble_input(&[], None).is_none());
        assert!(assemble_input(&[], Some("   ")).is_none());
    }

    #[test]
    fn format_embed_summary_shows_measured_positive_request_delta() {
        let cost = CostInfo {
            cost_usd: Some(0.0002),
            saved_usd: Some(0.0003),
            baseline_cost_usd: Some(0.0004),
            provider_cache_saved_usd: Some(0.0),
            cache_bust_usd: Some(0.0),
            summarizer_tax_usd: Some(0.0),
            model_used: Some("text-embedding-3-small".to_string()),
            ..CostInfo::default()
        };
        let s = format_embed_summary(&outcome(2, 3, cost), "ignored");
        let plain = console::strip_ansi_codes(&s);
        assert!(plain.contains("text-embedding-3-small"), "{plain}");
        assert!(plain.contains("2 embeddings"), "{plain}");
        assert!(plain.contains("× 3 dims"), "{plain}");
        assert!(plain.contains("$0.0002"), "{plain}");
        assert!(
            plain.contains("request delta +$0.000200 (positive estimate)"),
            "{plain}"
        );
        assert!(!plain.contains("saved 75%"), "{plain}");
        assert!(
            plain.contains("excludes judge/shadow taxes + invoice reconciliation"),
            "{plain}"
        );
    }

    #[test]
    fn format_embed_summary_shows_measured_regression() {
        let cost = CostInfo {
            cost_usd: Some(0.0006),
            // The compatibility field is clamped, so it must not hide this
            // complete raw-tuple regression in the human-facing summary.
            saved_usd: Some(0.0),
            baseline_cost_usd: Some(0.0004),
            provider_cache_saved_usd: Some(0.0),
            cache_bust_usd: Some(0.0),
            summarizer_tax_usd: Some(0.0),
            ..CostInfo::default()
        };
        let summary = format_embed_summary(&outcome(1, 4, cost), "ignored");
        let plain = console::strip_ansi_codes(&summary);
        assert!(
            plain.contains("request delta −$0.000200 (regression)"),
            "{plain}"
        );
        assert!(!plain.contains("saved 0%"), "{plain}");
    }

    #[test]
    fn format_embed_summary_marks_partial_tuple_unmeasured() {
        let cost = CostInfo {
            cost_usd: Some(0.0002),
            saved_usd: Some(0.0003),
            baseline_cost_usd: Some(0.0004),
            // An old/partial gateway response must not get a legacy fallback.
            provider_cache_saved_usd: None,
            cache_bust_usd: Some(0.0),
            summarizer_tax_usd: Some(0.0),
            ..CostInfo::default()
        };
        let summary = format_embed_summary(&outcome(1, 4, cost), "ignored");
        let plain = console::strip_ansi_codes(&summary);
        assert!(plain.contains("request delta not measured"), "{plain}");
        assert!(!plain.contains("saved 75%"), "{plain}");
    }

    #[test]
    fn format_embed_summary_minimal_falls_back_to_requested_model() {
        let s = format_embed_summary(&outcome(1, 4, CostInfo::default()), "my-model");
        let plain = console::strip_ansi_codes(&s);
        assert!(plain.contains("my-model"), "{plain}");
        assert!(plain.contains("1 embedding"), "{plain}");
        assert!(!plain.contains("embeddings"), "singular expected: {plain}");
        assert!(!plain.contains('$'), "no cost expected: {plain}");
        assert!(plain.contains("request delta not measured"), "{plain}");
    }
}
