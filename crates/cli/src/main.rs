//! `tt` — TokenTrimmer CLI.

use std::path::Path;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tt")]
#[command(about = "TokenTrimmer CLI — gateway, inspect, plan, audit", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the Gateway proxy server.
    Gateway,
    /// Scan a codebase for token-waste patterns.
    Inspect {
        /// Path to scan.
        path: String,
        /// Fail the process if any finding meets or exceeds this severity.
        #[arg(long, default_value = "high")]
        fail_on: String,
        /// Output destination. Omitted or "-" writes markdown to stdout.
        /// A path ending in ".json" writes JSON; any other path writes markdown.
        #[arg(long)]
        output: Option<String>,
    },
    /// Replay historical telemetry against a proposed config and project
    /// cost/savings/cache-hit-rate impact with bootstrap confidence intervals.
    ///
    /// v1 reads a serialized [`tt_plan_core::PlanInput`] from a JSON file —
    /// in production the request log + proposed config come from Postgres,
    /// but for offline analysis and CI gates the JSON file path is the
    /// universal interface.
    Plan {
        /// Path to a JSON file containing a serialized PlanInput.
        ///
        /// Use `--example` to dump a minimal example to stdout for editing.
        #[arg(long, conflicts_with = "example")]
        input: Option<String>,

        /// Output destination. Omitted or "-" writes a text summary to stdout.
        /// A path ending in ".json" writes the full PlanResult as JSON.
        #[arg(long)]
        output: Option<String>,

        /// Print an example PlanInput skeleton to stdout (no replay performed).
        #[arg(long)]
        example: bool,

        /// Apply the plan via the hosted backend (requires a tt_live_* key).
        /// Not yet wired — currently prints a notice and exits 0.
        #[arg(long, conflicts_with = "example")]
        apply: bool,
    },
    /// Audit log helpers.
    Audit {
        #[command(subcommand)]
        action: AuditAction,
    },
}

