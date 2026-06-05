# V1c — Report Summaries + Status Styling Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V1c (third V1 sub-slice). Finishes the CLI visual refresh for the report commands + audit.
**Depends on:** V1a (#22) `ui` module, V1b (#23) command styling — both merged.

## Goal

Style the human-facing output of `tt inspect` / `tt plan` / `tt inspect --cost-diff` / `tt audit verify` **without touching the report bodies**. The bodies (markdown / JSON, built by core crates) are destined for files (`--output report.md`) and pipes (`tt plan | jq`), so they stay **byte-identical plain**. Color comes from a **styled summary line + status notes on stderr** — which never pollute the stdout report.

## Design

### New `ui` stderr printers (`crates/cli/src/ui.rs`)
The summary/status lines go to **stderr** (so a piped/redirected stdout report stays clean). `warn`/`error` already target stderr; add the success/neutral counterparts:
```rust
/// Success line on STDERR (green ✓) — does not pollute stdout reports.
pub fn ok(msg: &str) {
    eprintln!("{} {}", Style::new().green().for_stderr().apply_to(OK), msg);
}
/// Neutral/status line on STDERR (dim).
pub fn note(msg: &str) {
    eprintln!("{}", Style::new().dim().for_stderr().apply_to(msg));
}
```
(`for_stderr()` gates on the stderr color global — consistent with the V1a per-stream fix.)

### `run_inspect` (`main.rs`)
- After `print!`/file-write, compute counts by `Severity` and emit a summary to stderr:
  - 0 findings → `ui::ok("Clean — no findings")`
  - findings incl. `Critical` → `ui::error(&format!("{n} finding(s) · {c} critical · {h} high · {m} medium · {l} low"))`
  - findings, no critical → `ui::warn(&format!("{n} finding(s) · {h} high · {m} medium · {l} low"))`
- The `eprintln!("wrote {n} finding(s) to {p}")` status note → `ui::note(...)`.

### `run_plan` (`main.rs`)
- After output, summary from `result.aggregates`:
  - `projected_savings_usd > 0` → `ui::ok(&format!("Projected savings ${:.4} ({:.1}%) · {} of {} requests rerouted", savings_usd, savings_pct, requests_rerouted, sample_size))`
  - else → `ui::note("No projected savings for this config.")`
- `eprintln!("wrote plan result to {p}")` → `ui::note(...)`.
- (The `--apply` not-wired `bail!` is unchanged — it's an error the anyhow path renders.)

### `run_cost_diff` (`main.rs`)
- Summary from `report` (`is_increase()` / `net_projected_usd`):
  - increase → `ui::warn(&format!("Net +${:.6} per call projected", net))`
  - decrease → `ui::ok(&format!("Net −${:.6} per call projected", net.abs()))`
  - ~0 → `ui::note("No net per-call cost change projected.")`
- `eprintln!("wrote cost-diff report to {p}")` → `ui::note(...)`.

### `run_suggest_plan` (`main.rs`)
- `eprintln!("wrote plan-input skeleton to {p} …")` → `ui::note(...)`.

### `run_audit_verify` (`main.rs`)
- Status lines (`loaded {n} entries`, `(--org … noted)`, `verifying-key sourced …`, `no chain to verify …`) → `ui::note(...)`.
- The final successful verification result → `ui::ok("chain verified ({n} entries)")` (exact wording per the current success path).

### `main.rs` Init arm — remove the duplicate summary
The Init arm prints `"Done. {} written, {} skipped."` (main.rs:471) **on top of** the styled summary `init::run` already emits (V1b). Remove the redundant main.rs lines (`println!()` + the `"Done. …"`) — `init::run`'s styled summary is the single source.

## Testing
- The report **bodies are unchanged** → existing inspect/plan snapshot/format tests in the core crates are unaffected; `tt-cli` tests stay green.
- Add `ui` unit tests for `ok`/`note` formatting (color disabled → `"✓ msg"` / `"msg"`).
- Manual smoke: `tt inspect <dir>` (terminal) shows the plain report on stdout + a colored severity summary on stderr; `tt inspect <dir> | cat` → report on stdout, summary still on stderr (plain when stderr non-TTY); `tt plan --input x.json --output r.json` → file is pure JSON, summary on stderr.
- `cargo clippy --workspace --all-targets -D warnings`; `cargo fmt`.

## Notes / Out of Scope
- **Report-body coloring is deliberately not done** — bodies must stay file/pipe-clean (the chosen design).
- Per-token coloring within the summary (e.g. green `$` amount inside an otherwise-plain line) is skipped for robustness — each summary line is a single semantic color via `ok`/`warn`/`error`/`note`.
- `tt retrieval` / `tt proxy` / `tt mcp` output: not report commands; leave as-is (proxy has its own TUI).
- This completes V1 (CLI visual refresh) for the user-facing command surface.
