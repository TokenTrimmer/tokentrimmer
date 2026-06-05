//! Terminal UI: the single place all `tt` output styling flows through.
//! Pure `format_*`/`table` helpers return `String`s (unit-testable); thin
//! printers write them. Color is gated by `console`'s process-global enable
//! state, set once by [`init`].

use std::time::Duration;

use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::{NOTHING, UTF8_FULL};
use comfy_table::{ContentArrangement, Table};
use console::Style;
use indicatif::{ProgressBar, ProgressStyle};

/// Resolve + apply color enablement once, at startup. Colors are decided
/// per-stream: on only when that stream is a TTY, `NO_COLOR` is unset, and
/// `--no-color` was not passed. (stdout and stderr are gated independently so a
/// redirected `2>err.log` never receives ANSI even when stdout is a terminal.)
pub fn init(no_color_flag: bool) {
    let no_color_env = std::env::var_os("NO_COLOR").is_some();
    console::set_colors_enabled(should_enable_color(
        no_color_flag,
        no_color_env,
        console::user_attended(),
    ));
    console::set_colors_enabled_stderr(should_enable_color(
        no_color_flag,
        no_color_env,
        console::user_attended_stderr(),
    ));
}

/// Pure color-enable decision (unit-tested).
#[must_use]
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
    // Errors/warnings print to stderr → gate on the stderr color global.
    Style::new().red().for_stderr()
}
pub fn warn_style() -> Style {
    Style::new().yellow().for_stderr()
}
pub fn muted() -> Style {
    Style::new().dim()
}

// --- symbols ---
pub const OK: &str = "✓";
pub const NO: &str = "✗";
pub const ARROW: &str = "→";
pub const BULLET: &str = "·";

// --- pure formatters ---
#[must_use]
pub fn format_success(msg: &str) -> String {
    format!("{} {}", success_style().apply_to(OK), msg)
}
#[must_use]
pub fn format_error(msg: &str) -> String {
    format!("{} {}", error_style().apply_to(NO), msg)
}
#[must_use]
pub fn format_warn(msg: &str) -> String {
    format!("{} {}", warn_style().apply_to("!"), msg)
}
#[must_use]
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

/// A `comfy-table` builder. With color enabled → rounded UTF-8 borders; when
/// disabled (piped / `NO_COLOR`) → a borderless plain layout, so scripts and
/// `grep` get clean columnar text instead of box-drawing characters.
///
/// `custom_styling` is enabled so embedded ANSI in cells is stripped before
/// width measurement — colored columns stay aligned.
#[must_use]
pub fn table(headers: &[&str], color_enabled: bool) -> Table {
    let mut t = Table::new();
    if color_enabled {
        t.load_preset(UTF8_FULL).apply_modifier(UTF8_ROUND_CORNERS);
    } else {
        t.load_preset(NOTHING);
        t.force_no_tty();
    }
    t.set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(
            headers
                .iter()
                .map(|h| heading_style().apply_to(h).to_string()),
        );
    t
}

/// RAII spinner: clears itself on drop, so an early `?`-return on a failed
/// network call never leaves a dangling spinner line on the terminal.
pub struct Spinner(ProgressBar);

impl Drop for Spinner {
    fn drop(&mut self) {
        self.0.finish_and_clear();
    }
}

/// A spinner for a network call. Hidden when color is off (`--no-color` /
/// `NO_COLOR`) or stderr is not a TTY; cleared on drop.
#[must_use]
pub fn spinner(msg: &str) -> Spinner {
    let pb = ProgressBar::new_spinner();
    if console::colors_enabled_stderr() && console::user_attended_stderr() {
        pb.set_style(
            ProgressStyle::with_template("{spinner} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        pb.set_message(msg.to_string());
        pb.enable_steady_tick(Duration::from_millis(80));
    } else {
        pb.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    }
    Spinner(pb)
}

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
        // Color disabled (both streams) → deterministic plain text. error/warn
        // are stderr-styled, so disable the stderr global too.
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
        assert_eq!(format_success("done"), "✓ done");
        assert_eq!(format_error("nope"), "✗ nope");
        assert_eq!(format_warn("careful"), "! careful");
    }

    #[test]
    fn table_with_borders_renders_header_and_rows() {
        console::set_colors_enabled(false);
        let mut t = table(&["NAME", "STATUS"], true);
        t.add_row(vec!["vis-downgrade", "on"]);
        let out = t.to_string();
        assert!(out.contains("NAME"));
        assert!(out.contains("vis-downgrade"));
        assert!(out.contains('╭'), "rounded top-left corner present: {out}");
    }

    #[test]
    fn table_plain_when_color_disabled() {
        // Piped / NO_COLOR → no box-drawing chars (scripts/grep get clean text).
        let mut t = table(&["NAME"], false);
        t.add_row(vec!["x"]);
        let out = t.to_string();
        assert!(!out.contains('╭'), "no box chars when disabled: {out:?}");
        assert!(
            !out.contains('│'),
            "no vertical bars when disabled: {out:?}"
        );
        assert!(out.contains('x'));
    }

    #[test]
    fn custom_styling_strips_ansi_for_width() {
        // A cell with embedded ANSI (visible width 6) must NOT bloat the column
        // to the escape-byte width — proves the `custom_styling` feature is on.
        console::set_colors_enabled(false);
        let mut t = table(&["M"], true);
        t.add_row(vec!["\u{1b}[36mgpt-4o\u{1b}[0m".to_string()]);
        let out = t.to_string();
        let max_w = out
            .lines()
            .map(console::measure_text_width)
            .max()
            .unwrap_or(0);
        assert!(
            max_w < 14,
            "column over-measured ANSI as width ({max_w}); custom_styling off? {out:?}"
        );
    }
}
