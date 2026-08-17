//! `docs/api.md` §4 — configuration.
//!
//! Credentials are never stored here.

use crate::error::ProxyError;
use serde::Deserialize;
pub mod edit;

use serde::Serialize;

pub const DEFAULT_PORT: u16 = 8787;

/// Where the configuration lives.
///
/// `CODEX_CC_PROXY_HOME` overrides it, which is what makes the daemon testable
/// without touching the developer's own configuration.
pub fn config_dir() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("CODEX_CC_PROXY_HOME") {
        return std::path::PathBuf::from(home);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("codex-cc-proxy")
}

pub fn config_path() -> std::path::PathBuf {
    config_dir().join("config.toml")
}

/// An example that can be copied verbatim into place.
pub const EXAMPLE: &str = r#"# Every key here has a default. A file that states nothing works; write only
# what you want to change.

port = 8787

# Optional. Caps reasoning effort on every request, whatever the client asks
# for: one of none, minimal, low, medium, high, xhigh, max, ultra. `ultracode`
# is the client's name for `ultra` and is accepted as one.
#
# `ultra` exists only on some models and only on a paid plan. A model whose
# catalog entry does not offer it is capped below it; where the catalog offers
# it and the account cannot use it, the backend refuses the request and says so.
#
# Capped again by what the model accepts, and raised to its lowest level when it
# accepts nothing that low: `minimal` is refused by some models outright, so it
# is moved to the nearest they will take rather than sent and failed.
#
# Both keys sit above the tables on purpose. In TOML a bare key written after a
# table header belongs to that table, so `effort` placed below `[tiers]` is
# `tiers.effort` — a different setting entirely.
# effort = "low"

# The defaults, shown so they can be changed. An omitted tier takes the value
# below; a tier written blank is refused rather than defaulted. WebFetch runs on
# the haiku tier, so that one matters more than it looks.
[tiers]
opus   = "gpt-5.6-terra"
sonnet = "gpt-5.6-luna"
haiku  = "gpt-5.6-luna"
fable  = "gpt-5.6-sol"

[transport]
websocket   = true

# Compression on both transports: zstd on an HTTP body, `permessage-deflate` on
# the socket, negotiated during the upgrade. About two thirds off the wire in
# each direction. It saves bytes, never tokens — quota is unaffected either way.
compression = true

[instructions]
# Lead the system prompt with one line naming the model that is actually
# answering. The prompt the client sends opens by calling the model something
# else, and nothing in the client can be made to say otherwise.
identity = true

# A short budget telling the model to read the smallest slice that answers the
# question rather than whole files. On by default, and deliberately so: this
# conversation is replayed upstream on every turn and echoed back three times,
# so broad reading spends the context window quickly.
working_budget = true

# Optional. Placed after the working budget, so it outranks it. Keep it
# constant: text that changes between turns changes `instructions`, and that
# costs every delta and every cache hit.
# append = """
# Prefer ripgrep over find.
# """

# Every key here has a default that is correct today and will not always be.
# They are configurable so a pinned binary can be repointed rather than rebuilt.
[upstream]
# What this proxy reports when asking for the model list. Not this crate's
# version. The backend filters the list by it — each model declares a minimum,
# and a version below every minimum returns an EMPTY LIST rather than an error,
# which reads exactly like an account with no models. Raise it when a new model
# is missing from `codex-cc-proxy models` but exists for your account.
# client_version = "2.0.0"

# The share of a context window left usable once instructions, tool overhead,
# and output are accounted for, where the catalog states no share of its own.
# This is the figure the client is told, so it decides when compaction fires:
# lower compacts sooner and wastes window, higher risks a turn refused for
# length. A model whose catalog entry states its own share keeps that one.
# effective_window_percent = 95.0

# endpoint  = "https://chatgpt.com/backend-api/codex/responses"
# websocket = "wss://chatgpt.com/backend-api/codex/responses"
# catalog   = "https://chatgpt.com/backend-api/codex/models"
"#;

/// Unknown keys are refused rather than ignored.
///
/// Tolerating them looks forgiving and is not: in TOML a top-level key written
/// after a table header belongs to that table, so `effort` below `[tiers]` is
/// `tiers.effort`. Ignored quietly, the operator believes they capped their
/// spending and every request runs at the backend's default instead. A
/// configuration key that does nothing is worse than one that is refused,
/// because only one of them says so.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub tiers: Tiers,
    #[serde(default)]
    pub transport: TransportConfig,
    /// A ceiling on reasoning effort, for operators who care what a turn costs.
    ///
    /// The client cannot choose this: it does not know whose quota it is
    /// spending. Omitted means no ceiling, and the backend's own default
    /// applies — not that effort is zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub instructions: InstructionsConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
}

