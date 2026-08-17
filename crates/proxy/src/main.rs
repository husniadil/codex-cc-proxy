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
        Command::Usage(args) => print_usage(args).await,
        Command::Statusline(args) => statusline(args).await,
        Command::Record(args) => record(args).await,
    }
}

/// The transport a live probe run answers through: HTTP, with the stored
/// credentials.
///
/// HTTP rather than the conduit. A probe is one turn with no continuation, so
/// nothing here exercises the incremental path, and the WebSocket transport's
/// value is entirely in that path. The simpler transport is also the one whose
/// failures are legible when a probe does fail.
fn live_transport() -> Result<Arc<dyn codex_cc_proxy::upstream::Transport>> {
    let credentials: Arc<dyn codex_cc_proxy::auth::store::CredentialStore> = Arc::new(
        codex_cc_proxy::auth::store::FileStore::new(credential_path()),
    );
    let tokens = Arc::new(codex_cc_proxy::auth::tokens::TokenSource::new(
        credentials,
        codex_cc_proxy::auth::flow::token_endpoint(),
        codex_cc_proxy::auth::flow::CLIENT_ID,
        Arc::new(codex_cc_proxy::auth::tokens::SystemClock),
    ));

    Ok(Arc::new(
        HttpTransport::new(UPSTREAM_ENDPOINT).with_credentials(tokens),
    ))
}

/// The mapping a live probe run uses: the operator's own.
///
/// The corpus asks for tier ids, and mapping them through the configuration is
/// the point — `web-fetch` asks on the haiku id specifically, so a live run
/// answers whether the tier the client's secondary conversations land on is
/// mapped to something that works.
fn live_models() -> Result<Arc<Vec<ModelMapping>>> {
    let tiers = Config::load()?.tiers.resolve()?;
    let by_tier = |name: &str| {
        tiers
            .iter()
            .find(|tier| tier.tier == name)
            .map(|tier| tier.model.clone())
    };

    let mut models: Vec<ModelMapping> = tiers
        .iter()
        .map(|tier| ModelMapping {
            requested: tier.tier.to_owned(),
            upstream: tier.model.clone(),
        })
        .collect();

    // The corpus names concrete model ids rather than tier words, because that
    // is what the client sends.
    for (requested, tier) in [
        ("claude-sonnet-4", "sonnet"),
        ("claude-3-5-haiku-20241022", "haiku"),
    ] {
        if let Some(upstream) = by_tier(tier) {
            models.push(ModelMapping {
                requested: requested.to_owned(),
                upstream,
            });
        }
    }

    Ok(Arc::new(models))
}

/// Probe the capabilities, and say what the answer rests on.
async fn doctor(args: cli::DoctorArgs) -> Result<()> {
    let fixtures = args
        .fixtures
        .unwrap_or_else(|| std::path::PathBuf::from("fixtures"));

    let (outcomes, against) = if args.live {
        (
            codex_cc_proxy::doctor::run_live(
                &fixtures,
                args.probe.as_deref(),
                live_transport()?,
                live_models()?,
                Config::load()?.effort_ceiling()?,
            )
            .await?,
            codex_cc_proxy::doctor::AGAINST_LIVE,
        )
    } else {
        (
            codex_cc_proxy::doctor::run(&fixtures, args.probe.as_deref()).await?,
            codex_cc_proxy::doctor::AGAINST_REPLAY,
        )
    };

    println!("{}", codex_cc_proxy::probe::matrix(&outcomes, against));

    let failed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.status, codex_cc_proxy::probe::Status::Failed(_)))
        .count();
    if failed > 0 {
        bail!("{failed} probe(s) failed");
    }
    Ok(())
}

