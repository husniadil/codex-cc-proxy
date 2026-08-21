//! Daemon and command line.

#![forbid(unsafe_code)]

mod cli;

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use cli::Cli;
use cli::Command;
use cli::RunArgs;
use proxenos::config::Config;
use proxenos::control;
use proxenos::daemon;
use proxenos::ingress::AppState;
use proxenos::ingress::ModelMapping;
use proxenos::render;
use proxenos::upstream::http::HttpTransport;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    // Before anything resolves a path: a store under the pre-rename name must
    // refuse loudly, not be silently shadowed by a fresh empty one.
    if let Some(refusal) = proxenos::config::renamed_home_refusal() {
        anyhow::bail!("{refusal}");
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args).await,
        Command::Login(args) => login(args).await,
        Command::Accounts(args) => accounts(args).await,
        Command::Status => print_status().await,
        Command::Models => print_models().await,
        Command::Env(args) => print_env(args).await,
        Command::Settings => print_settings().await,
        Command::Stop => stop().await,
        Command::Exec(args) => exec(args).await,
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
async fn live_transport() -> Result<Arc<dyn proxenos::upstream::Transport>> {
    let credentials: Arc<dyn proxenos::auth::store::AccountStore> =
        Arc::new(proxenos::auth::store::FileStore::new(credential_path()));
    let tokens = Arc::new(proxenos::auth::tokens::TokenSource::new(
        Arc::clone(&credentials) as Arc<dyn proxenos::auth::store::CredentialStore>,
        proxenos::auth::flow::token_endpoint(),
        proxenos::auth::flow::CLIENT_ID,
        Arc::new(proxenos::auth::tokens::SystemClock),
    ));
    let authorizer: Arc<dyn proxenos::auth::authorize::Authorizer> =
        Arc::new(proxenos::auth::authorize::AccountAuthorizer::new(
            Arc::clone(&credentials),
            Arc::clone(&tokens),
        ));

    // Resolved before anything is probed. A run that cannot authenticate used
    // to answer with the whole matrix, every row failed and the header saying
    // the backend answered and was billed — when nothing had been sent at all.
    // A capability reported as broken because the credential is missing sends
    // whoever reads it somewhere else entirely.
    let authorization = authorizer.authorize().await?;

    // The operator's endpoint, not a compiled-in one: a probe run that contacts
    // somewhere other than the daemon would answer about the wrong backend. Of
    // the kind the account holds, because the two are not interchangeable
    // (`proxy-behavior.md` §8.2).
    let config = Config::load()?;
    let endpoint = match authorization.kind {
        proxenos::auth::authorize::Kind::Key => config.upstream.key.endpoint,
        proxenos::auth::authorize::Kind::Subscription => config.upstream.endpoint,
    };

    Ok(Arc::new(
        HttpTransport::new(endpoint)
            .for_endpoint(authorization.kind)
            .with_credentials(authorizer),
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
        ("claude-sonnet-5", "sonnet"),
        ("claude-haiku-4-5-20251001", "haiku"),
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
    let fixtures = proxenos::doctor::Corpus::resolve(args.fixtures);

    let (outcomes, against) = if args.live {
        (
            proxenos::doctor::run_live(
                &fixtures,
                args.probe.as_deref(),
                live_transport().await?,
                live_models()?,
                Config::load()?.effort_ceiling()?,
            )
            .await?,
            proxenos::doctor::AGAINST_LIVE.to_owned(),
        )
    } else {
        (
            proxenos::doctor::run(&fixtures, args.probe.as_deref()).await?,
            // Which corpus answered is part of what the run establishes: a
            // directory can hold a recording made minutes ago, the embedded
            // copy is whatever this binary was built from.
            format!(
                "{} — {}",
                proxenos::doctor::AGAINST_REPLAY,
                fixtures.describe()
            ),
        )
    };

    println!("{}", proxenos::probe::matrix(&outcomes, &against));

    let failed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.status, proxenos::probe::Status::Failed(_)))
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
async fn login(args: cli::LoginArgs) -> Result<()> {
    let store: Arc<dyn proxenos::auth::store::AccountStore> =
        Arc::new(proxenos::auth::store::FileStore::new(credential_path()));

    if args.key {
        return store_key(&store, args.label.as_deref()).await;
    }

    let credentials =
        proxenos::auth::login::run(Arc::clone(&store), args.label.as_deref(), |url| {
            println!(
                "Open this URL to authorize:\n\n{url}\n\n\
             It authorizes whichever ChatGPT account that browser is signed into. \
             Sign in as the account you want first, or use a private window to \
             pick a different one.\n"
            );
        })
        .await?;

    // The name the store filed it under, which is the string `accounts --use`
    // takes. The account id is not always that name — a label supersedes it,
    // and a grant carrying no id is named by the store.
    let named = store
        .accounts()
        .ok()
        .and_then(|accounts| accounts.into_iter().find(|account| account.selected))
        .map(|account| account.name)
        .or(credentials.account_id);
    match &named {
        Some(account) => println!("Signed in ({account})."),
        None => println!("Signed in."),
    }
    if let Some(account) = named {
        hand_over_to(&account).await;
    }
    Ok(())
}

/// Store a key, read from stdin.
///
/// **Never from an argument.** A command line is visible to every process on
/// the machine and lands in the shell's history file; §8 keeps credentials out
/// of argv, and this is the path that would break that rule if it took one.
///
/// The name is required: a key carries no account id to be named by, and the
/// name is what selects it afterwards.
async fn store_key(
    store: &Arc<dyn proxenos::auth::store::AccountStore>,
    label: Option<&str>,
) -> Result<()> {
    let Some(name) = label else {
        anyhow::bail!(
            "name the account with `--as NAME`: a key carries no id to be named by, \
             and the name is what `accounts --use` takes"
        );
    };

    let mut key = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut key)
        .map_err(|error| anyhow::anyhow!("could not read the key from stdin: {error}"))?;
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("no key on stdin; pipe it in, or paste it and end with ctrl-d");
    }

    store.add_key(name, key)?;
    println!("Stored a key as {name}. It serves turns from now on.");
    hand_over_to(name).await;
    Ok(())
}

