mod cli;
mod convert;
mod diff;
mod interrupt;
mod output;
mod proc_match;
mod record;
mod registry;
mod update;
mod view;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

fn main() -> ExitCode {
    match try_main() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn try_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    match cli.command {
        Command::Record(args) => record::run(args, cli.verbose),
        Command::Convert(args) => {
            convert::run(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::View(args) => {
            view::run(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Diff(args) => {
            diff::run(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Update => {
            update::run()?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::EnvFilter;

    let filter = if verbose == 0 {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
    } else {
        let level = match verbose {
            1 => "info",
            2 => "debug",
            _ => "trace",
        };
        EnvFilter::new(level)
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(verbose >= 2)
        .with_writer(std::io::stderr)
        .compact()
        .init();
}
