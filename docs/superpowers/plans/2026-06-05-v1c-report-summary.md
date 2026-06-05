# V1c Report Summaries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Steps use checkbox (`- [ ]`).

**Goal:** Add `ui::ok`/`ui::note` stderr printers, then emit a styled summary + status note for the report commands (bodies stay plain).

**Architecture:** Pure `format_ok` (testable) + thin printers; summary logic lives in `main.rs`'s `run_*` functions where the result data is in hand. Report bodies untouched.

**Tech Stack:** `tt-cli`. Spec: `docs/superpowers/specs/2026-06-05-v1c-report-summary-design.md`.

---

## Task 1: `ui::ok` / `ui::note` stderr printers

**Files:** Modify `crates/cli/src/ui.rs`

- [ ] **Step 1: Write the failing test** (in the `ui` tests module):
```rust
    #[test]
    fn ok_formatter_has_check_prefix() {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
        assert_eq!(format_ok("done"), "✓ done");
    }
```
- [ ] **Step 2:** `cargo test -p tt-cli --lib ui::ok_formatter` → FAIL (no `format_ok`).
- [ ] **Step 3: Implement** (after `format_heading`, near the printers):
```rust
/// Success line content for STDERR (green ✓). Stderr-gated.
#[must_use]
pub fn format_ok(msg: &str) -> String {
    format!("{} {}", Style::new().green().for_stderr().apply_to(OK), msg)
}

/// Success line on STDERR — does not pollute stdout reports.
pub fn ok(msg: &str) {
    eprintln!("{}", format_ok(msg));
}

/// Neutral/status line on STDERR (dim).
pub fn note(msg: &str) {
    eprintln!("{}", Style::new().dim().for_stderr().apply_to(msg));
}
```
- [ ] **Step 4:** `cargo test -p tt-cli --lib ui::` → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(cli): ui::ok/note stderr status printers"`

---

## Task 2: Summaries + status notes in `main.rs`

**Files:** Modify `crates/cli/src/main.rs`

- [ ] **Step 1: `run_inspect`** — replace `eprintln!("wrote {} finding(s) to {p}", findings.len())` with `tt_cli::ui::note(&format!("wrote {} finding(s) to {p}", findings.len()))`. After the `match output { … }` block (before the gating `if !above.is_empty()`), add a severity summary:
```rust
    use tt_inspect_core::Severity;
    let count = |s: Severity| findings.iter().filter(|f| f.severity == s).count();
    let (c, h, m, l) = (
        count(Severity::Critical),
        count(Severity::High),
        count(Severity::Medium),
        count(Severity::Low),
    );
    if findings.is_empty() {
        tt_cli::ui::ok("Clean — no findings");
    } else if c > 0 {
        tt_cli::ui::error(&format!(
            "{} finding(s) · {c} critical · {h} high · {m} medium · {l} low",
            findings.len()
        ));
    } else {
        tt_cli::ui::warn(&format!(
            "{} finding(s) · {h} high · {m} medium · {l} low",
            findings.len()
        ));
    }
```
(Confirm `Severity` derives `PartialEq` for `f.severity == s`; if not, match on it instead.)

- [ ] **Step 2: `run_plan`** — change `eprintln!("wrote plan result to {p}")` → `tt_cli::ui::note(...)`. After the output `match`, add:
```rust
    let agg = &result.aggregates;
    if agg.projected_savings_usd > 0.0 {
        tt_cli::ui::ok(&format!(
            "Projected savings ${:.4} ({:.1}%) · {} of {} requests rerouted",
            agg.projected_savings_usd, agg.projected_savings_pct, agg.requests_rerouted, result.sample_size
        ));
    } else {
        tt_cli::ui::note("No projected savings for this config.");
    }
```
(Confirm field names: `aggregates.projected_savings_usd`, `projected_savings_pct`, `requests_rerouted`, and `result.sample_size` — adjust to the actual `PlanResult` shape.)

- [ ] **Step 3: `run_cost_diff`** — change the `eprintln!("wrote cost-diff report to {p}")` → `tt_cli::ui::note(...)`. After the output `match`, add:
```rust
    if report.is_increase() {
        tt_cli::ui::warn(&format!("Net +${:.6} per call projected", report.net_projected_usd));
    } else if report.net_projected_usd < 0.0 {
        tt_cli::ui::ok(&format!("Net −${:.6} per call projected", report.net_projected_usd.abs()));
    } else {
        tt_cli::ui::note("No net per-call cost change projected.");
    }
```
(Confirm `report.net_projected_usd` / `is_increase()` names.)

- [ ] **Step 4: `run_suggest_plan`** — `eprintln!("wrote plan-input skeleton to {p} …")` → `tt_cli::ui::note(...)`.

- [ ] **Step 5: `run_audit_verify`** — convert the status `println!`s (`loaded {n} entries`, `no chain to verify …`, `(--org … noted …)`, `verifying-key sourced …`) to `tt_cli::ui::note(...)`, and the final success result `println!` to `tt_cli::ui::ok(...)`. (Read the function; keep wording, only change the printer.)

- [ ] **Step 6: Init arm dedup** — remove the redundant summary in the `Command::Init` arm:
```rust
            let report = run(opts).context("tt init failed")?;
            let _ = report; // styled summary already emitted by init::run
```
i.e. delete the `println!();` + `println!("Done. {} written, {} skipped.", …)` lines (init's own styled "Done" block is the single source). If `report` becomes unused, drop the binding: `run(opts).context("tt init failed")?;`.

- [ ] **Step 7:** `cargo build -p tt-cli` then `cargo test -p tt-cli` → green (no body/test changes).

- [ ] **Step 8: Commit** `git commit -am "feat(cli): styled stderr summaries for inspect/plan/cost-diff/audit; dedup init summary"`

---

## Task 3: Final verification

- [ ] **Step 1:**
```bash
cargo fmt -p tt-cli
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p tt-cli
```
Expected: clean + green.
- [ ] **Step 2: Smoke** — `tt inspect <some-dir>` shows plain report on stdout + colored severity summary on stderr; `tt inspect <dir> | cat` keeps the report on stdout (plain). (Use a small repo path; the gating `bail!` on findings is expected.)
- [ ] **Step 3: Commit** any fmt: `git commit -am "style: cargo fmt (v1c)" || echo none`

---

## Self-review notes
- Bodies untouched → core-crate format tests unaffected; the only new test is `format_ok`.
- All summaries/status go to **stderr** via `ok`/`warn`/`error`/`note` (stderr-gated styles) → stdout report stays byte-clean for files/pipes.
- Field/method names (`PlanResult.aggregates.*`, cost-diff `report.*`, `Severity` eq) are confirmed at compile time in Task 2; adjust to the real shapes.
