//! Ctrl-C banner: brief session summary printed to stderr.

use crate::proxy::session::Rollup;
use std::path::Path;

pub fn print_summary(r: &Rollup, log_path: &Path) {
    let hit_rate = if r.requests > 0 {
        (r.cache_hits as f64) / (r.requests as f64) * 100.0
    } else {
        0.0
    };
    let cache_savings = r.total_cost_usd * (hit_rate / 100.0);
    let net_potential = r.suggested_savings_usd;
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
    eprintln!("│  Cache savings:      ${cache_savings:<27.4}│");
    eprintln!("│  Suggested savings:  ${net_potential:<27.4}│");
    eprintln!("│                                                    │");
    eprintln!(
        "│  Session log: {:<37}│",
        log_path.display().to_string()
    );
    eprintln!("└────────────────────────────────────────────────────┘");
}
