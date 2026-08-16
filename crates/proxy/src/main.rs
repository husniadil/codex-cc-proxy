//! Daemon and command line.

#![forbid(unsafe_code)]

mod cli;

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use cli::Cli;
use cli::Command;

fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Run(_) => bail!("`run` is not implemented yet"),
        Command::Login => bail!("`login` is not implemented yet"),
        Command::Status => bail!("`status` is not implemented yet"),
        Command::Models => bail!("`models` is not implemented yet"),
        Command::Env(_) => bail!("`env` is not implemented yet"),
        Command::Doctor(_) => bail!("`doctor` is not implemented yet"),
        Command::Record(_) => bail!("`record` is not implemented yet"),
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