/// Tell a running daemon that this account is the one serving turns now.
///
/// The CLI writes the credential file directly, and the daemon reads it on
/// every request — so the account moves either way. What does not move without
/// this is everything a switch carries with it (`api.md` §3): the
/// conversations already bound to the previous account, its quota, and its
/// model list. A live conversation keeps the endpoint it dialed, and after a
/// change of kind that endpoint refuses every turn it is given.
///
/// Best effort by design. No daemon running is the ordinary case for a login,
/// and it is not a failure of one.
async fn hand_over_to(name: &str) {
    let path = control::default_path();
    let told = control::call(
        &path,
        "accounts.select",
        Some(serde_json::json!({ "account": name })),
    )
    .await;
    match told {
        Ok(_) => println!("The running daemon now serves turns as {name}."),
        // A refusal is worth saying. A switch can now be refused for a reason
        // the operator can fix — a mapping this account cannot serve — and
        // staying quiet would leave a daemon serving the previous account with
        // nothing said about why. Only where there is a daemon to have refused:
        // no socket is no daemon, which is an ordinary state after a login and
        // not something to report as a failure.
        Err(error) if path.exists() => {
            eprintln!("The running daemon is still serving another account: {error}");
        }
        Err(_) => {}
    }
}

/// Stored accounts, and which one serves turns.
///
/// Through the socket, because the daemon is what holds the selection: a CLI
/// that wrote the file directly would leave a running daemon serving the
/// account it read at startup.
async fn accounts(args: cli::AccountsArgs) -> Result<()> {
    if let Some(names) = args.rename {
        let [from, to] = names.as_slice() else {
            anyhow::bail!("`--rename` takes the old name and the new one");
        };
        let result = control::call(
            &control::default_path(),
            "accounts.rename",
            Some(serde_json::json!({ "account": from, "name": to })),
        )
        .await?;
        println!("{}", render::renamed_account(&result));
        return Ok(());
    }

    if let Some(name) = args.forget {
        let result = control::call(
            &control::default_path(),
            "accounts.forget",
            Some(serde_json::json!({ "account": name })),
        )
        .await?;
        println!("{}", render::forgotten_account(&result));
        return Ok(());
    }

    if let Some(name) = args.select {
        let result = control::call(
            &control::default_path(),
            "accounts.select",
            Some(serde_json::json!({ "account": name })),
        )
        .await?;
        println!("{}", render::selected_account(&result));
        return Ok(());
    }

    let result = control::call(&control::default_path(), "accounts", None).await?;
    println!("{}", render::accounts(&result));
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
    let merged = proxenos::statusline::merge(payload, &usage);

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
    // The JSON form is the `settings` document, so it goes through `settings`
    // rather than alongside it. Rendering it here as well left one of the two
    // names unguarded, and it was the older name — the one a caller reaches for
    // out of habit — that printed a document quietly missing a permission rule.
    if args.json {
        return print_settings().await;
    }

    let result = control::call(&control::default_path(), "env", None).await?;
    println!("{}", render::env_shell(&result));
    Ok(())
}

