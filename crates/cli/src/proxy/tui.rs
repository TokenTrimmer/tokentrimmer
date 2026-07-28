//! Ctrl-C banner + live status line printed to stderr.

use crate::proxy::session::Rollup;
use std::io::Write;
use std::path::Path;

/// Format the single-line live status: requests, signed request delta, hit rate.
fn format_live_line(r: &Rollup) -> String {
    let hit_rate = if r.requests > 0 {
        (r.cache_hits as f64) / (r.requests as f64) * 100.0
    } else {
        0.0
    };
    let request_delta = if r.measured_request_deltas == 0 {
        "request delta unmeasured".to_owned()
    } else {
        let mut label = if r.total_signed_request_delta_usd < 0.0 {
            format!("est. regression ${:.4}", -r.total_signed_request_delta_usd)
        } else {
            format!(
                "est. request delta ${:.4}",
                r.total_signed_request_delta_usd
            )
        };
        if r.unmeasured_request_deltas > 0 {
            label.push_str(&format!(" · {} unmeasured", r.unmeasured_request_deltas));
        }
        label
    };
    format!(
        "  tt · {} req · {} · {:.0}% cached",
        r.requests, request_delta, hit_rate
    )
}

fn format_signed_usd(value: f64) -> String {
    if value < 0.0 {
        format!("-${:.4}", -value)
    } else {
        format!("${value:.4}")
    }
}

fn format_usd(value: f64) -> String {
    format!("${value:.4}")
}

fn print_summary_row(label: &str, value: impl std::fmt::Display) {
    let content = format!("  {label:<22}{value}");
    eprintln!("│{content:<52}│");
}

fn request_delta_coverage_label(r: &Rollup) -> String {
    if r.measured_request_deltas == 0 {
        format!("not measured (0 / {} req)", r.requests)
    } else if r.unmeasured_request_deltas > 0 {
        format!(
            "partial: {} measured / {} req",
            r.measured_request_deltas, r.requests
        )
    } else {
        format!("complete: {} measured", r.measured_request_deltas)
    }
}

/// Print/refresh the live status line on stderr, rewriting in place with `\r`
/// so it doesn't scroll. Called after each proxied request (unless `--no-tui`).
pub fn print_live_line(r: &Rollup) {
    // Trailing spaces clear any leftover chars from a longer previous line.
    eprint!("\r{}    ", format_live_line(r));
    let _ = std::io::stderr().flush();
}

pub fn print_summary(r: &Rollup, log_path: &Path) {
    let hit_rate = if r.requests > 0 {
        (r.cache_hits as f64) / (r.requests as f64) * 100.0
    } else {
        0.0
    };
    eprintln!();
    eprintln!("┌─ tokentrimmer session summary ─────────────────────┐");
    print_summary_row("Requests:", r.requests);
    print_summary_row("Total cost:", format_usd(r.total_cost_usd));
    print_summary_row(
        "Cached (L1+L2):",
        format!("{} ({hit_rate:.0}%)", r.cache_hits),
    );
    if r.measured_request_deltas == 0 {
        print_summary_row("Signed request delta:", "not measured");
        print_summary_row("Positive estimate:", "not measured");
        print_summary_row("Regression magnitude:", "not measured");
    } else if r.unmeasured_request_deltas > 0 {
        print_summary_row(
            "Signed delta (subset):",
            format_signed_usd(r.total_signed_request_delta_usd),
        );
        print_summary_row(
            "Positive (subset):",
            format_usd(r.total_positive_request_delta_usd),
        );
        print_summary_row(
            "Regression (subset):",
            format_usd(r.total_regression_request_delta_usd),
        );
    } else {
        print_summary_row(
            "Signed request delta:",
            format_signed_usd(r.total_signed_request_delta_usd),
        );
        print_summary_row(
            "Positive estimate:",
            format_usd(r.total_positive_request_delta_usd),
        );
        print_summary_row(
            "Regression magnitude:",
            format_usd(r.total_regression_request_delta_usd),
        );
    }
    print_summary_row("Delta coverage:", request_delta_coverage_label(r));
    print_summary_row("Delta unmeasured:", r.unmeasured_request_deltas);
    print_summary_row("Legacy saved (compat):", format_usd(r.total_savings_usd));
    print_summary_row("Suggested savings:", format_usd(r.suggested_savings_usd));
    eprintln!("│                                                    │");
    print_summary_row("Session log:", log_path.display());
    eprintln!("└────────────────────────────────────────────────────┘");
    eprintln!(
        "  Gateway request delta = baseline − cost − provider-cache − cache-bust − summarizer tax."
    );
    eprintln!("  It excludes judge/shadow measurement taxes and provider-invoice reconciliation.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_line_shows_signed_request_delta_and_hit_rate() {
        let r = Rollup {
            requests: 4,
            total_cost_usd: 0.0,
            cache_hits: 1,
            measured_request_deltas: 4,
            total_signed_request_delta_usd: 1.83,
            ..Default::default()
        };
        let line = format_live_line(&r);
        assert!(line.contains("4 req"), "line = {line}");
        assert!(line.contains("est. request delta $1.8300"), "line = {line}");
        assert!(line.contains("25% cached"), "line = {line}"); // 1/4
    }

    #[test]
    fn live_line_marks_regressions_and_partial_measurement() {
        let r = Rollup {
            requests: 3,
            measured_request_deltas: 1,
            unmeasured_request_deltas: 2,
            total_signed_request_delta_usd: -0.0042,
            total_regression_request_delta_usd: 0.0042,
            ..Default::default()
        };
        let line = format_live_line(&r);
        assert!(line.contains("est. regression $0.0042"), "line = {line}");
        assert!(line.contains("2 unmeasured"), "line = {line}");
    }
}
