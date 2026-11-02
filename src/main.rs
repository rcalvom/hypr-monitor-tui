mod backend;
mod model;
mod overlay;
mod profiles;
mod theme;
mod transaction;
mod ui;

use anyhow::{Result, bail};
use std::io::Write;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let guard = args.first().is_some_and(|s| s == "--guard");
    if let Err(error) = run(args) {
        if guard {
            let _ = writeln!(std::io::stdout(), "ERROR {error:#}");
        } else {
            eprintln!("Error: {error:#}");
        }
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("--guard") => transaction::guard(),
        Some("--identify-data") => overlay::show(serde_json::from_str(
            args.get(1)
                .ok_or_else(|| anyhow::anyhow!("Missing overlay data"))?,
        )?),
        Some("--list") => {
            println!("{}", serde_json::to_string_pretty(&backend::query()?)?);
            Ok(())
        }
        Some("--demo") => ui::run(ui::demo(), true, None),
        Some("--profile") => {
            let name = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("Missing profile name"))?;
            ui::run(backend::query()?, false, Some(name))
        }
        Some("--help" | "-h") => {
            println!(
                "hypr-monitor-tui\n\nUsage: hypr-monitor-tui [--list | --demo | --profile NAME]\n\nDefault: interactive editor. Changes require explicit apply and confirmation.\n--list          Read-only JSON (no serial numbers)\n--demo          Synthetic displays; no compositor writes or profile saves\n--profile NAME  Load a saved profile as a draft; does not apply automatically\n\nHyprland 0.57.x is required for runtime apply. See README.md for controls."
            );
            Ok(())
        }
        None => ui::run(backend::query()?, false, None),
        _ => bail!("Unknown argument. Use --help"),
    }
}
