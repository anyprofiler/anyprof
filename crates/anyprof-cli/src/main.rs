mod cli;
mod convert;
mod interrupt;
mod output;
mod record;
mod registry;
mod update;
mod view;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

fn main() {
    if let Err(err) = try_main() {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    match cli.command {
        Command::Record(args) => record::run(args),
        Command::Convert(args) => convert::run(args),
        Command::View(args) => view::run(args),
        Command::Update => update::run(),
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
