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

[tiers]
opus   = "gpt-5-codex"
sonnet = "gpt-5-codex"
haiku  = "gpt-5-codex-mini"
fable  = "gpt-5-codex-mini"
"#;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub tiers: Tiers,
    #[serde(default)]
    pub transport: TransportConfig,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Config {
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
            ProxyError::invalid_request(format!("{} is not valid: {error}", path.display()))
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            tiers: Tiers::default(),
            transport: TransportConfig::default(),
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
pub struct TransportConfig {
    #[serde(default = "yes")]
    pub websocket: bool,
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
