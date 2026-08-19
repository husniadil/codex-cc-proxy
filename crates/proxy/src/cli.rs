//! Command line surface. The verb set is semver-bound — see `docs/api.md` §6.

use clap::Parser;
use clap::Subcommand;

/// Run Claude Code against models served over a ChatGPT subscription.
#[derive(Debug, Parser)]
#[command(name = "codex-cc-proxy", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the daemon.
    Run(RunArgs),
    /// Authenticate.
    Login,
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
pub struct RunArgs {
    /// Port to bind on loopback. Overrides the configured value.
    #[arg(long, env = "CODEX_CC_PROXY_PORT")]
    pub port: Option<u16>,
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
#[derive(Debug, Subcommand)]
pub enum RecordMode {
    /// Capture what the client sends, before translation.
    Ingress,
    /// Capture what the backend sends back.
    Upstream,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// `env --json` and `settings` are one document under two names, so a
    /// caller cannot pick the one that leaves the policy out.
    #[test]
    fn settings_parses_as_a_verb_of_its_own() {
        let cli = Cli::try_parse_from(["codex-cc-proxy", "settings"]).unwrap();
        assert!(matches!(cli.command, Command::Settings));
    }

    /// Everything after the program name belongs to the child, hyphens and
    /// all. A launcher that swallowed one would make the thing it wraps
    /// undebuggable.
    #[test]
    fn exec_forwards_everything_after_the_program() {
        let cli =
            Cli::try_parse_from(["codex-cc-proxy", "exec", "claude", "--resume", "abc", "-p"])
                .unwrap();
        let Command::Exec(args) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(args.command, ["claude", "--resume", "abc", "-p"]);
    }

    /// `--` for the command whose own first argument would otherwise be read
    /// here. The separator itself is not passed on.
    #[test]
    fn exec_accepts_a_double_dash_boundary() {
        let cli =
            Cli::try_parse_from(["codex-cc-proxy", "exec", "--", "claude", "--help"]).unwrap();
        let Command::Exec(args) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(args.command, ["claude", "--help"]);
    }

    #[test]
    fn record_requires_an_explicit_mode() {
        // Defaulting the mode would let `record` spend quota without being asked
        // to. The mode is always stated.
        let parsed = Cli::try_parse_from(["codex-cc-proxy", "record"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn record_ingress_parses() {
        let cli = Cli::try_parse_from(["codex-cc-proxy", "record", "ingress"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Record(RecordArgs {
                mode: RecordMode::Ingress
            })
        ));
    }
}
