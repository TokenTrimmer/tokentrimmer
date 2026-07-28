//! Per-session cost rollup. Appends one JSONL line per response; aggregates
//! totals for the Ctrl-C banner.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::http::HeaderMap;
use chrono::Utc;
use serde::Serialize;

const BASELINE_COST_HEADER: &str = "x-tokentrimmer-baseline-cost-usd";
const COST_HEADER: &str = "x-tokentrimmer-cost-usd";
const LEGACY_SAVED_HEADER: &str = "x-tokentrimmer-saved-usd";
const PROVIDER_CACHE_SAVED_HEADER: &str = "x-tokentrimmer-provider-cache-saved-usd";
const CACHE_BUST_HEADER: &str = "x-tokentrimmer-cache-bust-usd";
const SUMMARIZER_TAX_HEADER: &str = "x-tokentrimmer-summarizer-tax-usd";

/// The proxy's stable JSONL projection of the shared request-delta estimate.
///
/// Its field names predate the first-party client type, so this intentionally
/// remains a serialization adapter. All validation and five-component math
/// comes from [`tt_client::RequestDeltaEstimate`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RequestDeltaEstimate {
    Measured {
        baseline_usd: f64,
        cost_usd: f64,
        provider_cache_saved_usd: f64,
        cache_bust_penalty_usd: f64,
        summarizer_tax_usd: f64,
        signed_usd: f64,
        positive_usd: f64,
        regression_usd: f64,
    },
    Unmeasured,
}

