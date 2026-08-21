//! Command line surface. The verb set is semver-bound — see `docs/api.md` §6.

use clap::Parser;
use clap::Subcommand;

/// Run Claude Code against models served over a ChatGPT subscription.
#[derive(Debug, Parser)]
#[command(name = "proxenos", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the daemon.
    Run(RunArgs),
    /// Authenticate. Adds an account; it never replaces one.
    Login(LoginArgs),
    /// Stored accounts, and which one serves turns.
    Accounts(AccountsArgs),
    /// Connection, tier mapping, and whether the catalog was reachable.
    Status,
    /// Available models.
    Models,
    /// Environment for Claude Code, as shell exports.
    Env(EnvArgs),
    /// The same configuration as one client settings document.
    ///
    /// A separate verb rather than a flag on `env`, because it produces
    /// something an environment is not: it carries the client policy no export
    /// can hold. `env --json` prints the identical document and stays for the
    /// callers that already use it.
    Settings,
    /// Ask the running daemon to stop.
    ///
    /// Named for what it asks, not for what follows: whether anything starts
    /// the daemon again belongs to whatever supervises it. What happened is
    /// reported from what was observed afterwards.
    Stop,
    /// Run a command with this proxy's configuration applied.
    ///
    /// Named for what it does rather than what it launches: a launcher that
    /// only ever starts one program is a launcher that cannot start the next
    /// one.
    Exec(ExecArgs),
    /// Probe backend capabilities.
    Doctor(DoctorArgs),
    /// What quota is left, as of the last turn.
    Usage(UsageArgs),
    /// Wrap a status-line script, adding the quota the client cannot supply.
    Statusline(StatuslineArgs),
    /// Capture exchanges as fixtures.
    Record(RecordArgs),
}

#[derive(Debug, clap::Args)]
pub struct LoginArgs {
    /// Store a key read from stdin instead of starting an authorization.
    ///
    /// The secret arrives on stdin and never as an argument: an argument is
    /// visible to every process on the machine and lands in shell history.
    #[arg(long)]
    pub key: bool,
    /// Store a Claude subscription token, guided.
    ///
    /// The same stored credential `--key --provider anthropic` produces, with
    /// the part a person needs and a pipe does not: where the token comes
    /// from, an entry that does not echo it, and a refusal before a credential
    /// of the wrong kind is filed under a name that spends it later. A
    /// non-terminal stdin still reads the token from the pipe, so nothing
    /// scripted regresses.
    #[arg(long = "setup-token", conflicts_with_all = ["key", "provider"])]
    pub setup_token: bool,
    /// What to call the account this authorization produces.
    ///
    /// Without one it is named by the account id the grant carries. A label is
    /// a local name for the account and never reaches the backend.
    #[arg(long = "as", value_name = "NAME")]
    pub label: Option<String>,
    /// Which provider's endpoints the stored key is spent against.
    ///
    /// Only meaningful with `--key`: an authorization is performed against one
    /// provider's server, so the grant it produces has nothing to choose. The
    /// default is the provider this project started with, so a login that
    /// names none stores what it always stored.
    #[arg(long, value_enum, default_value_t = proxenos::auth::store::Provider::Codex)]
    pub provider: proxenos::auth::store::Provider,
}

#[derive(Debug, clap::Args)]
pub struct AccountsArgs {
    /// Serve every following turn as this account.
    ///
    /// A switch rather than a listing, which is why it is a flag: an account
    /// changed by a mistyped positional is a turn billed to the wrong one.
    #[arg(long = "use", value_name = "NAME")]
    pub select: Option<String>,
    /// Forget this account, leaving the rest usable.
    ///
    /// The name is required: an account is gone once this returns, and the
    /// operator naming which one is the whole safeguard.
    #[arg(long = "forget", value_name = "NAME", conflicts_with = "select")]
    pub forget: Option<String>,
    /// Change what an account is called here, leaving its grant alone.
    ///
    /// Both halves, old name first. A login carrying no `--as` names the
    /// account by the id the backend knows it by, and changing that should not
    /// cost an authorization.
    #[arg(
        long = "rename",
        value_names = ["FROM", "TO"],
        num_args = 2,
        conflicts_with_all = ["select", "forget"]
    )]
    pub rename: Option<Vec<String>>,
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Port to bind on loopback. Overrides the configured value.
    #[arg(long, env = "PROXENOS_PORT")]
    pub port: Option<u16>,
    /// Start the daemon in the background and return once it answers.
    #[arg(long)]
    pub detach: bool,
}

#[derive(Debug, clap::Args)]
pub struct EnvArgs {
    /// Emit a Claude Code settings fragment instead of shell exports.
    ///
    /// The same output as the `settings` verb, which is the name for it.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct ExecArgs {
    /// The program to start, and everything to hand it.
    ///
    /// Opaque from the program name onward, so the client's own flags keep
    /// working unchanged. `--` is accepted for a command whose first argument
    /// would otherwise be read as this verb's.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct DoctorArgs {
    /// Run one probe by name instead of the whole suite.
    #[arg(long)]
    pub probe: Option<String>,
    /// Where the fixture corpus lives.
    #[arg(long)]
    pub fixtures: Option<std::path::PathBuf>,
    /// Answer the probes from the real backend instead of the recordings.
    ///
    /// This spends inference quota — one turn per probe — and needs
    /// credentials. Without it nothing is contacted and nothing is billed.
    #[arg(long)]
    pub live: bool,
}