/// How long to watch for the daemon to go, and then for anything to bring it
/// back.
///
/// The second is the longer of the two on purpose: launchd throttles a respawn
/// to ten seconds after the last start, and a window under that would report
/// "nothing started it again" moments before something did — sending the reader
/// to `run` straight into the port the supervisor is about to take. It returns
/// as soon as it sees the answer, so the wait is only paid when there is
/// genuinely nothing there.
const STOP_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);
const RESTART_WINDOW: std::time::Duration = std::time::Duration::from_secs(12);

/// What is answering the socket right now.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Answering {
    version: String,
    instance: Option<String>,
}

/// Ask the running daemon to stop, then say what actually happened.
///
/// The observation is the useful half. Under a supervisor a stop is how a
/// running daemon is replaced by the build on disk, which is the answer to "the
/// binary is new and nothing changed". Whether anything restarts it is not this
/// verb's doing, so it reports what it saw rather than claiming to have done it.
///
/// **It watches the instance, not the silence.** A socket falling quiet is a
/// statement about timing: a supervisor quick enough leaves no gap to see, and
/// one that throttles leaves a gap longer than any sensible wait. The daemon
/// mints an id at startup, so a different id is a different process no matter
/// how the two overlapped.
async fn stop() -> Result<()> {
    let before = answering().await;

    let result = match control::call(&control::default_path(), "shutdown", None).await {
        Ok(result) => result,
        // The chicken and the egg, stated rather than papered over. This verb
        // exists to replace a daemon older than the binary asking, and it
        // cannot replace one older than the verb: that daemon has no method to
        // ask. Nothing here can fix it, so it says which situation this is.
        Err(error) if error.status == axum::http::StatusCode::NOT_FOUND => {
            anyhow::bail!(
                "The running daemon is from an older build and has no `stop`. End that \
                 process however it was started, then `proxenos run`."
            );
        }
        Err(error) => return Err(error.into()),
    };
    let was = version_of(&result).unwrap_or_else(|| "the running daemon".to_owned());

    // Gone, or already replaced. An id that changed says the second even when
    // there was never a moment where nothing answered.
    let mut seen = None;
    let went = watch(STOP_WINDOW, |now| {
        seen = now.clone();
        now.is_none() || (now.is_some() && now != before)
    })
    .await;

    if !went {
        println!("asked {was} to stop; it is still answering after {STOP_WINDOW:?}");
        return Ok(());
    }

    if let Some(replacement) = seen {
        report_replacement(&was, &replacement);
        return Ok(());
    }

    // It went and nothing has taken its place yet. Wait long enough to outlast
    // a throttled respawn before saying nothing did.
    let mut back = None;
    watch(RESTART_WINDOW, |now| {
        back = now;
        back.is_some()
    })
    .await;

    match back {
        Some(replacement) => report_replacement(&was, &replacement),
        None => println!("stopped {was}; nothing started it again within {RESTART_WINDOW:?}"),
    }
    Ok(())
}

fn report_replacement(was: &str, now: &Answering) {
    if now.version == was {
        println!("stopped {was}; something started it again, on the same build");
    } else {
        println!(
            "stopped {was}; something started it again as {}",
            now.version
        );
    }
}

