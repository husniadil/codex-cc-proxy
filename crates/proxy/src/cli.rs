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
    /// Environment for Claude Code.
    Env(EnvArgs),
    /// Probe backend capabilities.
    Doctor(DoctorArgs),
    /// What quota is left, as of the last turn.
    Usage(UsageArgs),
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
    #[arg(long)]
    pub json: bool,
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
