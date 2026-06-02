//! Ctrl-C banner + live status line printed to stderr.

use crate::proxy::session::Rollup;
use std::io::Write;
use std::path::Path;

/// Format the single-line live status: requests, realized savings, hit rate.
fn format_live_line(r: &Rollup) -> String {
    let hit_rate = if r.requests > 0 {
        (r.cache_hits as f64) / (r.requests as f64) * 100.0
    } else {
        0.0
    };
    format!(
        "  tt · {} req · ${:.4} saved · {:.0}% cached",
        r.requests, r.total_savings_usd, hit_rate
    )
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
    eprintln!("│  Requests:           {:<28}│", r.requests);
    eprintln!("│  Total cost:         ${:<27.4}│", r.total_cost_usd);
    eprintln!(
        "│  Cached (L1+L2):     {} ({:.0}%){:<w$}│",
        r.cache_hits,
        hit_rate,
        "",
        w = 27usize.saturating_sub(format!("{} ({:.0}%)", r.cache_hits, hit_rate).len())
    );
    // Realized savings reported by the gateway (cache discount + routing
    // downgrade), summed across the session — not a heuristic.
    eprintln!("│  Saved (realized):   ${:<27.4}│", r.total_savings_usd);
    eprintln!("│  Suggested savings:  ${:<27.4}│", r.suggested_savings_usd);
    eprintln!("│                                                    │");
    eprintln!("│  Session log: {:<37}│", log_path.display().to_string());
    eprintln!("└────────────────────────────────────────────────────┘");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_line_shows_requests_savings_and_hit_rate() {
        let r = Rollup {
            requests: 4,
            total_cost_usd: 0.0,
            total_savings_usd: 1.83,
            cache_hits: 1,
            suggested_savings_usd: 0.0,
        };
        let line = format_live_line(&r);
        assert!(line.contains("4 req"), "line = {line}");
        assert!(line.contains("$1.8300 saved"), "line = {line}");
        assert!(line.contains("25% cached"), "line = {line}"); // 1/4
    }
}
