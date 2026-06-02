//! Per-session cost rollup. Appends one JSONL line per response; aggregates
//! totals for the Ctrl-C banner.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use serde::Serialize;

#[derive(Debug, Default, Clone)]
pub struct Rollup {
    pub requests: u32,
    pub total_cost_usd: f64,
    pub total_savings_usd: f64,
    pub cache_hits: u32,
    pub suggested_savings_usd: f64,
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
    /// Realized savings the gateway reported on `x-tokentrimmer-saved-usd`
    /// (cache discount + routing downgrade). Summed into the session rollup.
    pub realized_savings_usd: Option<f64>,
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

    #[test]
    fn append_writes_jsonl_line_and_updates_rollup() {
        let d = tempfile::tempdir().unwrap();
        let log = SessionLog::new(d.path()).unwrap();
        log.append(&LogLine {
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
            trace_id: Some("t"),
        })
        .unwrap();
        let r = log.snapshot();
        assert_eq!(r.requests, 1);
        assert_eq!(r.cache_hits, 1);
        assert!((r.total_savings_usd - 0.0003).abs() < 1e-9);
        let body = std::fs::read_to_string(log.path()).unwrap();
        assert!(body.contains("claude-haiku-4-5"));
    }
}