/// Where the backend is, and what this client says it is.
///
/// These have defaults that are correct today and will not always be. They are
/// here so a pinned binary can be repointed rather than rebuilt — and because
/// `client_version` in particular fails in a way nothing else can diagnose.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// The version this proxy reports when asking for the model catalog.
    ///
    /// Not this crate's version. The backend filters the catalog by it — each
    /// entry declares a minimum, and a version below every minimum returns an
    /// **empty list rather than an error**, which reads exactly like an account
    /// with no models. It goes stale as new models raise the bar, and when it
    /// does the symptom is a daemon that starts fine and offers nothing.
    #[serde(default = "default_client_version")]
    pub client_version: String,
    /// The share of a context window left usable once instructions, tool
    /// overhead, and output are accounted for. Applied where the catalog states
    /// no percentage of its own.
    ///
    /// This is the figure the client is told, so it decides when compaction
    /// fires. Lowering it compacts sooner and wastes window; raising it risks a
    /// turn refused for length, which the client cannot retry its way out of.
    #[serde(default = "default_effective_window_percent")]
    pub effective_window_percent: f64,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_websocket")]
    pub websocket: String,
    #[serde(default = "default_catalog")]
    pub catalog: String,
    /// Where a quota figure can be asked for rather than waited for.
    ///
    /// The backend volunteers a snapshot at the head of every stream, and that
    /// remains the free path and the one §6 describes. This is for a front-end
    /// that has to show a figure before any turn has been made — a dashboard
    /// opened on a daemon that has been idle since it started.
    #[serde(default = "default_usage")]
    pub usage: String,
}

fn default_client_version() -> String {
    "2.0.0".to_owned()
}

fn default_effective_window_percent() -> f64 {
    95.0
}

fn default_endpoint() -> String {
    "https://chatgpt.com/backend-api/codex/responses".to_owned()
}

fn default_websocket() -> String {
    "wss://chatgpt.com/backend-api/codex/responses".to_owned()
}

fn default_catalog() -> String {
    "https://chatgpt.com/backend-api/codex/models".to_owned()
}

fn default_usage() -> String {
    "https://chatgpt.com/backend-api/wham/usage".to_owned()
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            client_version: default_client_version(),
            effective_window_percent: default_effective_window_percent(),
            endpoint: default_endpoint(),
            websocket: default_websocket(),
            catalog: default_catalog(),
            usage: default_usage(),
        }
    }
}

/// §2.1 — what the proxy adds around the client's system prompt.
///
/// The prompt the client sends is written for a different model and opens by
/// saying so. Nothing else in the request tells the model what it actually is,
/// and nothing in the client can be made to — `--append-system-prompt` reaches
/// the same `system` field, so it can add to that prompt but never precede it.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionsConfig {
    /// Lead with one line naming the model that is actually answering.
    ///
    /// On by default: a model told it is a different product is being given a
    /// false premise on every turn, and that is not a neutral default.
    #[serde(default = "default_identity")]
    pub identity: bool,
    /// Operator text placed after the system prompt, where an instruction has
    /// to be in order to take precedence over the prompt above it.
    ///
    /// It must be stable for the life of a conversation: anything varying per
    /// turn changes `instructions`, which costs every delta and every cache hit
    /// (§4.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append: Option<String>,
    /// Send the working budget of §2.1.
    ///
    /// On by default. This proxy is opinionated about it because the cost is
    /// measured: the conversation is replayed upstream every turn and echoed
    /// back three times, so broad reading spends the window fast.
    #[serde(default = "default_working_budget")]
    pub working_budget: bool,
}

/// §2.1 — the working budget, sent by default.
///
/// The premise is measured here, not borrowed: the whole conversation is
/// replayed upstream on every turn, and the backend echoes it back three times
/// per turn on top of that. Context pulled in is therefore paid for again on
/// every subsequent turn, and a read that did not change the next action is the
/// most expensive thing a turn can do.
///
/// This is on by default because the alternative was measured and is worse —
/// without it the window is spent quickly on reads that changed nothing.
///
/// **Written as decision rules, with no "always", "never", or "must".** Those
/// are reserved for real invariants; a shipped absolute that collides with the
/// client's own prompt destabilizes more than a missing detail would, and this
/// text sits underneath a prompt written for a different model that already
/// says a great deal.
const WORKING_BUDGET: &str = "\
# Working budget

This conversation is replayed in full on every turn, so anything pulled into \
context is paid for again on each turn that follows. Retrieval that did not \
change what you do next is the most expensive kind of waste here.

