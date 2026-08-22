//! `docs/api.md` §2.6 — the supervisor unit.
//!
//! Everything the unit *is* — the platform decision, the socket path, the
//! rendering — is a pure function over data named here rather than read from
//! the process, so a plist that would supervise nothing is refused in a test
//! rather than discovered by an operator whose daemon never came back.
//!
//! Installing and removing it is the only part that touches the machine, and
//! that lives with the rest of the I/O.

use crate::control;
use crate::error::ProxyError;
use std::ffi::OsString;
use std::path::PathBuf;

/// What the supervised job is called, to launchd and to the operator.
pub const LABEL: &str = "proxenos.daemon";

/// Which supervisor a platform has, as an input rather than a `cfg`.
///
/// A `cfg` would make the refusal untestable on the machine that ships it,
/// which is exactly backwards: the refusal is the branch nobody exercises by
/// accident, so it is the one that most needs a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Other(&'static str),
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other(std::env::consts::OS)
        }
    }
}

/// What the installing shell saw, handed in whole.
#[derive(Debug, Clone)]
pub struct Origin {
    /// This binary, as launchd will have to resolve it.
    pub program: PathBuf,
    /// Where the daemon already logs.
    pub log: PathBuf,
    /// `PROXENOS_HOME`, if the operator's environment names one.
    pub proxenos_home: Option<OsString>,
    /// `TMPDIR`, if it names one.
    pub tmpdir: Option<OsString>,
}

/// A unit that would supervise this daemon, before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub label: String,
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub log: PathBuf,
    /// Exactly what the job's environment carries, and a closed set: adding to
    /// it is a deliberate edit, not a filter someone can widen by accident.
    pub environment: Vec<(String, String)>,
    /// The socket the supervised daemon will bind, derived from what the
    /// environment above hands it.
    pub socket: PathBuf,
}

/// Where the unit is written, given the user's home directory.
pub fn plist_path(home: &std::path::Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Decide what would be installed, or refuse and say why.
pub fn plan(platform: &Platform, origin: &Origin) -> Result<Unit, ProxyError> {
    if let Platform::Other(name) = platform {
        return Err(ProxyError::invalid_request(format!(
            "there is no supervisor for {name} here: launchd is the one this implements, and a \
             per-user LaunchAgent is a macOS thing. Supervising {name} would take a systemd user \
             unit — a `proxenos.service` with `Restart=always` under \
             `~/.config/systemd/user` — which nothing here writes, because a unit that is written \
             but never runs is worse than no verb: it reports success and supervises nothing. \
             Until then, start the daemon with `proxenos run --detach` and supervise it with \
             whatever this platform already has."
        )));
    }

    if !origin.program.is_absolute() {
        return Err(ProxyError::invalid_request(format!(
            "the supervisor needs an absolute path to this binary and was given {}. launchd \
             resolves nothing — no `PATH`, no working directory — so a relative program is a job \
             that never starts and says so nowhere.",
            origin.program.display()
        )));
    }

    let home = origin.proxenos_home.as_ref().map(PathBuf::from);

    // **Resolved, never left absent.** `TMPDIR` is carried whether or not the
    // installing shell names one, because launchd does not hand a job an empty
    // environment: it supplies a `TMPDIR` of its own. Carrying nothing would
    // therefore not mean "no `TMPDIR`" to the daemon — it would mean launchd's,
    // while the path planned here fell back to `/tmp` and the operator's CLI
    // went on dialing whatever its own shell says. That is the failure this
    // whole verb exists to prevent, arrived at from the inside. Carrying the
    // value the derivation actually used is what closes it.
    let tmpdir = origin
        .tmpdir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| control::FALLBACK_TMPDIR.into());

    let socket = control::path_for(home.as_deref(), Some(&tmpdir));
    control::ensure_addressable(&socket)?;

    // Carried explicitly rather than inherited. A launchd job's environment is
    // not the login shell's, and the socket path is derived from both of these
    // — so naming them is what makes the daemon's bind and the operator's dial
    // the same path by construction rather than by luck.
    let mut environment = Vec::new();
    if let Some(home) = &home {
        environment.push(("PROXENOS_HOME".to_owned(), home.display().to_string()));
    }
    environment.push(("TMPDIR".to_owned(), tmpdir.display().to_string()));

    Ok(Unit {
        label: LABEL.to_owned(),
        program: origin.program.clone(),
        // Foreground. A job that forks away leaves launchd supervising a
        // process that has already exited, and its respawn then fights the
        // daemon it cannot see.
        arguments: vec!["run".to_owned()],
        log: origin.log.clone(),
        environment,
        socket,
    })
}

/// Render the unit as a launchd property list.
pub fn render(unit: &Unit) -> String {
    let mut document = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST \
         1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist \
         version=\"1.0\">\n<dict>\n",
    );

    document.push_str(&format!(
        "    <key>Label</key>\n    <string>{}</string>\n",
        escape(&unit.label)
    ));

    document.push_str("    <key>ProgramArguments</key>\n    <array>\n");
    document.push_str(&format!(
        "        <string>{}</string>\n",
        escape(&unit.program.display().to_string())
    ));
    for argument in &unit.arguments {
        document.push_str(&format!("        <string>{}</string>\n", escape(argument)));
    }
    document.push_str("    </array>\n");

    if !unit.environment.is_empty() {
        document.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (key, value) in &unit.environment {
            document.push_str(&format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                escape(key),
                escape(value)
            ));
        }
        document.push_str("    </dict>\n");
    }

    let log = escape(&unit.log.display().to_string());
    document.push_str(&format!(
        "    <key>RunAtLoad</key>\n    <true/>\n    <key>KeepAlive</key>\n    \
         <true/>\n    <key>StandardOutPath</key>\n    <string>{log}</string>\n    \
         <key>StandardErrorPath</key>\n    <string>{log}</string>\n"
    ));

    document.push_str("</dict>\n</plist>\n");
    document
}

/// XML text, not markup. A home directory may legally contain `&`.
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// What is on disk, against what this environment would write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    Absent,
    Current,
    /// Installed, and not the unit this environment produces.
    ///
    /// The case worth naming: `TMPDIR` moving between install and now changes
    /// the socket the supervised daemon binds while the operator's CLI goes on
    /// dialing the old one. The daemon answers turns on the port and every verb
    /// reports connection refused, which reads as a dead daemon and is not one.
    /// A text comparison catches it without parsing anything back.
    Divergent,
}

/// Compare what is installed against what would be installed now.
pub fn compare(existing: Option<&str>, wanted: &Unit) -> Installed {
    match existing {
        None => Installed::Absent,
        Some(document) if document == render(wanted) => Installed::Current,
        Some(_) => Installed::Divergent,
    }
}

/// What `install` says about a daemon that is already answering.
///
/// The supervised job runs `run`, and `run` refuses a port another daemon
/// holds. So an install performed while a hand-started daemon is up is a real
/// install of a job that cannot start yet: launchd respawns it into the same
/// refusal until the port is free. The install is not undone by that and the
/// daemon is not stopped by this verb — ending a process the operator started
/// by hand is not what was asked for — so what is owed is the observation and
/// the way to hand over. Under a supervisor, `stop` is that way (§2.4).
///
/// `None` when nothing is answering, which is the normal path and gains no
/// output.
pub fn port_notice(answering: Option<&str>) -> Option<String> {
    let version = answering?;
    Some(format!(
        "  {version} is already answering, so the supervised job cannot take the port yet\n  \
         hand it over with `proxenos stop`; the supervisor starts this build in its place"
    ))
}
