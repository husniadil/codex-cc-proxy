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

/// The same surface over a socket.
const UPSTREAM_WEBSOCKET: &str = "wss://chatgpt.com/backend-api/codex/responses";

/// Where the model catalog lives.
const UPSTREAM_CATALOG: &str = "https://chatgpt.com/backend-api/codex/models";

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args).await,
        Command::Login => login().await,
        Command::Status => print_status().await,
        Command::Models => print_models().await,
        Command::Env(args) => print_env(args).await,
        Command::Doctor(args) => doctor(args).await,
        Command::Record(args) => record(args).await,
    }
}

/// Probe the capabilities, and say what the answer rests on.
async fn doctor(args: cli::DoctorArgs) -> Result<()> {
    let fixtures = args
        .fixtures
        .unwrap_or_else(|| std::path::PathBuf::from("fixtures"));

    let outcomes = codex_cc_proxy::doctor::run(&fixtures, args.probe.as_deref()).await?;

    println!(
        "{}",
        codex_cc_proxy::probe::matrix(&outcomes, codex_cc_proxy::doctor::AGAINST_REPLAY)
    );

    let failed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.status, codex_cc_proxy::probe::Status::Failed(_)))
        .count();
    if failed > 0 {
        bail!("{failed} probe(s) failed");
    }
    Ok(())
}

/// Login runs in the CLI rather than through the socket: it needs a browser and
/// a callback port, and the daemon need not be running to authenticate.
async fn login() -> Result<()> {
    let store: Arc<dyn codex_cc_proxy::auth::store::CredentialStore> = Arc::new(
        codex_cc_proxy::auth::store::FileStore::new(credential_path()),
    );

    let credentials = codex_cc_proxy::auth::login::run(store, |url| {
        // Printed as well as opened. An environment with no browser — a remote
        // shell, a container — still needs a way through, and one line of URL
        // is less in the way than an explanation of where to find it.
        println!("Open this URL to authorize:\n\n{url}\n");
        let _ = open_in_browser(url);
    })
    .await?;

    match credentials.account_id {
        Some(account) => println!("Signed in ({account})."),
        None => println!("Signed in."),
    }
    Ok(())
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };

    command
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
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
    let config = Config::load()?;
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

    let tokens = Arc::new(codex_cc_proxy::auth::tokens::TokenSource::new(
        Arc::clone(&credentials),
        codex_cc_proxy::auth::flow::token_endpoint(),
        codex_cc_proxy::auth::flow::CLIENT_ID,
        Arc::new(codex_cc_proxy::auth::tokens::SystemClock),
    ));

    // §7.0 — fetched with credentials when there are any. An unreachable
    // catalog falls back rather than failing: fetch failure is not evidence
    // that a model went away (§7.1), and a daemon that will not start because
    // the network blinked is the worse failure.
    let catalog = Arc::new(match tokens.access_token().await {
        Ok(token) => {
            codex_cc_proxy::catalog::fetch(
                &reqwest::Client::new(),
                UPSTREAM_CATALOG,
                &token,
                tokens.account_id().as_deref(),
            )
            .await
        }
        Err(error) => {
            tracing::info!(%error, "not authenticated; the model list is the fallback");
            codex_cc_proxy::catalog::Catalog::fallback()
        }
    });

    // An unknown model id is refused here, and the error names what the catalog
    // does have — which is the fastest way to find the id you actually meant.
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

    // Credentials are fetched per request rather than captured here, so a
    // refresh part-way through a session is transparent to both transports.
    // One conduit per conversation, built on its first turn. The binding is per
    // session because latching, the pooled connection, and the previous
    // response id all belong to one conversation.
    let websocket_enabled = config.transport.websocket;
    let compression = config.transport.compression;
    let conduit_tokens = Arc::clone(&tokens);
    let conduits: codex_cc_proxy::ingress::ConduitFactory = Arc::new(move || {
        let http = Arc::new(
            HttpTransport::new(UPSTREAM_ENDPOINT).with_credentials(Arc::clone(&conduit_tokens)),
        );
        let websocket = websocket_enabled.then(|| {
            Arc::new(
                codex_cc_proxy::upstream::websocket::WebSocketTransport::new(UPSTREAM_WEBSOCKET)
                    .with_credentials(Arc::clone(&conduit_tokens))
                    .with_compression(compression),
            )
        });
        Arc::new(codex_cc_proxy::upstream::conduit::Conduit::new(
            http, websocket,
        ))
    });

    let state = AppState {
        effort_ceiling: None,
        catalog: Arc::new(codex_cc_proxy::catalog::Catalog::fallback()),
        // Only reached if the factory is absent, which it is not here.
        transport: Arc::new(
            HttpTransport::new(UPSTREAM_ENDPOINT).with_credentials(Arc::clone(&tokens)),
        ),
        conduits: Some(conduits),
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
        sessions: Arc::new(codex_cc_proxy::session::SessionStore::new()),
    };

    daemon::serve(listener, state).await?;
    Ok(())
}

/// Where credentials live. Never the configuration file (§8).
///
/// The same directory the configuration is read from, resolved by the same
/// function: two copies of this rule drift, and the copy that drifts sends the
/// daemon looking for credentials somewhere the login never wrote them.
fn credential_path() -> std::path::PathBuf {
    codex_cc_proxy::config::config_dir().join("credentials.json")
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
