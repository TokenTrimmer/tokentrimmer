# V1b — CLI UI Rollout Design

**Status:** approved (design — applies the approved V1 design; V1a foundation merged in #22)
**Date:** 2026-06-05
**Slice:** V1b (second V1 sub-slice). Rolls the `tt_cli::ui` module out to the account + init commands and the gateway-migrate message.
**Depends on:** V1a (#22) merged — the `ui` module + its color/symbol/printer API.

## Goal

Apply the `ui` module to the remaining **command/status output** so the whole CLI speaks one visual voice. No new `ui` primitives — pure application of V1a's `success`/`warn`/`info`/`heading`/`muted` printers.

## Scope

### 1. `account` (`crates/cli/src/account/mod.rs`)
- **`login`** — `ui::success("Logged in. Stored {masked} in {path} (base: {base}).")`.
- **`whoami`** (configured): `ui::heading("Logged in")`, then key/value lines with `ui::muted()` labels and plain values: `key` / `base` / `config` (each `  {muted label} {value}`). (not configured): `ui::warn("Not logged in. Run `tt login --token <KEY>` or set TT_API_KEY.")` then a muted `base:` line, then `exit(1)` (unchanged).
- **`logout`** — removed: `ui::success("Logged out — removed {path}.")` + the revoke note via `ui::info` (muted). nothing-to-remove: `ui::info("Not logged in (nothing to remove).")`.

### 2. `init` (`crates/cli/src/init/mod.rs`)
- `+ Wrote {f} ({n} bytes)` → `ui::success(&format!("Wrote {f} ({n} bytes)"))`.
- `+ Updated .gitignore` / `+ Updated {f} (safe …)` → `ui::success`.
- `! Overwrote user-modified {f} (--force)` → `ui::warn`.
- `- Skipped {f} (…)` → `ui::info` (muted).
- Summary block: `ui::heading("Done")` (or similar), then the `Detected:` and `Files written: …, skipped: …` lines (the counts styled — written via `ui::success_style`/`muted` accents), and `Inspect baseline: …` via `ui::info`.

### 3. `main.rs`
- `"migrations applied"` (gateway `--migrate-only`) → `ui::success`.

## Out of Scope (V1c)
- **plan / inspect report bodies** — the `print!("{formatted}")` markdown documents and the `eprintln!("wrote … to {p}")` stderr status notes. Restyling these (and any colorized report sections) needs care to preserve the **stdout = report / stderr = status** piping contract, so it is a dedicated follow-up slice.
- `audit` subcommand output (low traffic; folds into V1c).

## Testing
- Existing `account`/`init` unit tests assert **logic** (credential storage, file writes, manifest), not stdout text, so output changes don't break them — confirm by running `cargo test -p tt-cli`.
- No new tests: this is application of already-tested `ui` printers. A manual smoke (`tt whoami`, `tt init --dry-run`) confirms the styled output; piped/`NO_COLOR` stays plain (inherited from V1a's color resolution).
- `cargo clippy --workspace --all-targets -D warnings`; `cargo fmt`.

## Notes
- `account`/`init` are lib modules → use `crate::ui`. `main.rs` is the bin → `tt_cli::ui`.
- All these commands are terminal-interactive (not part of a piped data path), so routing `success`/`heading`/`info` to **stdout** is correct; `warn` stays on **stderr** (V1a semantics).
