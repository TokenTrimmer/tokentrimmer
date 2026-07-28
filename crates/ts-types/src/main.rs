use anyhow::{bail, Result};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "check".into());
    if args.next().is_some() {
        bail!("usage: cargo run -p tt-ts-types -- [check|write]");
    }
    let root = tt_ts_types::repository_root();
    match command.as_str() {
        "check" => tt_ts_types::check_artifacts(&root),
        "write" => tt_ts_types::write_artifacts(&root),
        _ => bail!("unknown command {command:?}; expected check or write"),
    }
}
