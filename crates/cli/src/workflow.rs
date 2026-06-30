//! `tt workflow` — offline validate + cost-estimate for a WorkflowDefinition JSON.
//!
//! `tt workflow check <file.json>` parses the definition, validates it
//! structurally (offline — all pinned models are accepted; `Auto` is rejected),
//! projects the cost with `estimate_workflow`, and prints a per-node table with
//! warnings.  Optional flags allow writing a machine-readable baseline dump
//! (`--output`) and comparing against a prior dump (`--baseline`), with
//! `--fail-on-cost-increase` for CI gates.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::ui;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// `tt workflow check` — offline validate + cost-estimate + optional baseline diff.
///
/// * `file`    — path to a `WorkflowDefinition` JSON.
/// * `inputs`  — optional JSON string substituted for `{{input}}` in prompts.
/// * `baseline`— path to a prior `WorkflowEstimate` JSON (from `--output`).
/// * `fail_on_cost_increase` — exit non-zero when the current cost exceeds the
///   baseline by more than floating-point epsilon.
/// * `output`  — write the current `WorkflowEstimate` JSON here (for a future
///   `--baseline` comparison).
pub fn check(
    file: PathBuf,
    inputs: Option<String>,
    baseline: Option<PathBuf>,
    fail_on_cost_increase: bool,
    output: Option<String>,
) -> anyhow::Result<()> {
    // ---- 1. Parse the workflow definition ----------------------------------
    let raw = std::fs::read_to_string(&file)
        .with_context(|| format!("reading workflow file {}", file.display()))?;
    let def: tt_core::workflow::types::WorkflowDefinition = serde_json::from_str(&raw)
        .with_context(|| format!("parsing workflow JSON {}", file.display()))?;

    // ---- 2. Parse `--inputs` -----------------------------------------------
    let inputs_val = match inputs {
        Some(s) => {
            serde_json::from_str::<serde_json::Value>(&s).context("parsing --inputs JSON")?
        }
        None => serde_json::Value::Null,
    };

    // ---- 3. Validate (offline — model_exists = |_| true) -------------------
    // Note: `ModelSelection::Auto` is unconditionally rejected by `validate`
    // regardless of `model_exists`.  All other pinned model ids are accepted
    // (no registry available offline).
    if let Err(errors) = tt_core::workflow::validate::validate(&def, &|_| true) {
        ui::error(&format!(
            "workflow validation failed ({} error{}):",
            errors.len(),
            if errors.len() == 1 { "" } else { "s" }
        ));
        for e in &errors {
            eprintln!("  {} {e}", ui::accent().apply_to("·"));
        }
        anyhow::bail!("workflow validation failed");
    }

    // ---- 4. Estimate -------------------------------------------------------
    let est = tt_core::workflow::estimate::estimate_workflow(&def, &inputs_val);

    // ---- 5. Print results --------------------------------------------------
    print_estimate(&est);

    // ---- 6. Write estimate JSON if `--output` is given ---------------------
    if let Some(ref out_path) = output {
        let json = serde_json::to_string_pretty(&est).context("serializing estimate")?;
        std::fs::write(out_path, json)
            .with_context(|| format!("writing estimate to {out_path}"))?;
        ui::ok(&format!("estimate written to {out_path}"));
    }

    // ---- 7. Baseline diff if `--baseline` is given -------------------------
    if let Some(baseline_path) = baseline {
        let baseline_raw = std::fs::read_to_string(&baseline_path)
            .with_context(|| format!("reading baseline {}", baseline_path.display()))?;
        let baseline_est: tt_core::workflow::estimate::WorkflowEstimate =
            serde_json::from_str(&baseline_raw)
                .with_context(|| format!("parsing baseline JSON {}", baseline_path.display()))?;

        print_diff(&est, &baseline_est);

        let net_delta = est.projected_cost_usd - baseline_est.projected_cost_usd;
        // Use a small epsilon to absorb floating-point noise.
        const EPS: f64 = 1e-9;
        if fail_on_cost_increase && net_delta > EPS {
            anyhow::bail!(
                "--fail-on-cost-increase: projected cost increased by ${:.8} \
                 (baseline ${:.8} → current ${:.8})",
                net_delta,
                baseline_est.projected_cost_usd,
                est.projected_cost_usd,
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Print helpers
// ---------------------------------------------------------------------------

fn print_estimate(est: &tt_core::workflow::estimate::WorkflowEstimate) {
    ui::heading("Workflow cost estimate");
    println!(
        "  projected cost: {}",
        ui::heading_style().apply_to(format!("${:.8}", est.projected_cost_usd))
    );

    if !est.per_node.is_empty() {
        let mut table = ui::table(&["NODE", "MODEL", "COST (USD)"], console::colors_enabled());
        for n in &est.per_node {
            table.add_row(vec![
                n.node_id.clone(),
                n.model.as_deref().unwrap_or("-").to_string(),
                n.cost_usd
                    .map(|c| format!("${c:.8}"))
                    .unwrap_or_else(|| "-".to_string()),
            ]);
        }
        println!("{table}");
    }

    for w in &est.warnings {
        ui::warn(&format!("warning: {w}"));
    }

    ui::note(
        "Note: estimate is a linear projection — a Branch counts all arms; \
         loops are not multiplied by max_iters.",
    );
}

fn print_diff(
    current: &tt_core::workflow::estimate::WorkflowEstimate,
    baseline: &tt_core::workflow::estimate::WorkflowEstimate,
) {
    ui::heading("Cost diff vs. baseline");

    let baseline_map: HashMap<&str, Option<f64>> = baseline
        .per_node
        .iter()
        .map(|n| (n.node_id.as_str(), n.cost_usd))
        .collect();

    for node in &current.per_node {
        let prior = baseline_map.get(node.node_id.as_str()).copied().flatten();
        let delta = match (node.cost_usd, prior) {
            (Some(c), Some(p)) => Some(c - p),
            _ => None,
        };
        let arrow = match delta {
            Some(d) if d > 1e-12 => "▲",
            Some(d) if d < -1e-12 => "▼",
            _ => "=",
        };
        let delta_str = delta
            .map(|d| {
                if d.abs() < 1e-12 {
                    " (unchanged)".to_string()
                } else {
                    format!(" ({arrow} ${:.8})", d.abs())
                }
            })
            .unwrap_or_else(|| " (n/a)".to_string());
        println!(
            "  {} {}: {}{}",
            arrow,
            node.node_id,
            node.cost_usd
                .map(|c| format!("${c:.8}"))
                .unwrap_or_else(|| "-".to_string()),
            delta_str,
        );
    }

    let net = current.projected_cost_usd - baseline.projected_cost_usd;
    let net_arrow = if net > 1e-9 {
        "▲"
    } else if net < -1e-9 {
        "▼"
    } else {
        "="
    };
    println!(
        "  net: ${:.8} (baseline ${:.8}) → {net_arrow} ${:.8}",
        current.projected_cost_usd,
        baseline.projected_cost_usd,
        net.abs(),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tt_core::workflow::estimate::WorkflowEstimate;

    // ---- shared fixtures ------------------------------------------------

    /// A minimal valid workflow JSON: Trigger → Model(gpt-4o-mini) → Output.
    fn valid_workflow_json() -> &'static str {
        r#"{
          "id": "00000000-0000-0000-0000-000000000000",
          "version": 1,
          "name": "test",
          "nodes": [
            {"id": "t", "type": "trigger"},
            {
              "id": "m",
              "type": "model",
              "selection": {"type": "model", "model": "gpt-4o-mini"},
              "prompt": "Summarize: {{input}}"
            },
            {"id": "o", "type": "output"}
          ],
          "edges": [
            {"from": "t", "to": "m"},
            {"from": "m", "to": "o"}
          ]
        }"#
    }

    /// A workflow JSON with a dangling edge (edge.to references a missing node).
    fn invalid_workflow_json() -> &'static str {
        r#"{
          "id": "00000000-0000-0000-0000-000000000001",
          "version": 1,
          "name": "bad",
          "nodes": [
            {"id": "t", "type": "trigger"},
            {
              "id": "m",
              "type": "model",
              "selection": {"type": "model", "model": "gpt-4o-mini"},
              "prompt": "hello"
            },
            {"id": "o", "type": "output"}
          ],
          "edges": [
            {"from": "t", "to": "m"},
            {"from": "m", "to": "missing_node"}
          ]
        }"#
    }

    // ---- tests ----------------------------------------------------------

    /// A valid fixture definition → check() returns Ok and estimates a cost.
    #[test]
    fn check_estimates_and_prints() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("workflow.json");
        std::fs::write(&file, valid_workflow_json()).expect("write fixture");

        let result = check(file, None, None, false, None);
        assert!(
            result.is_ok(),
            "expected Ok for a valid workflow; got {result:?}"
        );
    }

    /// A definition with a dangling edge → check() returns Err (validation fails).
    #[test]
    fn check_rejects_invalid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("invalid.json");
        std::fs::write(&file, invalid_workflow_json()).expect("write fixture");

        let result = check(file, None, None, false, None);
        assert!(
            result.is_err(),
            "expected Err for a workflow with a dangling edge; got Ok"
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("validation failed"),
            "error message should mention 'validation failed'; got: {msg}"
        );
    }

    /// Baseline with cost=0 + a costlier def + --fail-on-cost-increase → Err.
    #[test]
    fn check_baseline_diff_fails_on_increase() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Write the workflow definition to a file.
        let file = dir.path().join("workflow.json");
        std::fs::write(&file, valid_workflow_json()).expect("write fixture");

        // Construct a cheap baseline estimate (projected_cost_usd = 0.0).
        let cheap_baseline = WorkflowEstimate {
            projected_cost_usd: 0.0,
            per_node: vec![],
            warnings: vec![],
        };
        let baseline_path = dir.path().join("baseline.json");
        std::fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&cheap_baseline).expect("serialize baseline"),
        )
        .expect("write baseline");

        // The valid workflow uses gpt-4o-mini which has a positive projected
        // cost, so net_delta > 0 → fail_on_cost_increase must trigger.
        let result = check(file, None, Some(baseline_path), true, None);
        assert!(
            result.is_err(),
            "expected Err when projected cost exceeds the baseline"
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("fail-on-cost-increase") || msg.contains("increased"),
            "error message should mention cost increase; got: {msg}"
        );
    }
}
