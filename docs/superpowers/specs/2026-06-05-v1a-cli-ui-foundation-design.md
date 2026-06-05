# V1a — CLI UI Foundation Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V1a (first of two V1 sub-slices; V1b = roll the `ui` module out to the remaining commands).
**Part of:** V1 "CLI visual refresh" — graphically refresh the `tt` CLI with a cohesive design system.

## Goal

A reusable `tt_cli::ui` module — the single place all terminal output flows through — and migrate the `tt route` commands (`list`/`show`/`add`/`rm`) to it as the working proof. Colors, status symbols, boxed tables, styled errors/headings, and spinners on network calls — consistent, and automatically plain when piped or `NO_COLOR` is set.

Today the CLI has zero styling: hand-padded `println!` columns (e.g. `route::print_routes`) scattered across commands.

## Architecture

New module `crates/cli/src/ui.rs`, structured so the formatting is **pure (returns `String`)** and thin wrappers print it — keeping it unit-testable.

- **Color resolution** — `ui::init(no_color_flag: bool)` called once in `main()`. Resolves `enabled = !no_color_flag && env NO_COLOR unset && stdout is a TTY`, then calls `console::set_colors_enabled(enabled)` (the process-global `console` respects this for all `Style`s). A global `--no-color` clap flag on the top-level `Cli` feeds `no_color_flag`. Pure helper `ui::should_enable_color(no_color_flag, no_color_env, is_tty) -> bool` is unit-tested.
- **`style`** — semantic `console::Style` constructors: `heading()` (bold), `accent()` (cyan — models/highlights), `success()` (green), `error()` (red), `warn()` (yellow), `muted()` (dim). Each respects the global enable state.
- **`symbols`** — `OK = "✓"`, `NO = "✗"`, `DOT_ON/DOT_OFF = "● / ○"`, `ARROW = "→"`, `BULLET = "·"`. (UTF-8; the terminal-fallback for non-UTF8 is a follow-up — comfy-table + console already degrade color.)
- **`table(headers: &[&str]) -> comfy_table::Table`** — preconfigured with the `UTF8_ROUND_CORNERS` preset and content-aware width; callers add rows. A pure `render`-to-`String` path for tests.
- **line printers** — `heading(s)`, `success(s)`, `error(s)`, `warn(s)`, `info(s)` write a styled line (errors to stderr with an `error()`-styled `✗ ` prefix). A pure `format_*` companion returns the `String` for tests.
- **`spinner(msg) -> Spinner`** — an `indicatif::ProgressBar::new_spinner()` wrapper that only animates on a TTY (no-op otherwise) and clears on `finish`. Used to wrap `route` HTTP calls.

**Dependencies (add to `crates/cli/Cargo.toml`, prefer workspace):** `console` (explicit; already transitive via `dialoguer`), `comfy-table`, `indicatif`.

## `tt route` migration (the proof)

`crates/cli/src/route/mod.rs`:
- **`print_routes`** → build a `ui::table(["NAME", "ROUTE", "PRIO", "STATUS"])`. Each row: name (bold), a `from → target` "route" cell (models in cyan; `*` when match-all), priority, and a status cell (`✓ on` green / `✗ off` dim). Title line `ROUTES · {n}` via `ui::heading`. Empty state via `ui::info` ("No routes. Create one with `tt route add …`."). Keep the underlying `from`/`target`/`enabled` data extraction; only rendering changes.
- **`show`** — pretty key/value block via `ui::heading` + styled fields (instead of raw `serde_json::to_string_pretty`). Keep `--json`-style raw output reachable if a flag exists; otherwise the styled view is the default.
- **`add`** — replace the `println!("Created route …")` with `ui::success("Created route {name} ({id}).")`.
- **`rm`** — `ui::success("Removed route {id}.")`.
- **network calls** (`send` in list/show/add/rm) — wrap each in a `ui::spinner` ("Loading routes…", "Creating route…", etc.) that clears before the result prints. Errors from `send` surface via the existing `anyhow` path; `main` renders them through `ui::error` (see below).
- **`run` error rendering** — the top-level error path in `main` for `tt route` prints failures via `ui::error` rather than the default `anyhow` chain.

`crates/cli/src/main.rs`:
- Add `#[arg(long, global = true)] no_color: bool` to `Cli`; call `ui::init(cli.no_color)` immediately after parsing.

## Testing
- **`ui` unit tests:** `should_enable_color` truth table (flag/env/TTY combinations); `symbols` constants; `format_success/error/...` produce the expected prefixes; a `table` renders the expected header + a row to a `String` (assert on substrings, color-disabled for determinism).
- **`route` tests:** the existing `build_new_route` JSON tests are unaffected (mapping unchanged). Add a `print_routes`-renders test by extracting a pure `routes_table(routes: &Value) -> String` and asserting it contains the names/targets/`on`/`off` (with color disabled).
- `cargo clippy --workspace --all-targets -D warnings`; `cargo fmt`. Manual smoke: `tt route list` on a TTY (boxed/colored) and piped (`| cat`, plain).

## Design tokens
Bold headings · **cyan** accent (models/highlights) · green `✓ on` · dim `✗ off` · red errors · yellow warnings · **rounded-box** tables (`UTF8_ROUND_CORNERS`).

## Out of Scope (V1b and beyond)
- Rolling the `ui` module out to `login`/`whoami`/`logout`, `init`, `plan`/`inspect`/`audit` output — **V1b**.
- ASCII symbol fallback for non-UTF8 terminals; a `--color=always|never|auto` tri-state (only `--no-color` for now); themeable palette / light-background detection.
- Branded banner on `tt --help` (the "full brand" option was not chosen).
