//! `browser` — entry point. Parses a tiny argument set, loads config, and hands
//! off to the TUI.
//!
//! Usage:
//!   browser [--config PATH] [TARGET...]
//!
//! TARGET is a URL, a `:`-command (e.g. `:s rust crate`), or free words that get
//! routed to search. With no TARGET, a welcome screen is shown.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use browser_core::Config;

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("browser: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    if args.help {
        print_help();
        return Ok(());
    }
    let config = Config::load(args.config.as_deref()).context("loading config")?;

    if args.dump {
        let target = args.target.context("--dump requires a target")?;
        let out = browser_tui::dump(config, target.trim(), 100).await?;
        print!("{out}");
        return Ok(());
    }

    browser_tui::run(config, args.target).await
}

struct Args {
    config: Option<PathBuf>,
    target: Option<String>,
    help: bool,
    dump: bool,
}

impl Args {
    fn parse(argv: impl Iterator<Item = String>) -> Result<Args> {
        let mut config = None;
        let mut help = false;
        let mut dump = false;
        let mut rest: Vec<String> = Vec::new();
        let mut argv = argv.peekable();

        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--help" | "-h" => help = true,
                "--dump" => dump = true,
                "--config" | "-c" => {
                    let path = argv.next().context("--config requires a path argument")?;
                    config = Some(PathBuf::from(path));
                }
                // First non-flag argument: everything from here is the target.
                _ => {
                    rest.push(arg);
                    rest.extend(argv.by_ref());
                }
            }
        }

        let target = if rest.is_empty() { None } else { Some(rest.join(" ")) };
        Ok(Args { config, target, help, dump })
    }
}

fn print_help() {
    println!(
        "browser — a modal terminal browser\n\n\
         USAGE:\n    browser [--config PATH] [TARGET...]\n\n\
         TARGET:\n    a URL, a :command (e.g. :s rust crate), or words to search\n\n\
         OPTIONS:\n    -c, --config PATH   use an explicit config file\n    -h, --help          show this help"
    );
}