## Reading

Read the smallest slice that answers the question. Prefer a targeted search or \
a bounded line range over a whole file; read a file whole when most of it is \
needed, or when its structure is itself the question. What is already in \
context does not need reading again.

After a read, consider whether you can act. If you can, act. If a fact is still \
missing, name that fact and make one more targeted read for it.

## Tools and skills

Reach for a tool or a skill when its subject is what you are actually doing and \
it tells you something you would otherwise guess. One whose content you could \
predict from its description is rarely worth its cost. Having consulted one, \
prefer doing the work over collecting another.

These budgets take precedence over anything above that asks for broad reading \
or preemptive tool use before acting.";

fn default_identity() -> bool {
    true
}

fn default_working_budget() -> bool {
    true
}

impl Default for InstructionsConfig {
    fn default() -> Self {
        Self {
            identity: default_identity(),
            append: None,
            working_budget: default_working_budget(),
        }
    }
}

impl InstructionsConfig {
    /// The line that leads `instructions`, for the model actually answering.
    ///
    /// Names the client too, because "you are X" without saying where leaves
    /// the model to reconcile it with a harness prompt that says otherwise.
    /// Both halves are true, which is the only reason either is here.
    pub fn lead(&self, model: &str) -> Option<String> {
        self.identity.then(|| {
            format!(
                "You are {model}, answering through Claude Code, a terminal-based coding agent."
            )
        })
    }

    /// The working budget, when it is switched on.
    pub fn budget(&self) -> Option<&'static str> {
        self.working_budget.then_some(WORKING_BUDGET)
    }

    pub fn trailer(&self) -> Option<String> {
        self.append
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    }
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Config {
    /// The effort ceiling, if one is set and recognized.
    ///
    /// An unrecognized value is an error rather than a silent fallback: an
    /// operator who wrote `effort = "cheap"` meant to cap their spending, and
    /// quietly ignoring it spends their quota at full rate.
    pub fn effort_ceiling(
        &self,
    ) -> Result<Option<codex_cc_proxy_core::responses::Effort>, ProxyError> {
        let Some(effort) = &self.effort else {
            return Ok(None);
        };
        parse_effort(effort).map(Some)
    }

    /// Check the values that parse but cannot work.
    ///
    /// Refused rather than clamped. A clamp makes an operator's mistake look
    /// like it was accepted, and both ends of this range fail silently: zero
    /// advertises a window of nothing so every turn is refused for length, and
    /// over a hundred advertises more window than exists so the guard that was
    /// meant to catch that stops catching it.
    pub fn validate(&self) -> Result<(), ProxyError> {
        let percent = self.upstream.effective_window_percent;
        if !(percent > 0.0 && percent <= 100.0) {
            return Err(ProxyError::invalid_request(format!(
                "`upstream.effective_window_percent = {percent}` is not a usable share of a \
                 context window. It must be greater than 0 and at most 100."
            )));
        }
        Ok(())
    }

    /// Read the configuration, or report why it could not be read.
    ///
    /// A missing file is not an error — it is a first run, and the message says
    /// what to write and where. An unreadable one *is* an error: silently
    /// falling back to defaults would start a daemon that ignores what the
    /// operator wrote.
    pub fn load() -> Result<Self, ProxyError> {
        let path = config_path();

        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            // A missing file is not an error once every key has a default: it
            // is someone who has not needed to change anything yet. Where the
            // file would go is logged, because a default that cannot be found
            // is a default that cannot be changed.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    path = %path.display(),
                    "no configuration file; using defaults. Write one there to change them"
                );
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(ProxyError::invalid_request(format!(
                    "could not read {}: {error}",
                    path.display()
                )));
            }
        };

        toml::from_str(&raw).map_err(|error| {
            let hint = if error.to_string().contains("unknown field") {
                "\n\nA key in the wrong place reads as an unknown one. In TOML a bare \
                 key written after a table header belongs to that table, so a top-level \
                 setting has to sit above `[tiers]` and `[transport]`."
            } else {
                ""
            };
            ProxyError::invalid_request(format!("{} is not valid: {error}{hint}", path.display()))
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            tiers: Tiers::default(),
            transport: TransportConfig::default(),
            effort: None,
            instructions: InstructionsConfig::default(),
            upstream: UpstreamConfig::default(),
        }
    }
}

