# V1a CLI UI Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A reusable `tt_cli::ui` module (colors, symbols, boxed tables, line printers, spinner; NO_COLOR/TTY-aware) and migrate the `tt route` commands to it.

**Architecture:** Pure formatting fns (`-> String`) wrapped by thin printers, so the module is unit-testable. Color via `console`'s process-global enable state; tables via `comfy-table`; spinners via `indicatif`. `tt-cli` is a lib+bin: `ui` lives in the lib (`lib.rs`), `main.rs` calls `ui::init`.

**Tech Stack:** `console` 0.15, `comfy-table` 7, `indicatif` 0.17. Spec: `docs/superpowers/specs/2026-06-05-v1a-cli-ui-foundation-design.md`.

---

## Task 1: The `ui` module + unit tests

**Files:**
- Modify: `crates/cli/Cargo.toml` (deps)
- Create: `crates/cli/src/ui.rs`
- Modify: `crates/cli/src/lib.rs` (`pub mod ui;`)

- [ ] **Step 1: Write the failing unit tests** — create `crates/cli/src/ui.rs` with ONLY the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_enabled_only_on_tty_without_no_color() {
        assert!(should_enable_color(false, false, true)); // tty, no flags → on
        assert!(!should_enable_color(true, false, true)); // --no-color → off
        assert!(!should_enable_color(false, true, true)); // NO_COLOR env → off
        assert!(!should_enable_color(false, false, false)); // piped/non-tty → off
    }

    #[test]
    fn line_formatters_have_expected_prefixes() {
        // Color disabled → deterministic plain text with the symbol prefix.
        console::set_colors_enabled(false);
        assert_eq!(format_success("done"), "✓ done");
        assert_eq!(format_error("nope"), "✗ nope");
        assert_eq!(format_warn("careful"), "! careful");
    }

    #[test]
    fn table_renders_header_and_rows() {
        console::set_colors_enabled(false);
        let mut t = table(&["NAME", "STATUS"]);
        t.add_row(vec!["vis-downgrade", "on"]);
        let out = t.to_string();
        assert!(out.contains("NAME"));
        assert!(out.contains("vis-downgrade"));
        assert!(out.contains('╭'), "rounded top-left corner present: {out}"); // UTF8_ROUND_CORNERS
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-cli --lib ui::`
Expected: FAIL to compile — `should_enable_color`, `format_success`, `table`, etc. don't exist; `console`/`comfy-table` not deps.

- [ ] **Step 3: Add dependencies**

In `crates/cli/Cargo.toml` `[dependencies]`:
```toml
console = "0.15"
comfy-table = "7"
indicatif = "0.17"
```

- [ ] **Step 4: Implement the `ui` module** (prepend above the test module in `crates/cli/src/ui.rs`):

```rust
//! Terminal UI: the single place all `tt` output styling flows through.
//! Pure `format_*`/`table` helpers return `String`s (unit-testable); thin
//! printers write them. Color is gated by `console`'s process-global enable
//! state, set once by [`init`].

use std::time::Duration;

use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{ContentArrangement, Table};
use console::Style;
use indicatif::{ProgressBar, ProgressStyle};

/// Resolve + apply color enablement once, at startup. Colors are on only when
/// stdout is a TTY, `NO_COLOR` is unset, and `--no-color` was not passed.
pub fn init(no_color_flag: bool) {
    let enabled = should_enable_color(
        no_color_flag,
        std::env::var_os("NO_COLOR").is_some(),
        console::user_attended(),
    );
    console::set_colors_enabled(enabled);
}

/// Pure color-enable decision (unit-tested).
pub fn should_enable_color(no_color_flag: bool, no_color_env: bool, is_tty: bool) -> bool {
    !no_color_flag && !no_color_env && is_tty
}

// --- semantic styles (respect the global enable state) ---
pub fn heading_style() -> Style {
    Style::new().bold()
}
pub fn accent() -> Style {
    Style::new().cyan()
}
pub fn success_style() -> Style {
    Style::new().green()
}
pub fn error_style() -> Style {
    Style::new().red()
}
pub fn warn_style() -> Style {
    Style::new().yellow()
}
pub fn muted() -> Style {
    Style::new().dim()
}

// --- symbols ---
pub const OK: &str = "✓";
pub const NO: &str = "✗";
pub const DOT_ON: &str = "●";
pub const DOT_OFF: &str = "○";
pub const ARROW: &str = "→";
pub const BULLET: &str = "·";

// --- pure formatters ---
pub fn format_success(msg: &str) -> String {
    format!("{} {}", success_style().apply_to(OK), msg)
}
pub fn format_error(msg: &str) -> String {
    format!("{} {}", error_style().apply_to(NO), msg)
}
pub fn format_warn(msg: &str) -> String {
    format!("{} {}", warn_style().apply_to("!"), msg)
}
pub fn format_heading(msg: &str) -> String {
    heading_style().apply_to(msg).to_string()
}

// --- printers ---
pub fn success(msg: &str) {
    println!("{}", format_success(msg));
}
pub fn error(msg: &str) {
    eprintln!("{}", format_error(msg));
}
pub fn warn(msg: &str) {
    eprintln!("{}", format_warn(msg));
}
pub fn info(msg: &str) {
    println!("{}", muted().apply_to(msg));
}
pub fn heading(msg: &str) {
    println!("{}", format_heading(msg));
}

/// A `comfy-table` preconfigured with rounded UTF-8 borders + dynamic width.
pub fn table(headers: &[&str]) -> Table {
    let mut t = Table::new();
    t.load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(headers.iter().map(|h| heading_style().apply_to(h).to_string()));
    t
}

/// A spinner for a network call. Hidden automatically when stderr is not a TTY
/// (indicatif's default draw target). Call `.finish_and_clear()` when done.
pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}
```

In `crates/cli/src/lib.rs`, add `pub mod ui;` (alongside the other `pub mod` declarations).

NOTE: `set_header` takes `IntoIterator<Item = impl Into<Cell>>`; `String` is `Into<Cell>`. Confirm the comfy-table 7 method names (`load_preset`, `apply_modifier`, `set_content_arrangement`, `set_header`, `add_row`, `to_string`) against the resolved version — adjust if the builder API differs.

- [ ] **Step 5: Run to verify green**

Run: `cargo test -p tt-cli --lib ui::`
Expected: PASS — color truth table, formatters, table render all green.

- [ ] **Step 6: Commit**

```bash
git add crates/cli/Cargo.toml crates/cli/src/ui.rs crates/cli/src/lib.rs
git commit -m "feat(cli): ui module — colors, symbols, tables, spinners (NO_COLOR/TTY-aware)"
```

---

## Task 2: Migrate `tt route` to `ui`

**Files:**
- Modify: `crates/cli/src/route/mod.rs` (`print_routes` → table; `add`/`rm`/`show` printers; spinners)
- Modify: `crates/cli/src/main.rs` (`--no-color` global flag + `ui::init`)

- [ ] **Step 1: Write the failing render test** (in `crates/cli/src/route/mod.rs` tests)

```rust
    #[test]
    fn routes_table_renders_names_targets_and_status() {
        console::set_colors_enabled(false);
        let routes = json!([
            { "id": "a", "name": "vis", "priority": 100, "enabled": true,
              "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} },
            { "id": "b", "name": "capped", "priority": 50, "enabled": false,
              "when": {}, "then": {"target_model":"claude-haiku"} },
        ]);
        let out = routes_table(&routes);
        assert!(out.contains("vis"));
        assert!(out.contains("gpt-4o-mini"));
        assert!(out.contains("on"));
        assert!(out.contains("off"));
        assert!(out.contains('→')); // from → target
    }
```

(Add `use console;` access — `console` is now a dep; reference via `crate::ui` styles. Import `crate::ui` in the route module.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-cli routes_table_renders`
Expected: FAIL — `routes_table` doesn't exist.

- [ ] **Step 3: Implement `routes_table` + migrate printers**

In `crates/cli/src/route/mod.rs`, add `use crate::ui;` and replace `print_routes` with a pure builder + thin printer:

```rust
/// Pure: render the routes list as a styled table string.
fn routes_table(routes: &Value) -> String {
    let Some(arr) = routes.as_array() else {
        return ui::format_warn("unexpected response (not a list)");
    };
    if arr.is_empty() {
        return "No routes. Create one with `tt route add --from <model> --to <model>`.".into();
    }
    let mut t = ui::table(&["NAME", "ROUTE", "PRIO", "STATUS"]);
    for r in arr {
        let name = r["name"].as_str().unwrap_or("?");
        let target = r["then"]["target_model"].as_str().unwrap_or("?");
        let from = r["when"]["model_in"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("*");
        let route_cell = format!(
            "{} {} {}",
            ui::accent().apply_to(from),
            ui::BULLET, // placeholder; replaced below
            ui::accent().apply_to(target)
        );
        // from → target
        let route_cell = format!(
            "{} {} {}",
            ui::accent().apply_to(from),
            ui::ARROW,
            ui::accent().apply_to(target)
        );
        let _ = route_cell; // keep the second binding
        let status = if r["enabled"].as_bool().unwrap_or(false) {
            format!("{} on", ui::success_style().apply_to(ui::OK))
        } else {
            ui::muted().apply_to(format!("{} off", ui::NO)).to_string()
        };
        t.add_row(vec![
            ui::heading_style().apply_to(name).to_string(),
            format!(
                "{} {} {}",
                ui::accent().apply_to(from),
                ui::ARROW,
                ui::accent().apply_to(target)
            ),
            r["priority"].as_u64().unwrap_or(0).to_string(),
            status,
        ]);
    }
    format!("{}\n{}", ui::format_heading(&format!("ROUTES {} {}", ui::BULLET, arr.len())), t)
}

fn print_routes(routes: &Value) {
    println!("{}", routes_table(routes));
}
```

(Clean up the duplicate `route_cell` bindings shown above — the final `add_row` builds the `from → target` cell inline; the two scratch `let route_cell` lines are illustrative and should NOT be in the final code.)

Migrate the success messages and add spinners around the HTTP `send` calls:
- `RouteCmd::List`: `let sp = ui::spinner("Loading routes…"); let routes = send(...).await?; sp.finish_and_clear(); print_routes(&routes);`
- `RouteCmd::Add`: spinner "Creating route…"; then `ui::success(&format!("Created route {} ({}).", id, name));`
- `RouteCmd::Rm`: spinner "Removing route…"; then `ui::success(&format!("Removed route {id}."));`
- `RouteCmd::Show`: spinner "Loading route…"; then a styled key/value block (`ui::heading` for the name, fields via `ui::accent`/`ui::muted`) — keep `serde_json::to_string_pretty` as the body for now, prefixed by a heading.

- [ ] **Step 4: Add the `--no-color` flag + `ui::init` in `main.rs`**

In `crates/cli/src/main.rs`, add to the top-level `Cli` struct:
```rust
    /// Disable colored output.
    #[arg(long, global = true)]
    no_color: bool,
```
Immediately after `let cli = Cli::parse();`, add: `tt_cli::ui::init(cli.no_color);`
(Confirm the parsed variable name; if `main` matches on `Cli::parse()` inline, bind it first.)

- [ ] **Step 5: Run to verify green**

Run: `cargo test -p tt-cli`
Expected: PASS — `routes_table_renders…` green; existing `build_new_route` tests unaffected.

- [ ] **Step 6: Manual smoke (optional but recommended)**

```bash
cargo run -q -p tt-cli -- route add --help   # still works
# On a TTY, `tt route list` would show a boxed table; piped → plain (set NO_COLOR=1 to verify).
```

- [ ] **Step 7: Commit**

```bash
git add crates/cli/src/route/mod.rs crates/cli/src/main.rs
git commit -m "feat(cli): style tt route output via ui (boxed table, symbols, spinners)"
```

---

## Task 3: Final verification

- [ ] **Step 1: fmt + workspace clippy + tests**

```bash
cargo fmt -p tt-cli
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p tt-cli
```
Expected: fmt clean, clippy clean (watch for `console`/`comfy-table` unused-import or needless-borrow lints), all tests green.

- [ ] **Step 2: Commit any fmt changes**

```bash
git commit -am "style: cargo fmt (v1a)" || echo "nothing to commit"
```

---

## Self-review notes
- **Testability:** `should_enable_color`, `format_*`, `table`, and `routes_table` are pure (return `String`/`Table`); tests force `console::set_colors_enabled(false)` for deterministic plain output.
- **NO_COLOR/TTY:** `init` resolves once and sets the `console` global; `--no-color` + `NO_COLOR` + non-TTY all disable. The spinner self-hides off-TTY (indicatif default draw target).
- **No churn to existing tests:** `build_new_route` mapping is unchanged; only rendering changed. The route module gains `use crate::ui;`.
- **API caveat:** comfy-table 7 builder method names + `set_header` `Into<Cell>` bound are confirmed at compile time in Task 1; adjust if the resolved minor differs.
- **Scope:** V1a is the `ui` module + `tt route`; V1b rolls it out to the other commands.
