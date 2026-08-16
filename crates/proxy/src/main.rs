//! Daemon and command line.

#![forbid(unsafe_code)]

mod cli;

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use cli::Cli;
use cli::Command;
use cli::RunArgs;
use codex_cc_proxy::config::Config;
use codex_cc_proxy::daemon;
use codex_cc_proxy::ingress::AppState;
use codex_cc_proxy::ingress::ModelMapping;
use codex_cc_proxy::upstream::http::HttpTransport;
use std::sync::Arc;

/// Where the Responses API lives. Not a published or supported endpoint — see
/// `docs/api.md` §7.
const UPSTREAM_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args).await,
        Command::Login => bail!("`login` is not implemented yet"),
        Command::Status => bail!("`status` is not implemented yet"),
        Command::Models => bail!("`models` is not implemented yet"),
        Command::Env(_) => bail!("`env` is not implemented yet"),
        Command::Doctor(_) => bail!("`doctor` is not implemented yet"),
        Command::Record(_) => bail!("`record` is not implemented yet"),
    }
}

async fn run(args: RunArgs) -> Result<()> {
    let config = Config::default();
    let port = args.port.unwrap_or(config.port);

    // Refused before binding: a daemon that starts with an incomplete mapping
    // breaks WebFetch in a way that looks unrelated to tier mapping (§7.1).
    let tiers = config.tiers.resolve()?;

    let listener = daemon::bind(port).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "listening");

    let state = AppState {
        transport: Arc::new(HttpTransport::new(UPSTREAM_ENDPOINT)),
        models: Arc::new(
            tiers
                .into_iter()
                .map(|tier| ModelMapping {
                    requested: tier.tier.to_owned(),
                    upstream: tier.model,
                })
                .collect(),
        ),
    };

    daemon::serve(listener, state).await?;
    Ok(())
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
