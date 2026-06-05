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
        .set_header(
            headers
                .iter()
                .map(|h| heading_style().apply_to(h).to_string()),
        );
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
        assert!(out.contains('╭'), "rounded top-left corner present: {out}");
    }
}