impl From<tt_client::RequestDeltaEstimate> for RequestDeltaEstimate {
    fn from(estimate: tt_client::RequestDeltaEstimate) -> Self {
        match estimate {
            tt_client::RequestDeltaEstimate::Measured {
                baseline_cost_usd,
                cost_usd,
                provider_cache_saved_usd,
                cache_bust_usd,
                summarizer_tax_usd,
                signed_usd,
                positive_usd,
                regression_usd,
            } => Self::Measured {
                baseline_usd: baseline_cost_usd,
                cost_usd,
                provider_cache_saved_usd,
                cache_bust_penalty_usd: cache_bust_usd,
                summarizer_tax_usd,
                signed_usd,
                positive_usd,
                regression_usd,
            },
            tt_client::RequestDeltaEstimate::Unmeasured => Self::Unmeasured,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GatewayAccounting {
    pub cost_usd: Option<f64>,
    /// Compatibility-only positive header retained for existing JSONL readers.
    /// It is not the signed request-delta metric or invoice reconciliation.
    pub realized_savings_usd: Option<f64>,
    pub request_delta_estimate: RequestDeltaEstimate,
}

/// Parse only finite, non-negative gateway money headers. A missing or invalid
/// request-delta component is deliberately unmeasured rather than zero-filled.
pub(crate) fn gateway_accounting_from_headers(headers: &HeaderMap) -> GatewayAccounting {
    let cost_usd = nonnegative_finite_header(headers, COST_HEADER);
    let realized_savings_usd = nonnegative_finite_header(headers, LEGACY_SAVED_HEADER);
    let request_delta_estimate = tt_client::RequestDeltaEstimate::from_components(
        nonnegative_finite_header(headers, BASELINE_COST_HEADER),
        cost_usd,
        nonnegative_finite_header(headers, PROVIDER_CACHE_SAVED_HEADER),
        nonnegative_finite_header(headers, CACHE_BUST_HEADER),
        nonnegative_finite_header(headers, SUMMARIZER_TAX_HEADER),
    )
    .into();

    GatewayAccounting {
        cost_usd,
        realized_savings_usd,
        request_delta_estimate,
    }
}

fn nonnegative_finite_header(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        // Normalize negative zero so JSONL and rollups never render `-0.0`.
        .map(|value| if value == 0.0 { 0.0 } else { value })
}

#[derive(Debug, Default, Clone)]
pub struct Rollup {
    pub requests: u32,
    pub total_cost_usd: f64,
    /// Compatibility rollup of the legacy positive-only `realized_savings_usd`
    /// field. Do not use this as the signed request-delta total.
    pub total_savings_usd: f64,
    pub cache_hits: u32,
    pub suggested_savings_usd: f64,
    pub measured_request_deltas: u32,
    pub unmeasured_request_deltas: u32,
    pub total_signed_request_delta_usd: f64,
    pub total_positive_request_delta_usd: f64,
    pub total_regression_request_delta_usd: f64,
}

#[derive(Debug, Serialize)]
pub struct LogLine<'a> {
    pub timestamp: String,
    pub mode: &'a str,
    pub route: &'a str,
    pub model: Option<&'a str>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cost_usd: Option<f64>,
    pub preview_cost_usd: Option<f64>,
    pub cache_layer: Option<&'a str>,
    pub suggested_route: Option<&'a str>,
    pub suggested_savings_usd: Option<f64>,
    /// Compatibility-only positive legacy value from
    /// `x-tokentrimmer-saved-usd`. It must not be displayed as a signed delta.
    pub realized_savings_usd: Option<f64>,
    /// The complete signed request-delta estimate, or an explicit unmeasured
    /// state when any component was absent or invalid.
    pub request_delta_estimate: RequestDeltaEstimate,
    pub trace_id: Option<&'a str>,
}

pub struct SessionLog {
    path: PathBuf,
    rollup: Mutex<Rollup>,
}

impl SessionLog {
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let date = Utc::now().format("%Y-%m-%d").to_string();
        Ok(Self {
            path: dir.join(format!("{date}.jsonl")),
            rollup: Mutex::new(Rollup::default()),
        })
    }

    pub fn append(&self, line: &LogLine<'_>) -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let json = serde_json::to_string(line).unwrap();
        writeln!(f, "{json}")?;
        let mut r = self.rollup.lock().unwrap();
        r.requests += 1;
        r.total_cost_usd += line.cost_usd.unwrap_or(0.0);
        if matches!(line.cache_layer, Some("hit-l1" | "hit-l2")) {
            r.cache_hits += 1;
        }
        r.suggested_savings_usd += line.suggested_savings_usd.unwrap_or(0.0);
        r.total_savings_usd += line.realized_savings_usd.unwrap_or(0.0);
        match line.request_delta_estimate {
            RequestDeltaEstimate::Measured {
                signed_usd,
                positive_usd,
                regression_usd,
                ..
            } => {
                r.measured_request_deltas += 1;
                r.total_signed_request_delta_usd += signed_usd;
                r.total_positive_request_delta_usd += positive_usd;
                r.total_regression_request_delta_usd += regression_usd;
            }
            RequestDeltaEstimate::Unmeasured => r.unmeasured_request_deltas += 1,
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Rollup {
        self.rollup.lock().unwrap().clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(BASELINE_COST_HEADER, "0.0010".parse().unwrap());
        headers.insert(COST_HEADER, "0.0006".parse().unwrap());
        headers.insert(PROVIDER_CACHE_SAVED_HEADER, "0.0001".parse().unwrap());
        headers.insert(CACHE_BUST_HEADER, "0.0001".parse().unwrap());
        headers.insert(SUMMARIZER_TAX_HEADER, "0.0001".parse().unwrap());
        headers.insert(LEGACY_SAVED_HEADER, "0.0001".parse().unwrap());
        headers
    }

    fn log_line(request_delta_estimate: RequestDeltaEstimate) -> LogLine<'static> {
        LogLine {
            timestamp: "ts".into(),
            mode: "gateway",
            route: "POST /v1/messages",
            model: Some("claude-haiku-4-5"),
            input_tokens: Some(10),
            output_tokens: Some(5),
            cost_usd: Some(0.0001),
            preview_cost_usd: Some(0.0001),
            cache_layer: Some("hit-l1"),
            suggested_route: None,
            suggested_savings_usd: None,
            realized_savings_usd: Some(0.0003),
            request_delta_estimate,
            trace_id: Some("t"),
        }
    }

    #[test]
    fn gateway_accounting_requires_all_finite_nonnegative_delta_components() {
        let accounting = gateway_accounting_from_headers(&complete_headers());
        assert_eq!(accounting.cost_usd, Some(0.0006));
        assert_eq!(accounting.realized_savings_usd, Some(0.0001));
        assert_eq!(
            accounting.request_delta_estimate,
            RequestDeltaEstimate::from(tt_client::RequestDeltaEstimate::from_components(
                Some(0.0010),
                Some(0.0006),
                Some(0.0001),
                Some(0.0001),
                Some(0.0001),
            )),
            "proxy header accounting must retain the client estimate semantics"
        );
        match accounting.request_delta_estimate {
            RequestDeltaEstimate::Measured {
                signed_usd,
                positive_usd,
                regression_usd,
                ..
            } => {
                assert!((signed_usd - 0.0001).abs() < 1e-12);
                assert!((positive_usd - 0.0001).abs() < 1e-12);
                assert_eq!(regression_usd, 0.0);
            }
            RequestDeltaEstimate::Unmeasured => panic!("complete component set must be measured"),
        }

        let mut missing = complete_headers();
        missing.remove(SUMMARIZER_TAX_HEADER);
        assert!(matches!(
            gateway_accounting_from_headers(&missing).request_delta_estimate,
            RequestDeltaEstimate::Unmeasured
        ));

        for (header, value) in [
            (BASELINE_COST_HEADER, "not-a-number"),
            (COST_HEADER, "NaN"),
            (PROVIDER_CACHE_SAVED_HEADER, "infinity"),
            (CACHE_BUST_HEADER, "-0.0001"),
            (SUMMARIZER_TAX_HEADER, "-0.0001"),
        ] {
            let mut invalid = complete_headers();
            invalid.insert(header, value.parse().unwrap());
            assert!(matches!(
                gateway_accounting_from_headers(&invalid).request_delta_estimate,
                RequestDeltaEstimate::Unmeasured
            ));
        }

        let mut invalid_legacy = complete_headers();
        invalid_legacy.insert(LEGACY_SAVED_HEADER, "-0.0001".parse().unwrap());
        assert_eq!(
            gateway_accounting_from_headers(&invalid_legacy).realized_savings_usd,
            None
        );
    }

    #[test]
    fn proxy_projection_preserves_client_delta_and_unmeasured_state() {
        let client_estimate = tt_client::RequestDeltaEstimate::from_components(
            Some(0.001),
            Some(0.0008),
            Some(0.0001),
            Some(0.0001),
            Some(0.0002),
        );
        let tt_client::RequestDeltaEstimate::Measured {
            baseline_cost_usd,
            cost_usd,
            provider_cache_saved_usd,
            cache_bust_usd,
            summarizer_tax_usd,
            signed_usd,
            positive_usd,
            regression_usd,
        } = client_estimate
        else {
            panic!("complete components must produce a client estimate");
        };
        assert_eq!(
            RequestDeltaEstimate::from(client_estimate),
            RequestDeltaEstimate::Measured {
                baseline_usd: baseline_cost_usd,
                cost_usd,
                provider_cache_saved_usd,
                cache_bust_penalty_usd: cache_bust_usd,
                summarizer_tax_usd,
                signed_usd,
                positive_usd,
                regression_usd,
            }
        );
        assert_eq!(
            RequestDeltaEstimate::from(tt_client::RequestDeltaEstimate::Unmeasured),
            RequestDeltaEstimate::Unmeasured
        );
    }

    #[test]
    fn jsonl_serialization_keeps_proxy_field_names_for_client_estimates() {
        let measured = RequestDeltaEstimate::from(tt_client::RequestDeltaEstimate::Measured {
            baseline_cost_usd: 0.5,
            cost_usd: 0.75,
            provider_cache_saved_usd: 0.0,
            cache_bust_usd: 0.0,
            summarizer_tax_usd: 0.0,
            signed_usd: -0.25,
            positive_usd: 0.0,
            regression_usd: 0.25,
        });
        let measured_json = serde_json::to_value(log_line(measured)).unwrap();
        let expected_measured = serde_json::json!({
            "state": "measured",
            "baseline_usd": 0.5,
            "cost_usd": 0.75,
            "provider_cache_saved_usd": 0.0,
            "cache_bust_penalty_usd": 0.0,
            "summarizer_tax_usd": 0.0,
            "signed_usd": -0.25,
            "positive_usd": 0.0,
            "regression_usd": 0.25,
        });
        assert_eq!(serde_json::to_value(measured).unwrap(), expected_measured);
        assert_eq!(measured_json["request_delta_estimate"], expected_measured);

        let unmeasured = RequestDeltaEstimate::Unmeasured;
        let unmeasured_json = serde_json::to_value(log_line(unmeasured)).unwrap();
        let expected_unmeasured = serde_json::json!({ "state": "unmeasured" });
        assert_eq!(
            serde_json::to_value(unmeasured).unwrap(),
            expected_unmeasured
        );
        assert_eq!(
            unmeasured_json["request_delta_estimate"],
            expected_unmeasured
        );
    }

    #[test]
    fn append_writes_measured_and_unmeasured_jsonl_and_updates_rollup() {
        let d = tempfile::tempdir().unwrap();
        let log = SessionLog::new(d.path()).unwrap();
        log.append(&log_line(RequestDeltaEstimate::from(
            tt_client::RequestDeltaEstimate::from_components(
                Some(0.001),
                Some(0.0008),
                Some(0.0001),
                Some(0.0001),
                Some(0.0002),
            ),
        )))
        .unwrap();
        log.append(&log_line(RequestDeltaEstimate::Unmeasured))
            .unwrap();
        let r = log.snapshot();
        assert_eq!(r.requests, 2);
        assert_eq!(r.cache_hits, 2);
        assert!((r.total_savings_usd - 0.0006).abs() < 1e-9);
        assert_eq!(r.measured_request_deltas, 1);
        assert_eq!(r.unmeasured_request_deltas, 1);
        assert!((r.total_signed_request_delta_usd + 0.0002).abs() < 1e-12);
        assert_eq!(r.total_positive_request_delta_usd, 0.0);
        assert!((r.total_regression_request_delta_usd - 0.0002).abs() < 1e-12);
        let body = std::fs::read_to_string(log.path()).unwrap();
        assert!(body.contains("claude-haiku-4-5"));
        assert!(body.contains("\"state\":\"measured\""));
        assert!(body.contains("\"state\":\"unmeasured\""));
    }
}