#[derive(Subcommand)]
enum AuditAction {
    /// Verify the integrity of the local audit log hash chain.
    ///
    /// Reads entries from `.claude/AUDIT-CHAIN.jsonl` (one JSON object per
    /// line). This path is a placeholder until the Postgres audit writer ships
    /// in Week 7.  Pass `--key <hex-file>` with the hex-encoded Ed25519
    /// verifying key that was used to sign the chain.
    Verify {
        /// Filter to a specific org UUID (recorded but not yet enforced — all
        /// entries in the file are verified regardless).
        #[arg(long)]
        org: Option<String>,
        /// Path to a file containing the hex-encoded Ed25519 verifying key.
        #[arg(long)]
        key: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Gateway => {
            println!("tt gateway: not yet implemented (Week 2-5)");
        }
        Command::Inspect {
            path,
            fail_on,
            output,
        } => {
            run_inspect(&path, &fail_on, output.as_deref())?;
        }
        Command::Plan {
            input,
            output,
            example,
            apply,
        } => {
            run_plan(input.as_deref(), output.as_deref(), example, apply)?;
        }
        Command::Audit {
            action: AuditAction::Verify { org, key },
        } => {
            run_audit_verify(org.as_deref(), key.as_deref())?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Output format detection
// ---------------------------------------------------------------------------

/// Whether to emit a markdown report or a JSON array.
enum OutputFormat {
    Markdown,
    Json,
}

/// Infer the desired output format from the destination path.
fn output_format_for(output: Option<&str>) -> OutputFormat {
    match output {
        Some(p) if p.ends_with(".json") => OutputFormat::Json,
        _ => OutputFormat::Markdown,
    }
}

// ---------------------------------------------------------------------------
// `tt inspect` implementation
// ---------------------------------------------------------------------------

/// Run the inspect engine against `path`, format the results, and either write
/// them to `output` or print to stdout.  Exits non-zero via [`anyhow::bail!`]
/// when any finding meets or exceeds `fail_on`.
fn run_inspect(path: &str, fail_on: &str, output: Option<&str>) -> anyhow::Result<()> {
    use tt_inspect_core::Severity;

    let fail_on_sev = Severity::from_str_ci(fail_on).unwrap_or(Severity::High);

    let mut engine = tt_inspect_core::Engine::new();
    // Register all 10 P0 production rules.
    for rule in tt_inspect_rules_tier1::all_rules() {
        engine.add_rule(rule);
    }

    let findings = engine.scan(std::path::Path::new(path));

    let formatted = match output_format_for(output) {
        OutputFormat::Json => tt_inspect_core::output::format_json(&findings),
        OutputFormat::Markdown => tt_inspect_core::output::format_markdown(&findings),
    };

    match output {
        Some(p) if !p.is_empty() && p != "-" => {
            std::fs::write(p, &formatted)
                .map_err(|e| anyhow::anyhow!("failed to write output to {p}: {e}"))?;
            eprintln!("wrote {} finding(s) to {p}", findings.len());
        }
        _ => {
            print!("{formatted}");
        }
    }

    let above: Vec<_> = findings
        .iter()
        .filter(|f| f.severity.weight() >= fail_on_sev.weight())
        .collect();

    if !above.is_empty() {
        anyhow::bail!(
            "{} finding(s) at or above {:?} severity \
             (use --fail-on critical to disable gating)",
            above.len(),
            fail_on_sev,
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// `tt audit verify` implementation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// `tt plan` — replay historical telemetry against a proposed config
// ---------------------------------------------------------------------------

/// Implement `tt plan`.
///
/// v1 reads a serialized [`tt_plan_core::PlanInput`] from a JSON file at
/// `--input`. Production wiring (read from Postgres given a window + diff
/// spec) lands when the hosted Plan endpoint ships; the JSON-file interface
/// stays as the universal offline path for CI gates and developer experiments.
fn run_plan(
    input: Option<&str>,
    output: Option<&str>,
    example: bool,
    apply: bool,
) -> anyhow::Result<()> {
    if example {
        print_plan_example();
        return Ok(());
    }
    if apply {
        eprintln!(
            "tt plan --apply: hosted backend not wired (cloud repo + auth required). \
             For now, review the projection here and apply via the dashboard once it ships."
        );
    }
    let input_path = input
        .ok_or_else(|| anyhow::anyhow!("usage: tt plan --input <plan_input.json>  (or --example)"))?;

    let raw = std::fs::read_to_string(input_path)
        .map_err(|e| anyhow::anyhow!("read {input_path}: {e}"))?;
    let plan_input: tt_plan_core::PlanInput =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parse {input_path}: {e}"))?;

    let result = tt_plan_core::replay(plan_input)
        .map_err(|e| anyhow::anyhow!("replay failed: {e}"))?;

    let payload = match output {
        Some(p) if p.ends_with(".json") => serde_json::to_string_pretty(&result)?,
        _ => format_plan_text(&result),
    };

    match output {
        Some(p) if p != "-" => {
            std::fs::write(p, &payload)?;
            eprintln!("wrote plan result to {p}");
        }
        _ => {
            print!("{payload}");
        }
    }

    Ok(())
}

/// Human-readable summary of a [`tt_plan_core::PlanResult`]. Mirrors the
/// shape of `docs/03-plan-replay-design.md` § "CLI output format".
fn format_plan_text(r: &tt_plan_core::PlanResult) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# TokenTrimmer Plan\n\nplan_id   : {}\norg_id    : {}\nwindow    : {} → {}\nsample    : {} requests\n\n",
        r.plan_id, r.org_id, r.window_start.to_rfc3339(), r.window_end.to_rfc3339(), r.sample_size
    ));
    let a = &r.aggregates;
    out.push_str("## Aggregates\n\n");
    out.push_str(&format!(
        "  baseline_cost    ${:.4}\n  projected_cost   ${:.4}\n  projected_savings ${:.4} ({:.1}%)\n",
        a.total_baseline_cost_usd, a.total_projected_cost_usd, a.projected_savings_usd, a.projected_savings_pct
    ));
    out.push_str(&format!(
        "  cache_hit_rate   {:.1}%\n  p50_latency      {:.0}ms\n  p95_latency      {:.0}ms\n",
        a.cache_hit_rate_projected * 100.0, a.p50_latency_ms_projected, a.p95_latency_ms_projected
    ));
    out.push_str(&format!(
        "  requests: {} rerouted, {} unchanged, {} unprice-able\n\n",
        a.requests_rerouted, a.requests_unchanged, a.requests_unprice_able
    ));

    let c = &r.confidence_intervals;
    out.push_str("## 95% confidence intervals\n\n");
    out.push_str(&format!(
        "  savings_usd     ${:.4} – ${:.4}\n  savings_pct     {:.1}% – {:.1}%\n  cache_hit_rate  {:.1}% – {:.1}%\n  p50_latency_ms  {:.0} – {:.0}\n  p95_latency_ms  {:.0} – {:.0}\n\n",
        c.savings_usd_95.0, c.savings_usd_95.1,
        c.savings_pct_95.0, c.savings_pct_95.1,
        c.cache_hit_rate_95.0 * 100.0, c.cache_hit_rate_95.1 * 100.0,
        c.p50_latency_ms_95.0, c.p50_latency_ms_95.1,
        c.p95_latency_ms_95.0, c.p95_latency_ms_95.1,
    ));

    if !a.l2_projections.is_empty() {
        out.push_str("## L2 semantic cache sweep\n\n");
        out.push_str("  threshold  hit_rate  hits/total\n");
        for p in &a.l2_projections {
            out.push_str(&format!(
                "  {:>9.2}  {:>7.1}%  {}/{}\n",
                p.threshold, p.projected_l2_hit_rate * 100.0, p.projected_l2_hits, p.total
            ));
        }
        if a.l2_poisoning_candidates > 0 {
            out.push_str(&format!(
                "  ⚠ {} cache-poisoning candidate(s) detected (similar requests with divergent outcomes)\n",
                a.l2_poisoning_candidates
            ));
        }
        out.push('\n');
    }

    if !r.per_route_breakdown.is_empty() {
        out.push_str("## Per-route\n\n");
        for row in &r.per_route_breakdown {
            out.push_str(&format!(
                "  {} ({}): matched={} baseline=${:.4} projected=${:.4} saved=${:.4}\n",
                row.route_name, row.route_id, row.matched,
                row.baseline_cost_usd, row.projected_cost_usd, row.savings_usd
            ));
        }
        out.push('\n');
    }

    if !r.caveats.is_empty() {
        out.push_str("## Caveats\n\n");
        for c in &r.caveats {
            out.push_str(&format!("  - {c}\n"));
        }
    }

    out
}

/// Print a minimal example PlanInput to stdout. Users redirect to a file
/// and edit. Avoids the chicken-and-egg of "I want to try `tt plan` but
/// don't know the JSON shape".
fn print_plan_example() {
    let example = serde_json::json!({
        "plan_id": "00000000-0000-0000-0000-000000000001",
        "org_id":  "00000000-0000-0000-0000-000000000002",
        "window_start": "2026-05-01T00:00:00Z",
        "window_end":   "2026-05-08T00:00:00Z",
        "requests": [
            {
                "id": "00000000-0000-0000-0000-000000000010",
                "org_id": "00000000-0000-0000-0000-000000000002",
                "ts": "2026-05-01T12:00:00Z",
                "provider": "openai",
                "model": "gpt-4o",
                "input_tokens": 1000,
                "output_tokens": 200,
                "cached_tokens": 0,
                "cost_usd": 0.0045,
                "baseline_cost_usd": 0.0045,
                "cached": false,
                "cache_layer": null,
                "matched_route_id": null,
                "latency_ms": 800,
                "upstream_latency_ms": 750,
                "status": 200,
                "tag": null
            }
        ],
        "proposed_routes": [
            {
                "id": "00000000-0000-0000-0000-000000000099",
                "name": "cheap-for-short",
                "priority": 100,
                "enabled": true,
                "when": { "model_in": ["gpt-4o"], "input_tokens_lt": 2000 },
                "then": { "target_model": "gpt-4o-mini" }
            }
        ],
        "pricing": {
            "openai:gpt-4o-mini": {
                "input_per_million": 0.15,
                "output_per_million": 0.60,
                "cached_input_per_million": 0.075
            }
        },
        "config": {
            "l1_ttl_seconds": null,
            "l2_threshold_sweep": [0.85, 0.90, 0.92, 0.95],
            "l2_ttl_seconds": null
        },
        "seed": 42,
        "bootstrap_iterations": 1000
    });
    println!("{}", serde_json::to_string_pretty(&example).unwrap());
}

/// Implement `tt audit verify`.
///
/// v1 reads from `.claude/AUDIT-CHAIN.jsonl` (placeholder until the Postgres
/// audit writer ships in Week 7).  Each line must be a JSON object that
/// deserializes as [`tt_telemetry::audit::AuditEntry`].
fn run_audit_verify(org: Option<&str>, key_path: Option<&str>) -> anyhow::Result<()> {
    println!(
        "v1 audit verify reads from .claude/AUDIT-CHAIN.jsonl (placeholder until Postgres \
         audit writer ships in Week 7)."
    );

    let key_path = match key_path {
        Some(p) => p,
        None => {
            anyhow::bail!(
                "must provide --key <path> for verification: \
                 pass a file containing the hex-encoded Ed25519 verifying key"
            );
        }
    };

    // Load the verifying key from a hex-encoded file.
    let key_hex = std::fs::read_to_string(key_path)
        .map_err(|e| anyhow::anyhow!("failed to read key file {key_path}: {e}"))?;
    let key_bytes =
        hex::decode(key_hex.trim()).map_err(|e| anyhow::anyhow!("key hex decode failed: {e}"))?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("verifying key must be exactly 32 bytes (64 hex chars)"))?;
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
        .map_err(|e| anyhow::anyhow!("invalid Ed25519 verifying key: {e}"))?;

    let chain_path = Path::new(".claude/AUDIT-CHAIN.jsonl");
    if !chain_path.exists() {
        println!("no chain to verify (.claude/AUDIT-CHAIN.jsonl not found)");
        if let Some(o) = org {
            println!("(org filter --org={o} noted; no entries to filter)");
        }
        return Ok(());
    }

    // Parse entries line-by-line.
    let content = std::fs::read_to_string(chain_path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", chain_path.display()))?;

    let entries: Vec<tt_telemetry::audit::AuditEntry> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line).map_err(|e| {
                anyhow::anyhow!("failed to parse line {}: {e}", i + 1)
            })
        })
        .collect::<anyhow::Result<_>>()?;

    println!("loaded {} entries", entries.len());

    if let Some(o) = org {
        println!("(--org={o} noted; filtering is deferred to Week 7 Postgres writer)");
    }

    match tt_telemetry::audit::verify_chain(&entries, &verifying_key) {
        Ok(()) => {
            println!("chain OK — all {} entries verified", entries.len());
        }
        Err(e) => {
            anyhow::bail!("chain verification FAILED: {e}");
        }
    }

    Ok(())
}