/// All four tiers must be mapped explicitly.
///
/// The client routes different work to different tiers, and background and
/// summarization traffic runs on the cheapest one. A defaulted mapping hides
/// which model handles that traffic and what it costs, so the mapping is stated
/// rather than inferred (§7.1).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tiers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sonnet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub haiku: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fable: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    #[serde(default = "yes")]
    pub websocket: bool,
    /// Compression on both transports.
    ///
    /// zstd on an HTTP body, announced with `Content-Encoding`, and
    /// `permessage-deflate` on the socket, offered during the upgrade and
    /// selected by the server. About two thirds off the wire in each direction.
    ///
    /// It saves bytes and never tokens. Turning it off is a supported thing to
    /// do and costs only bandwidth.
    #[serde(default = "yes")]
    pub compression: bool,
}

fn yes() -> bool {
    true
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            websocket: true,
            compression: true,
        }
    }
}

/// One tier, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTier {
    pub tier: &'static str,
    pub model: String,
    /// Whether this came from `DEFAULT_TIERS` rather than the configuration.
    ///
    /// A default is this proxy's guess about an account it has not seen; a
    /// stated model is the operator's decision. The catalog is allowed to
    /// overrule the first and never the second.
    pub defaulted: bool,
}

/// What each tier maps to when the configuration says nothing.
///
/// Stated rather than required. Demanding all four made the first run fail on a
/// file the operator had not written yet, and the concern it answered — that a
/// defaulted mapping hides which model serves background and summarization
/// traffic — is met better by `status`, which prints the mapping in use
/// whether or not it was written down.
///
/// `WebFetch` runs on the haiku tier, so a defaulted haiku is the one that most
/// needs to exist: unmapped, it breaks in a way that looks unrelated.
///
/// These are model ids, so they go stale. A default naming a retired model is
/// refused at startup by catalog validation, with the error saying what exists.
/// One effort name, or an error naming every value that would have worked.
///
/// An unrecognized value is an error rather than a silent fallback: an operator
/// who wrote `effort = "cheap"` meant to cap their spending, and quietly
/// ignoring it spends their quota at full rate.
pub fn parse_effort(effort: &str) -> Result<codex_cc_proxy_core::responses::Effort, ProxyError> {
    codex_cc_proxy_core::responses::Effort::parse(effort).ok_or_else(|| {
        ProxyError::invalid_request(format!(
            "`{effort}` is not a recognized effort. \
             One of: none, minimal, low, medium, high, xhigh, max."
        ))
    })
}

/// The four tier names, in the order they are reported.
///
/// Named once so an error can list them rather than describing them.
pub const TIER_NAMES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

const DEFAULT_TIERS: [(&str, &str); 4] = [
    ("opus", "gpt-5.6-terra"),
    ("sonnet", "gpt-5.6-luna"),
    ("haiku", "gpt-5.6-luna"),
    ("fable", "gpt-5.6-sol"),
];

impl Tiers {
    /// Resolve all four, defaulting the ones the configuration left out.
    ///
    /// A value that is present but blank is refused rather than defaulted. An
    /// omission is someone accepting the shipped answer; a blank is a mistake,
    /// and quietly replacing it would hide the mistake instead of naming it.
    pub fn resolve(&self) -> Result<Vec<ResolvedTier>, ProxyError> {
        let entries = [
            ("opus", &self.opus),
            ("sonnet", &self.sonnet),
            ("haiku", &self.haiku),
            ("fable", &self.fable),
        ];

        let blank: Vec<&str> = entries
            .iter()
            .filter(|(_, model)| model.as_ref().is_some_and(|model| model.trim().is_empty()))
            .map(|(tier, _)| *tier)
            .collect();

        if !blank.is_empty() {
            return Err(ProxyError::invalid_request(format!(
                "these tiers are mapped to an empty value: {}. Give each a model id, \
                 or remove the line to take the default.",
                blank.join(", ")
            )));
        }

        let resolved: Vec<ResolvedTier> = entries
            .iter()
            .map(|(tier, model)| ResolvedTier {
                tier,
                defaulted: model.is_none(),
                model: (*model).clone().unwrap_or_else(|| {
                    DEFAULT_TIERS
                        .iter()
                        .find(|(name, _)| name == tier)
                        .map_or_else(String::new, |(_, model)| (*model).to_owned())
                }),
            })
            .collect();

        // §7.2 — a `[1m]` marker makes the client believe it has roughly four
        // times the headroom it has, and auto-compaction would never fire
        // before the window overran.
        if let Some(marked) = resolved.iter().find(|tier| tier.model.contains("[1m]")) {
            return Err(ProxyError::invalid_request(format!(
                "tier `{}` maps to `{}`, which carries a [1m] marker: the client \
                 would assume a million-token window and never compact in time",
                marked.tier, marked.model
            )));
        }

        Ok(resolved)
    }
}