/// Login runs in the CLI rather than through the socket: it needs a callback
/// port, and the daemon need not be running to authenticate.
///
/// **The URL is printed, never opened.** Opening it hands the authorization to
/// whichever account the default browser is already signed into, which is a
/// choice this command has no basis for making — the grant it produces is the
/// one every later request spends. Printing it leaves the choice where it
/// belongs, and costs one paste.
async fn login() -> Result<()> {
    let store: Arc<dyn codex_cc_proxy::auth::store::CredentialStore> = Arc::new(
        codex_cc_proxy::auth::store::FileStore::new(credential_path()),
    );

    let credentials = codex_cc_proxy::auth::login::run(store, |url| {
        println!(
            "Open this URL to authorize:\n\n{url}\n\n\
             It authorizes whichever ChatGPT account that browser is signed into. \
             Sign in as the account you want first, or use a private window to \
             pick a different one.\n"
        );
    })
    .await?;

    match credentials.account_id {
        Some(account) => println!("Signed in ({account})."),
        None => println!("Signed in."),
    }
    Ok(())
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

/// What quota is left. Reported from the snapshot the backend volunteers on
/// each turn, so it costs nothing to ask and is as of the last turn made.
async fn print_usage(args: cli::UsageArgs) -> Result<()> {
    let result = control::call(&control::default_path(), "usage", None).await?;
    println!(
        "{}",
        if args.json {
            serde_json::to_string_pretty(&result)?
        } else {
            render::usage(&result)
        }
    );
    Ok(())
}

/// Wrap a status-line script, adding what the client cannot supply.
///
/// The client hands a status line a JSON payload with no quota in it, because
/// it tracks a subscription quota for its own account and a proxy is not one.
/// This reads that payload, merges in what the backend reported, and hands it to
/// the user's own command — which keeps working exactly as written.
///
/// **It never fails the status line.** A daemon that is not running, a socket
/// that does not answer, a snapshot that cannot be read: every one of those
/// passes the payload through unchanged. A status line renders constantly, and
/// one that breaks is worse than one missing a figure.
async fn statusline(args: cli::StatuslineArgs) -> Result<()> {
    use std::io::Read;
    use std::io::Write;

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    let payload: serde_json::Value =
        serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
    let usage = control::call(&control::default_path(), "usage", None)
        .await
        .unwrap_or(serde_json::Value::Null);
    let merged = codex_cc_proxy::statusline::merge(payload, &usage);

    // Falls back to what arrived: if the payload would not parse, merging
    // produced null, and handing a script `null` is worse than handing it the
    // bytes it was going to get anyway.
    let body = if merged.is_null() {
        input
    } else {
        serde_json::to_string(&merged)?
    };

    let Some((program, arguments)) = args.command.split_first() else {
        println!("{body}");
        return Ok(());
    };

    let mut child = std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body.as_bytes())?;
    }
    let status = child.wait()?;

    // The child's status is this command's: a wrapper that swallows a failure
    // makes the thing it wraps undebuggable.
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
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
        cli::RecordMode::Ingress => run_with(RunArgs { port: None }, Capture::Ingress).await,
        cli::RecordMode::Upstream => {
            // Says so before it starts, because the cost is the difference
            // between the two modes and it is not recoverable afterwards.
            tracing::warn!(
                "recording upstream: every turn through this daemon spends quota \
                 and is written to disk with its content"
            );
            run_with(RunArgs { port: None }, Capture::Upstream).await
        }
    }
}

/// What a run captures, beyond the empty streams §5.4 always records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    /// Nothing extra.
    Nothing,
    /// What the client sent, before translation. Free.
    Ingress,
    /// The exchange with the backend. Spends quota, and holds both halves of
    /// what a fixture needs.
    Upstream,
}

async fn run(args: RunArgs) -> Result<()> {
    run_with(args, Capture::Nothing).await
}

async fn run_with(args: RunArgs, capture: Capture) -> Result<()> {
    let usage = Arc::new(codex_cc_proxy::usage::UsageStore::default());
    let switches = Arc::new(codex_cc_proxy::recorder::Switches::new(match capture {
        Capture::Nothing => None,
        Capture::Ingress => Some(codex_cc_proxy::recorder::Mode::Ingress),
        Capture::Upstream => Some(codex_cc_proxy::recorder::Mode::Upstream),
    }));
    let config = Config::load()?;
    let port = args.port.unwrap_or(config.port);

    // Refused before binding: a daemon that starts with an incomplete mapping
    // breaks WebFetch in a way that looks unrelated to tier mapping (§7.1).
    let tiers = config.tiers.resolve()?;

    // Also refused before binding, so a mistyped ceiling is caught at startup
    // rather than silently spending at full rate.
    let effort_ceiling = config.effort_ceiling()?;
    match effort_ceiling {
        Some(effort) => tracing::info!(?effort, "reasoning effort is capped"),
        None => tracing::info!("reasoning effort is whatever the client asks for"),
    }

    let listener = daemon::bind(port).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "listening");

    let credentials: Arc<dyn codex_cc_proxy::auth::store::CredentialStore> = Arc::new(
        codex_cc_proxy::auth::store::FileStore::new(credential_path()),
    );

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
        capture: Arc::clone(&switches),
        usage: Arc::clone(&usage),
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
            HttpTransport::new(UPSTREAM_ENDPOINT)
                .with_credentials(Arc::clone(&conduit_tokens))
                .with_compression(compression),
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
        effort_ceiling,
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
            codex_cc_proxy::recorder::Recorder::default_directory(),
        )),
        capture: Arc::clone(&switches),
        usage: Arc::clone(&usage),
        instructions: Arc::new(config.instructions.clone()),
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