/// Poll until `settled` accepts what it sees, or the window closes.
async fn watch(
    window: std::time::Duration,
    mut settled: impl FnMut(Option<Answering>) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if settled(answering().await) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
    }
}

async fn answering() -> Option<Answering> {
    let result = control::call(&control::default_path(), "status", None)
        .await
        .ok()?;
    Some(Answering {
        version: version_of(&result).unwrap_or_else(|| "a build that does not say".to_owned()),
        instance: result
            .get("instance")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

fn version_of(result: &serde_json::Value) -> Option<String> {
    result
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Start a program with this proxy's configuration applied.
///
/// The environment half is set on the child, which is the launcher's whole job.
/// The policy half has no environment variable (`proxy-behavior.md` §7.3), so it
/// rides on the client's own settings flag and nothing is written to disk: no
/// file to go stale, and none to clean up. The document holds no secret — the
/// auth token's value is ignored by design — so argv is a fine place for it.
///
/// **It refuses before starting anything when the daemon is not answering.**
/// Launching regardless would hand the operator a connection refused from a
/// client that cannot explain it.
async fn exec(args: cli::ExecArgs) -> Result<()> {
    let result = control::call(&control::default_path(), "env", None)
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "the daemon is not answering ({error}), so there is no configuration to start \
                 this with. Start it with `proxenos run`."
            )
        })?;

    // Same stop as `settings`, and for a sharper reason: a launch that quietly
    // omits the policy produces a working session with a rule missing from it,
    // and nothing about that session would ever say so.
    control::require_client_policy(&result)?;

    // Compact rather than pretty: this goes on a command line, and a document
    // full of newlines is one that a person reading `ps` has to reassemble.
    let policy = result
        .get("settings")
        .filter(|policy| !policy.is_null())
        .map(serde_json::to_string)
        .transpose()?;

    let plan = proxenos::launch::plan(&args.command, policy.as_deref())?;

    // By design, and said out loud: to the person whose deny rule just
    // vanished, a silent by-design is indistinguishable from a bug.
    if policy.is_some() && !plan.carries_policy {
        eprintln!(
            "note: the client policy rides `--settings`, which `{}` does not take. \
             This launch carries the environment only.",
            plan.program
        );
    }

    let mut child = std::process::Command::new(&plan.program);
    child.args(&plan.arguments);
    for (name, value) in render::variables(&result) {
        child.env(name, value);
    }

    // A real exec on Unix, so signals, job control, the terminal, and the exit
    // status all pass through untouched. A wrapper sitting in the middle would
    // orphan the child on a Ctrl-C.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only returns if the exec itself failed.
        let error = child.exec();
        Err(anyhow::Error::new(error).context(format!("could not start `{}`", plan.program)))
    }
    #[cfg(not(unix))]
    {
        let status = child.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// One complete client settings document, for a file or a launcher.
///
/// The same bytes `env --json` prints. Two names, one document: a caller that
/// reaches for the obvious one must not get the half that leaves the policy out.
async fn print_settings() -> Result<()> {
    let result = control::call(&control::default_path(), "env", None).await?;
    // A document silently missing a permission rule looks complete and behaves
    // otherwise, so this stops rather than prints.
    control::require_client_policy(&result)?;
    println!("{}", render::settings_json(&result));
    Ok(())
}

/// Capture exchanges as fixtures.
///
/// Ingress capture runs the daemon normally and records what the client sends
/// before translation. It needs no credentials, because nothing upstream is
/// involved at the point of capture.
async fn record(args: cli::RecordArgs) -> Result<()> {
    match args.mode {
        cli::RecordMode::Ingress => {
            run_with(
                RunArgs {
                    port: None,
                    detach: false,
                },
                Capture::Ingress,
            )
            .await
        }
        cli::RecordMode::Upstream => {
            // Says so before it starts, because the cost is the difference
            // between the two modes and it is not recoverable afterwards.
            tracing::warn!(
                "recording upstream: every turn through this daemon spends quota \
                 and is written to disk with its content"
            );
            run_with(
                RunArgs {
                    port: None,
                    detach: false,
                },
                Capture::Upstream,
            )
            .await
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
    if args.detach {
        return detach(&args).await;
    }
    run_with(args, Capture::Nothing).await
}

/// Start the daemon as its own process and return once it answers.
///
/// The child is a plain `run` of this same binary in its own process group,
/// its output appended to a log file, because a detached process's terminal is
/// gone the moment this command returns. Success is observed, not assumed:
/// this returns 0 only once the daemon answers the control socket, and a child
/// that dies first has its log quoted rather than summarized.
async fn detach(args: &RunArgs) -> Result<()> {
    use anyhow::Context;

    let socket = control::default_path();
    if control::call(&socket, "status", None).await.is_ok() {
        bail!(
            "a daemon is already answering on the control socket. Stop it with \
             `proxenos stop` first."
        );
    }

    let log_path = proxenos::config::config_dir().join("daemon.log");
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("could not open the log at {}", log_path.display()))?;
    let stderr_log = log.try_clone().context("could not clone the log handle")?;

    // Where this start's writes begin. The log is appended across starts, so a
    // failure must quote only what this child wrote — the tail of an earlier
    // run presented as the reason this one died would be a lie with evidence.
    let baseline = log.metadata().map(|meta| meta.len()).unwrap_or(0);

    let own = std::env::current_exe().context("could not find this binary's own path")?;
    let mut command = std::process::Command::new(own);
    command.arg("run");
    if let Some(port) = args.port {
        command.args(["--port", &port.to_string()]);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(stderr_log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console window, and
        // the terminal's Ctrl-C no longer reaches it.
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let mut child = command.spawn().context("could not start the daemon")?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        // Exit first: a child that died must be reported from its log, and
        // checking the socket first would race an unrelated answerer.
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "the daemon exited before it started answering ({status}). Its log ends with:\n{}",
                log_tail(&log_path, baseline)
            );
        }
        if control::call(&socket, "status", None).await.is_ok() {
            println!(
                "daemon running (pid {}), logging to {}\nstop it with `proxenos stop`",
                child.id(),
                log_path.display()
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            // Ended rather than left behind: exiting nonzero while the daemon
            // quietly finishes coming up would leave the report and the
            // machine disagreeing about whether anything is running.
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the daemon did not start answering within 10s and was ended. Its log ends with:\n{}",
                log_tail(&log_path, baseline)
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The last few lines the daemon wrote after `since`, for an error message
/// that shows the failure instead of describing it.
fn log_tail(path: &std::path::Path, since: u64) -> String {
    let content = std::fs::read(path).unwrap_or_default();
    let start = usize::try_from(since)
        .unwrap_or(usize::MAX)
        .min(content.len());
    let text = String::from_utf8_lossy(content.get(start..).unwrap_or(&[]));
    let count = text.lines().count();
    if count == 0 {
        return "(nothing was written this start)".to_owned();
    }
    text.lines()
        .skip(count.saturating_sub(12))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run_with(args: RunArgs, capture: Capture) -> Result<()> {
    let usage = Arc::new(proxenos::usage::UsageStore::default());
    let switches = Arc::new(proxenos::recorder::Switches::new(match capture {
        Capture::Nothing => None,
        Capture::Ingress => Some(proxenos::recorder::Mode::Ingress),
        Capture::Upstream => Some(proxenos::recorder::Mode::Upstream),
    }));
    let config = Config::load()?;
    let port = args.port.unwrap_or(config.port);

    // Values that parse but cannot work, refused before binding rather than
    // clamped — a clamp makes an operator's mistake look like it was accepted.
    config.validate()?;

    let credentials: Arc<dyn proxenos::auth::store::AccountStore> =
        Arc::new(proxenos::auth::store::FileStore::new(credential_path()));

    // Which account's mapping is in force. Read before the mapping is resolved
    // rather than after, because a catalog is one account's menu (§7.0) and a
    // mapping written for one account can name a model another is not offered.
    // Nothing selected — no login yet — takes the shared tables, which is the
    // only answer available and the one a first run wants.
    let serving = serving_account(&credentials);

    // Refused before binding: a daemon that starts with an incomplete mapping
    // breaks WebFetch in a way that looks unrelated to tier mapping (§7.1).
    let mut tiers = config.tiers_for(serving.as_deref()).resolve()?;

    // Also refused before binding, so a mistyped ceiling is caught at startup
    // rather than silently spending at full rate.
    let effort_ceiling = config.effort_ceiling_for(serving.as_deref())?;
    match effort_ceiling {
        Some(effort) => tracing::info!(?effort, "reasoning effort is capped"),
        None => tracing::info!("reasoning effort is whatever the client asks for"),
    }

    let listener = daemon::bind(port).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "listening");

    let tokens = Arc::new(proxenos::auth::tokens::TokenSource::new(
        Arc::clone(&credentials) as Arc<dyn proxenos::auth::store::CredentialStore>,
        proxenos::auth::flow::token_endpoint(),
        proxenos::auth::flow::CLIENT_ID,
        Arc::new(proxenos::auth::tokens::SystemClock),
    ));

    // One authorizer for every path that authenticates: it reads the store per
    // request, so a switch or a login reaches the next request with nothing
    // rebuilt.
    let authorizer: Arc<dyn proxenos::auth::authorize::Authorizer> =
        Arc::new(proxenos::auth::authorize::AccountAuthorizer::new(
            Arc::clone(&credentials),
            Arc::clone(&tokens),
        ));

    // §7.0 — fetched with credentials when there are any. An unreachable
    // catalog falls back rather than failing: fetch failure is not evidence
    // that a model went away (§7.1), and a daemon that will not start because
    // the network blinked is the worse failure.
    //
    // In a slot holding both endpoints, not a value: a catalog describes the
    // plan of the account it was fetched for, so switching accounts on a
    // running daemon has to be able to replace it — from the endpoint that
    // account's kind belongs to, which is why the pair travels together.
    // Shared with the ingress, or a replacement would move what the socket
    // reports and not what routes turns.
    let catalog = Arc::new(proxenos::catalog::CatalogSource::new(
        proxenos::catalog::Catalog::fallback(),
        config.upstream.catalog.clone(),
        config.upstream.key.catalog.clone(),
        config.upstream.client_version.clone(),
        config.upstream.effective_window_percent,
    ));
    match authorizer.authorize().await {
        Ok(authorization) => {
            catalog.refresh(&authorization).await;
        }
        Err(error) => {
            tracing::info!(%error, "not authenticated; the model list is the fallback");
        }
    }

    // A shipped default naming a model this account cannot see is replaced
    // rather than refused: `gpt-5.6-sol` is plan-gated, so the default mapping
    // would otherwise fail to start for most accounts. A model the operator
    // stated is left alone and validated below.
    let substituted = catalog
        .current()
        .substitute_unavailable_defaults(&mut tiers);
    if !substituted.is_empty() {
        tracing::info!(
            tiers = substituted.join(", "),
            "some default tier mappings were substituted for models this account has"
        );
    }

    // An unknown model id is refused here, and the error names what the catalog
    // does have — which is the fastest way to find the id you actually meant.
    catalog.current().validate(
        &tiers
            .iter()
            .map(|tier| tier.model.clone())
            .collect::<Vec<_>>(),
    )?;

    // One policy, shared. The control socket can move the tier mapping and the
    // effort ceiling on a running daemon, and the ingress routes turns from the
    // same value — so a change moves what actually happens rather than only
    // what `status` reports.
    let policy = Arc::new(proxenos::policy::Policy::new(
        proxenos::policy::Snapshot::new(tiers.clone(), effort_ceiling),
    ));

    // One signal, shared: asking over the socket has to move this process, not
    // merely answer about it.
    let shutdown = Arc::new(proxenos::daemon::Shutdown::default());

    // One store, shared with the ingress: a switch that cleared a set of
    // conversations the ingress does not serve would clear nothing.
    let sessions = Arc::new(proxenos::session::SessionStore::new());

    let control_state = proxenos::control::handler::ControlState {
        port: addr.port(),
        policy: Arc::clone(&policy),
        catalog: Arc::clone(&catalog),
        credentials: Arc::clone(&credentials),
        capture: Arc::clone(&switches),
        usage: Arc::clone(&usage),
        login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
        config: Arc::new(config.clone()),
        shutdown: Arc::clone(&shutdown),
        tokens: Some(Arc::clone(&tokens)),
        usage_endpoint: config.upstream.usage.clone(),
        sessions: Arc::clone(&sessions),
        config_path: Some(proxenos::config::config_path()),
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
    let conduit_authorizer = Arc::clone(&authorizer);
    let conduit_credentials = Arc::clone(&credentials);
    let conduit_endpoint = config.upstream.endpoint.clone();
    let conduit_websocket = config.upstream.websocket.clone();
    let conduit_key = config.upstream.key.endpoint.clone();
    let conduits: proxenos::ingress::ConduitFactory = Arc::new(move |session_id| {
        // Which endpoint this conversation belongs to is decided by the
        // account serving turns when it starts. A switch drops every live
        // session (§3), so a conversation never outlives the kind it was
        // opened for.
        let kind = proxenos::auth::authorize::selected_kind(&conduit_credentials);
        let (endpoint, socket) = match kind {
            proxenos::auth::authorize::Kind::Key => (&conduit_key, None),
            proxenos::auth::authorize::Kind::Subscription => {
                (&conduit_endpoint, Some(&conduit_websocket))
            }
        };

        let http = Arc::new(
            HttpTransport::new(endpoint)
                .for_endpoint(kind)
                .with_credentials(Arc::clone(&conduit_authorizer))
                .with_compression(compression),
        );
        let websocket = websocket_enabled.then_some(socket).flatten().map(|socket| {
            Arc::new(
                proxenos::upstream::websocket::WebSocketTransport::new(socket)
                    .with_credentials(Arc::clone(&conduit_authorizer))
                    .with_compression(compression),
            )
        });
        Arc::new(proxenos::upstream::conduit::Conduit::new(
            http, websocket, session_id,
        ))
    });

    // The fetched catalog, not a fresh fallback. Everything that depends on
    // knowing a model — the window guard (§7.2) and both effort caps (§2.7) —
    // reads it from here, and a fallback entry states neither, so handing one
    // over leaves all three silently doing nothing.
    tracing::info!(
        models = catalog.current().ids().len(),
        authoritative = catalog.current().authoritative,
        "model catalog in use"
    );

    let state = AppState {
        policy: Arc::clone(&policy),
        catalog: Arc::clone(&catalog),
        // Only reached if the factory is absent, which it is not here.
        transport: Arc::new(
            HttpTransport::new(&config.upstream.endpoint).with_credentials(Arc::clone(&authorizer)),
        ),
        conduits: Some(conduits),
        // §5.4 — empty streams are recorded whether or not capture was asked
        // for, because an empty stream is always a defect and is otherwise
        // invisible.
        recorder: Some(proxenos::recorder::Recorder::new(
            proxenos::recorder::Recorder::default_directory(),
        )),
        capture: Arc::clone(&switches),
        usage: Arc::clone(&usage),
        instructions: Arc::new(config.instructions.clone()),
        sessions,
    };

    // Whichever comes first: the listener stopping on its own, or a stop asked
    // for over the socket. An in-flight turn is cut — a person typing `stop`
    // means it, and the client's own retry handles a dropped connection.
    tokio::select! {
        result = daemon::serve(listener, state) => result?,
        () = shutdown.wait() => tracing::info!("stopping, as asked over the control socket"),
    }
    Ok(())
}

/// The name of the account serving turns, which is what an account section in
/// the configuration is keyed by.
///
/// The name rather than the account id: a key account has no id, and the name
/// is the string every account verb already takes.
fn serving_account(store: &Arc<dyn proxenos::auth::store::AccountStore>) -> Option<String> {
    store
        .accounts()
        .ok()?
        .into_iter()
        .find(|account| account.selected)
        .map(|account| account.name)
}

/// Where credentials live. Never the configuration file (§8).
///
/// The same directory the configuration is read from, resolved by the same
/// function: two copies of this rule drift, and the copy that drifts sends the
/// daemon looking for credentials somewhere the login never wrote them.
fn credential_path() -> std::path::PathBuf {
    proxenos::config::config_dir().join("credentials.json")
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