#[derive(Debug, clap::Args)]
pub struct UsageArgs {
    /// Emit the raw snapshot, for a status line or a script.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct StatuslineArgs {
    /// The status-line command to run, after `--`.
    ///
    /// Its stdin is the client's payload with the quota merged in, and its
    /// output is passed through untouched. Omit it to print the merged payload
    /// instead, which is what a script that would rather pipe it wants.
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct RecordArgs {
    #[command(subcommand)]
    pub mode: RecordMode,
}

/// The two capture modes differ in what they cost: ingress needs no credentials
/// and spends nothing, upstream needs both.
///
/// Each mode runs a daemon, so each carries the daemon's port control. Declared
/// here rather than assembled in the handler, because a declared binding is the
/// only kind clap actually runs: the first version built the run arguments by
/// hand and silently dropped `PROXENOS_PORT` on this verb alone.
#[derive(Debug, Subcommand)]
pub enum RecordMode {
    /// Capture what the client sends, before translation.
    Ingress {
        /// Port to bind on loopback. Overrides the configured value.
        #[arg(long, env = "PROXENOS_PORT")]
        port: Option<u16>,
    },
    /// Capture what the backend sends back.
    Upstream {
        /// Port to bind on loopback. Overrides the configured value.
        #[arg(long, env = "PROXENOS_PORT")]
        port: Option<u16>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn stop_parses_as_a_verb_of_its_own() {
        let cli = Cli::try_parse_from(["proxenos", "stop"]).unwrap();
        assert!(matches!(cli.command, Command::Stop));
    }

    /// The flag belongs to `run` alone; without it the daemon owns the
    /// terminal, which is the right default for watching it work.
    #[test]
    fn run_can_be_asked_to_detach() {
        let cli = Cli::try_parse_from(["proxenos", "run", "--detach"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("run should parse");
        };
        assert!(args.detach);

        let cli = Cli::try_parse_from(["proxenos", "run"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("run should parse");
        };
        assert!(!args.detach);
    }

    /// A key is not an authorization, and the secret is never an argument.
    #[test]
    fn login_can_store_a_key_instead_of_starting_a_flow() {
        let cli = Cli::try_parse_from(["proxenos", "login", "--key", "--as", "billing"]).unwrap();
        let Command::Login(args) = cli.command else {
            panic!("login should parse");
        };
        assert!(args.key);
        assert_eq!(args.label.as_deref(), Some("billing"));

        // There is nowhere to put a secret on the command line.
        assert!(Cli::try_parse_from(["proxenos", "login", "--key", "sk-secret"]).is_err());
    }

    /// A stored key names the provider it is spent against, and defaults to
    /// the one this project started with.
    ///
    /// `roadmap.md` v0.6.0 — routing reads the provider off the account, so
    /// this flag is what puts an account on the second provider's path at all.
    /// The secret still has nowhere to go on the command line: the provider is
    /// a name, and the key stays on stdin.
    #[test]
    fn login_key_names_the_provider_it_is_for() {
        let cli = Cli::try_parse_from([
            "proxenos",
            "login",
            "--key",
            "--as",
            "relay",
            "--provider",
            "anthropic",
        ])
        .unwrap();
        let Command::Login(args) = cli.command else {
            panic!("login should parse");
        };
        assert_eq!(args.provider, proxenos::auth::store::Provider::Anthropic);

        // Naming none is the provider this verb has always meant.
        let cli = Cli::try_parse_from(["proxenos", "login", "--key", "--as", "billing"]).unwrap();
        let Command::Login(args) = cli.command else {
            panic!("login should parse");
        };
        assert_eq!(args.provider, proxenos::auth::store::Provider::Codex);

        // A provider this proxy has no path for is refused at the boundary
        // rather than stored and discovered on the first turn.
        assert!(
            Cli::try_parse_from([
                "proxenos",
                "login",
                "--key",
                "--as",
                "x",
                "--provider",
                "gemini",
            ])
            .is_err()
        );
    }

    /// The guided flow is a front door over the same stored credential, so it
    /// refuses the flags that would describe a different one.
    #[test]
    fn login_setup_token_refuses_the_flags_that_contradict_it() {
        let cli =
            Cli::try_parse_from(["proxenos", "login", "--setup-token", "--as", "sub"]).unwrap();
        let Command::Login(args) = cli.command else {
            panic!("login should parse");
        };
        assert!(args.setup_token);
        assert!(!args.key);

        assert!(Cli::try_parse_from(["proxenos", "login", "--setup-token", "--key"]).is_err());
        assert!(
            Cli::try_parse_from([
                "proxenos",
                "login",
                "--setup-token",
                "--provider",
                "anthropic",
            ])
            .is_err()
        );
    }

    /// A login names the account it produces, so an operator holding two of
    /// them has something to call each.
    #[test]
    fn login_takes_a_label() {
        let cli = Cli::try_parse_from(["proxenos", "login", "--as", "work"]).unwrap();
        let Command::Login(args) = cli.command else {
            panic!("login should parse");
        };
        assert_eq!(args.label.as_deref(), Some("work"));

        let cli = Cli::try_parse_from(["proxenos", "login"]).unwrap();
        let Command::Login(args) = cli.command else {
            panic!("login should parse");
        };
        assert_eq!(args.label, None);
    }

    /// Renaming takes both halves and stands alone. One of them missing would
    /// leave the command guessing which account it was about.
    #[test]
    fn accounts_renames_with_both_halves_or_not_at_all() {
        let cli = Cli::try_parse_from(["proxenos", "accounts", "--rename", "old", "new"]).unwrap();
        let Command::Accounts(args) = cli.command else {
            panic!("accounts should parse");
        };
        assert_eq!(
            args.rename.as_deref(),
            Some(["old".to_owned(), "new".to_owned()].as_slice())
        );

        assert!(Cli::try_parse_from(["proxenos", "accounts", "--rename", "old"]).is_err());
        assert!(
            Cli::try_parse_from([
                "proxenos", "accounts", "--rename", "old", "new", "--use", "other"
            ])
            .is_err()
        );
    }

    /// Forgetting names its account and cannot be combined with switching:
    /// one call, one thing, and the destructive one always says what it is
    /// about to lose.
    #[test]
    fn accounts_forgets_only_the_account_it_is_given() {
        let cli = Cli::try_parse_from(["proxenos", "accounts", "--forget", "spare"]).unwrap();
        let Command::Accounts(args) = cli.command else {
            panic!("accounts should parse");
        };
        assert_eq!(args.forget.as_deref(), Some("spare"));
        assert_eq!(args.select, None);

        // A bare `--forget` has no default target.
        assert!(Cli::try_parse_from(["proxenos", "accounts", "--forget"]).is_err());
        // And it is not a switch.
        assert!(
            Cli::try_parse_from(["proxenos", "accounts", "--use", "a", "--forget", "b"]).is_err()
        );
    }

    /// Listing is the default; switching has to be asked for. A bare
    /// `accounts` that switched on a stray argument would bill a turn to the
    /// wrong account.
    #[test]
    fn accounts_lists_by_default_and_switches_only_when_asked() {
        let cli = Cli::try_parse_from(["proxenos", "accounts"]).unwrap();
        let Command::Accounts(args) = cli.command else {
            panic!("accounts should parse");
        };
        assert_eq!(args.select, None);

        let cli = Cli::try_parse_from(["proxenos", "accounts", "--use", "work"]).unwrap();
        let Command::Accounts(args) = cli.command else {
            panic!("accounts should parse");
        };
        assert_eq!(args.select.as_deref(), Some("work"));

        // A bare name is not a switch.
        assert!(Cli::try_parse_from(["proxenos", "accounts", "work"]).is_err());
    }

    /// `env --json` and `settings` are one document under two names, so a
    /// caller cannot pick the one that leaves the policy out.
    #[test]
    fn settings_parses_as_a_verb_of_its_own() {
        let cli = Cli::try_parse_from(["proxenos", "settings"]).unwrap();
        assert!(matches!(cli.command, Command::Settings));
    }

    /// Everything after the program name belongs to the child, hyphens and
    /// all. A launcher that swallowed one would make the thing it wraps
    /// undebuggable.
    #[test]
    fn exec_forwards_everything_after_the_program() {
        let cli =
            Cli::try_parse_from(["proxenos", "exec", "claude", "--resume", "abc", "-p"]).unwrap();
        let Command::Exec(args) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(args.command, ["claude", "--resume", "abc", "-p"]);
    }

    /// `--` for the command whose own first argument would otherwise be read
    /// here. The separator itself is not passed on.
    #[test]
    fn exec_accepts_a_double_dash_boundary() {
        let cli = Cli::try_parse_from(["proxenos", "exec", "--", "claude", "--help"]).unwrap();
        let Command::Exec(args) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(args.command, ["claude", "--help"]);
    }

    #[test]
    fn record_requires_an_explicit_mode() {
        // Defaulting the mode would let `record` spend quota without being asked
        // to. The mode is always stated.
        let parsed = Cli::try_parse_from(["proxenos", "record"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn record_ingress_parses() {
        let cli = Cli::try_parse_from(["proxenos", "record", "ingress"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Record(RecordArgs {
                mode: RecordMode::Ingress { .. }
            })
        ));
    }

    #[test]
    fn record_takes_the_port_the_daemon_verbs_take() {
        let cli =
            Cli::try_parse_from(["proxenos", "record", "ingress", "--port", "18799"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Record(RecordArgs {
                mode: RecordMode::Ingress { port: Some(18799) }
            })
        ));
    }
}
