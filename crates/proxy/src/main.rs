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
use codex_cc_proxy::control;
use codex_cc_proxy::daemon;
use codex_cc_proxy::ingress::AppState;
use codex_cc_proxy::ingress::ModelMapping;
use codex_cc_proxy::render;
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
        Command::Status => print_status().await,
        Command::Models => print_models().await,
        Command::Env(args) => print_env(args).await,
        Command::Doctor(_) => bail!("`doctor` is not implemented yet"),
        Command::Record(args) => record(args).await,
    }
}

/// Every verb but `run` works through the control socket. The CLI holds no
/// state of its own, so a second front-end needs no new daemon work.
async fn print_status() -> Result<()> {
    let result = control::call(&control::default_path(), "status", None).await?;
    println!("{}", render::status(&result));
    Ok(())
}

async fn print_models() -> Result<()> {
    let result = control::call(&control::default_path(), "models", None).await?;
    println!("{}", render::models(&result));
    Ok(())
}

async fn print_env(args: cli::EnvArgs) -> Result<()> {
    let result = control::call(&control::default_path(), "env", None).await?;
    println!(
        "{}",
        if args.json {
            render::env_json(&result)
        } else {
            render::env_shell(&result)
        }
    );
    Ok(())
}

/// Capture exchanges as fixtures.
///
/// Ingress capture runs the daemon normally and records what the client sends
/// before translation. It needs no credentials, because nothing upstream is
/// involved at the point of capture.
async fn record(args: cli::RecordArgs) -> Result<()> {
    match args.mode {
        cli::RecordMode::Ingress => run_with(RunArgs { port: None }, true).await,
        cli::RecordMode::Upstream => {
            bail!("`record upstream` needs credentials and is not implemented yet")
        }
    }
}

async fn run(args: RunArgs) -> Result<()> {
    run_with(args, false).await
}

async fn run_with(args: RunArgs, record_ingress: bool) -> Result<()> {
    let config = Config::default();
    let port = args.port.unwrap_or(config.port);

    // Refused before binding: a daemon that starts with an incomplete mapping
    // breaks WebFetch in a way that looks unrelated to tier mapping (§7.1).
    let tiers = config.tiers.resolve()?;

    let listener = daemon::bind(port).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "listening");

    let credentials: Arc<dyn codex_cc_proxy::auth::store::CredentialStore> = Arc::new(
        codex_cc_proxy::auth::store::FileStore::new(credential_path()),
    );
    let recording = Arc::new(std::sync::atomic::AtomicBool::new(record_ingress));

    // The catalog is fetched with credentials when there are any. An
    // unreachable catalog falls back rather than failing: fetch failure is not
    // evidence that a model went away (§7.1).
    let catalog = Arc::new(codex_cc_proxy::catalog::Catalog::fallback());
    catalog.validate(
        &tiers
            .iter()
            .map(|tier| tier.model.clone())
            .collect::<Vec<_>>(),
    )?;

    let control_state = codex_cc_proxy::control::handler::ControlState {
        port: addr.port(),
        tiers: Arc::new(tiers.clone()),
        catalog: Arc::clone(&catalog),
        credentials: Arc::clone(&credentials),
        recording: Arc::clone(&recording),
    };
    let socket_path = control::default_path();
    tokio::spawn(async move {
        if let Err(error) = control::serve(&socket_path, control_state).await {
            tracing::warn!(%error, "the control socket stopped");
        }
    });

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
        // §5.4 — empty streams are recorded whether or not capture was asked
        // for, because an empty stream is always a defect and is otherwise
        // invisible.
        recorder: Some(codex_cc_proxy::recorder::Recorder::new(
            std::env::temp_dir().join("codex-cc-proxy-captures"),
        )),
        record_ingress,
    };

    daemon::serve(listener, state).await?;
    Ok(())
}

/// Where credentials live. Never the configuration file (§8).
fn credential_path() -> std::path::PathBuf {
    std::env::var_os("CODEX_CC_PROXY_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dirs_config_dir().join("codex-cc-proxy"))
        .join("credentials.json")
}

fn dirs_config_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
        .unwrap_or_else(std::env::temp_dir)
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
