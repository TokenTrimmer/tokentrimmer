# V1b CLI UI Rollout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Apply the `ui` printers to `account`, `init`, and the gateway-migrate message.

**Architecture:** Pure application of V1a's `ui::{success,warn,info,heading,muted}`. No new primitives, no logic change. Existing tests assert logic (not stdout), so verification = tests still pass + clippy + smoke.

**Tech Stack:** `tt-cli`. Spec: `docs/superpowers/specs/2026-06-05-v1b-cli-ui-rollout-design.md`.

---

## Task 1: Style `account` (login / whoami / logout)

**Files:** Modify `crates/cli/src/account/mod.rs` (add `use crate::ui;`)

- [ ] **Step 1:** Add `use crate::ui;` to the imports.
- [ ] **Step 2:** `login` — replace the `println!("Logged in. Stored …")` with:
  ```rust
  ui::success(&format!(
      "Logged in. Stored {} in {} (base: {}).",
      context::mask_key(&validated),
      dir.join("credentials.toml").display(),
      base,
  ));
  ```
- [ ] **Step 3:** `whoami` configured branch — replace the four `println!`s with:
  ```rust
  ui::heading("Logged in");
  println!("  {} {} (source: {})", ui::muted().apply_to("key:   "), context::mask_key(k.expose()), ctx.key_source);
  println!("  {} {} (source: {})", ui::muted().apply_to("base:  "), ctx.base_url, ctx.base_source);
  println!("  {} {}", ui::muted().apply_to("config:"), store::config_dir().display());
  ```
  not-configured branch:
  ```rust
  ui::warn("Not logged in. Run `tt login --token <KEY>` or set TT_API_KEY.");
  eprintln!("  {} {} (source: {})", ui::muted().apply_to("base:"), ctx.base_url, ctx.base_source);
  std::process::exit(1);
  ```
- [ ] **Step 4:** `logout` — removed branch:
  ```rust
  ui::success(&format!("Logged out — removed {}.", dir.join("credentials.toml").display()));
  ui::info("Note: this only clears the local key; it does not revoke it server-side. Revoke in the dashboard if it may be compromised.");
  ```
  else branch: `ui::info("Not logged in (nothing to remove).");`
- [ ] **Step 5:** Run `cargo test -p tt-cli --lib account::` → existing tests pass (they assert credential logic, not output). Build clean.
- [ ] **Step 6:** Commit: `git commit -am "feat(cli): style tt login/whoami/logout via ui"`

---

## Task 2: Style `init`

**Files:** Modify `crates/cli/src/init/mod.rs` (add `use crate::ui;`)

- [ ] **Step 1:** Add `use crate::ui;`.
- [ ] **Step 2:** Replace the per-file status `println!`s:
  - `println!("+ Wrote {} ({} bytes)", f.dest.display(), f.content.len());` → `ui::success(&format!("Wrote {} ({} bytes)", f.dest.display(), f.content.len()));`
  - `println!("+ Updated .gitignore");` → `ui::success("Updated .gitignore");`
  - `println!("+ Updated {} (safe — unchanged from prior install)", f.dest.display());` → `ui::success(&format!("Updated {} (safe — unchanged from prior install)", f.dest.display()));`
  - `println!("! Overwrote user-modified {} (--force)", f.dest.display());` → `ui::warn(&format!("Overwrote user-modified {} (--force)", f.dest.display()));`
  - `println!("- Skipped {} (user-modified; --force to overwrite)", f.dest.display());` → `ui::info(&format!("Skipped {} (user-modified; --force to overwrite)", f.dest.display()));`
- [ ] **Step 3:** Summary block — replace:
  ```rust
  println!();
  ui::heading("Done");
  println!("  {} {:?} + frameworks {:?}", ui::muted().apply_to("detected:"), detection.languages, detection.frameworks);
  println!("  {} {} written, {} skipped", ui::muted().apply_to("files:   "), ui::success_style().apply_to(written), skipped);
  if let Some(n) = baseline_findings {
      ui::info(&format!("Inspect baseline: {n} findings -> .claude/inspect-baseline.json"));
  }
  ```
- [ ] **Step 4:** Run `cargo test -p tt-cli --lib init::` → existing tests pass (file-write logic unchanged). Build clean.
- [ ] **Step 5:** Commit: `git commit -am "feat(cli): style tt init output via ui"`

---

## Task 3: `main.rs` migrate message + final verification

**Files:** Modify `crates/cli/src/main.rs`

- [ ] **Step 1:** `println!("migrations applied");` → `tt_cli::ui::success("migrations applied");`
- [ ] **Step 2:** fmt + workspace clippy + tests:
  ```bash
  cargo fmt -p tt-cli
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test -p tt-cli
  ```
  Expected: clean + green.
- [ ] **Step 3:** Smoke: `cargo run -q -p tt-cli -- init --dry-run --path /tmp/v1b-smoke 2>&1 | head` shows styled output; `NO_COLOR=1 …` plain.
- [ ] **Step 4:** Commit: `git commit -am "feat(cli): style gateway migrate message via ui"` (or fold into Task 3's changes).

---

## Self-review notes
- No new `ui` primitives — pure application; the printers + color resolution were tested/hardened in V1a.
- stdout for `success`/`heading`/`info` (terminal commands, not piped data); `warn` on stderr (V1a semantics). whoami's not-logged-in `base:` goes to stderr alongside the warn.
- Existing tests assert logic, not stdout text → unaffected; verification is "still green" + smoke.
