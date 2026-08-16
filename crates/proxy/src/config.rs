//! `docs/api.md` §4 — configuration.
//!
//! Credentials are never stored here.

use crate::error::ProxyError;
use serde::Deserialize;
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
pub const EXAMPLE: &str = r#"port = 8787

# Optional. Caps reasoning effort on every request, whatever the client asks
# for: one of none, minimal, low, medium, high, xhigh, max.
#
# Both keys sit above the tables on purpose. In TOML a bare key written after a
# table header belongs to that table, so `effort` placed below `[tiers]` is
# `tiers.effort` — a different setting entirely.
# effort = "low"

[tiers]
opus   = "gpt-5-codex"
sonnet = "gpt-5-codex"
haiku  = "gpt-5-codex-mini"
fable  = "gpt-5-codex-mini"

[transport]
websocket   = true

# zstd on the HTTP body. Not the WebSocket, where compression is negotiated in
# the upgrade rather than chosen here.
compression = true
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
        codex_cc_proxy_core::responses::Effort::parse(effort)
            .map(Some)
            .ok_or_else(|| {
                ProxyError::invalid_request(format!(
                    "`effort = \"{effort}\"` is not a recognized effort. \
                     One of: none, minimal, low, medium, high, xhigh, max."
                ))
            })
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProxyError::invalid_request(format!(
                    "no configuration at {}.\n\nWrite one first:\n\n{}\nAll four tiers are \
                     required. WebFetch runs on the haiku tier, so leaving it unmapped breaks \
                     WebFetch in a way that looks unrelated.",
                    path.display(),
                    EXAMPLE
                )));
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
    /// zstd on the HTTP body, announced with `Content-Encoding`.
    ///
    /// It does not apply to the WebSocket transport: compression there is
    /// `permessage-deflate`, negotiated in the upgrade, which this client
    /// cannot yet offer.
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
}

impl Tiers {
    /// Resolve all four, or say which are missing.
    ///
    /// `WebFetch` runs on the haiku tier, so an unmapped haiku breaks it in a
    /// way that looks unrelated to tier mapping. Refusing to start is the
    /// loudest available failure and the only one that points at the cause.
    pub fn resolve(&self) -> Result<Vec<ResolvedTier>, ProxyError> {
        let entries = [
            ("opus", &self.opus),
            ("sonnet", &self.sonnet),
            ("haiku", &self.haiku),
            ("fable", &self.fable),
        ];

        let missing: Vec<&str> = entries
            .iter()
            .filter(|(_, model)| model.as_ref().is_none_or(|model| model.trim().is_empty()))
            .map(|(tier, _)| *tier)
            .collect();

        if !missing.is_empty() {
            return Err(ProxyError::invalid_request(format!(
                "every tier must be mapped explicitly; missing: {}",
                missing.join(", ")
            )));
        }

        let resolved: Vec<ResolvedTier> = entries
            .iter()
            .filter_map(|(tier, model)| {
                model.as_ref().map(|model| ResolvedTier {
                    tier,
                    model: model.clone(),
                })
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
