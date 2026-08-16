//! `docs/proxy-behavior.md` §7.0 — the model catalog.

use crate::error::ProxyError;
use serde::Deserialize;
use std::collections::BTreeMap;

/// The share of a context window left usable after instructions, tool overhead,
/// and output are accounted for. Applied where an entry states no percentage of
/// its own.
const DEFAULT_EFFECTIVE_PERCENT: f64 = 95.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub id: String,
    /// `None` where the catalog stated no window.
    ///
    /// Unknown, not assumed. A guessed window either rejects requests that
    /// would have worked or forwards ones that cannot, and both are worse than
    /// declining to guess.
    pub context_window: Option<u64>,
    pub effective_percent: Option<f64>,
    pub visible: bool,
}

impl Model {
    /// The window the guard actually enforces.
    pub fn effective_window(&self) -> Option<u64> {
        let window = self.context_window?;
        let percent = self.effective_percent.unwrap_or(DEFAULT_EFFECTIVE_PERCENT);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some((window as f64 * percent / 100.0) as u64)
    }
}

#[derive(Debug, Deserialize)]
struct CatalogResponse {
    #[serde(default)]
    data: Vec<CatalogEntry>,
    #[serde(default)]
    models: Vec<CatalogEntry>,
}

#[derive(Debug, Deserialize)]
struct CatalogEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    /// A ceiling the account may not have. Where both are present the
    /// smaller-scoped `context_window` wins.
    #[serde(default)]
    max_context_window: Option<u64>,
    #[serde(default)]
    effective_context_window_percent: Option<f64>,
    #[serde(default)]
    is_visible: Option<bool>,
}

/// What is known about the available models.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    models: BTreeMap<String, Model>,
    /// Whether this came from the backend or from the fallback list. Fetch
    /// failure is not the same claim as absence, and validation that depends on
    /// the catalog is skipped when it is unavailable rather than failed.
    pub authoritative: bool,
}

impl Catalog {
    /// The list used when a fetch fails.
    ///
    /// Ids only. A fallback entry states no window, so the guard does not fire
    /// for it — the daemon starts and reports honestly rather than blocking on
    /// an unreachable catalog or inventing figures it does not have.
    pub fn fallback() -> Self {
        let models = ["gpt-5-codex", "gpt-5-codex-mini", "gpt-5", "gpt-5-mini"]
            .into_iter()
            .map(|id| {
                (
                    id.to_owned(),
                    Model {
                        id: id.to_owned(),
                        context_window: None,
                        effective_percent: None,
                        visible: true,
                    },
                )
            })
            .collect();

        Self {
            models,
            authoritative: false,
        }
    }

    pub fn parse(body: &str) -> Result<Self, ProxyError> {
        let response: CatalogResponse = serde_json::from_str(body).map_err(|error| {
            ProxyError::upstream(
                axum::http::StatusCode::BAD_GATEWAY,
                format!("unreadable model catalog: {error}"),
            )
        })?;

        let entries = if response.data.is_empty() {
            response.models
        } else {
            response.data
        };

        let models = entries
            .into_iter()
            .filter_map(|entry| {
                let id = entry.id.or(entry.slug)?;
                let model = Model {
                    // Where both windows are present the smaller-scoped one is
                    // authoritative: the maximum describes a ceiling this
                    // account may not have.
                    context_window: entry.context_window.or(entry.max_context_window),
                    effective_percent: entry.effective_context_window_percent,
                    visible: entry.is_visible.unwrap_or(true),
                    id: id.clone(),
                };
                Some((id, model))
            })
            .collect();

        Ok(Self {
            models,
            authoritative: true,
        })
    }

    pub fn get(&self, id: &str) -> Option<&Model> {
        self.models.get(id)
    }

    /// The models offered for mapping.
    ///
    /// Hidden entries are excluded from what is offered, but their window
    /// metadata is retained: a session may reference a model the picker filters
    /// out, and knowing its window is better than not.
    pub fn selectable(&self) -> Vec<&Model> {
        self.models.values().filter(|model| model.visible).collect()
    }

    pub fn ids(&self) -> Vec<&str> {
        self.models.keys().map(String::as_str).collect()
    }

    /// Check a tier mapping against the catalog.
    ///
    /// An unreachable catalog skips validation rather than failing it. Fetch
    /// failure is not evidence that a model went away, and refusing to start
    /// because the network was briefly unavailable is a worse failure than
    /// starting with an unvalidated mapping.
    pub fn validate(&self, mapped: &[String]) -> Result<(), ProxyError> {
        if !self.authoritative {
            tracing::warn!("model catalog unavailable; tier mapping was not validated");
            return Ok(());
        }

        let unknown: Vec<&str> = mapped
            .iter()
            .map(String::as_str)
            .filter(|id| !self.models.contains_key(*id))
            .collect();

        if unknown.is_empty() {
            return Ok(());
        }

        Err(ProxyError::invalid_request(format!(
            "these mapped models are not in the catalog: {}. Available: {}",
            unknown.join(", "),
            self.ids().join(", ")
        )))
    }
}

/// Fetch the catalog, falling back rather than failing.
pub async fn fetch(client: &reqwest::Client, endpoint: &str, token: &str) -> Catalog {
    let attempt = client
        .get(endpoint)
        .bearer_auth(token)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status);

    let body = match attempt {
        Ok(response) => response.text().await.ok(),
        Err(error) => {
            tracing::warn!(%error, "could not fetch the model catalog; using the fallback list");
            None
        }
    };

    body.as_deref()
        .and_then(|body| Catalog::parse(body).ok())
        .unwrap_or_else(Catalog::fallback)
}
