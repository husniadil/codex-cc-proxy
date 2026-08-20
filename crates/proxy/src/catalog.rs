//! `docs/proxy-behavior.md` §7.0 — the model catalog.

use crate::config::ResolvedTier;
use crate::error::ProxyError;

/// Which model to fall back to when a shipped default is unavailable, in order
/// of preference. The workhorse first: it is the one most accounts have.
const DEFAULT_PREFERENCE: [&str; 3] = ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.5"];
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;

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
    /// Efforts this model accepts. Empty where the catalog said nothing, which
    /// is not the same as "none are supported".
    pub efforts: Vec<String>,
}

impl Model {
    /// The most this model will accept.
    ///
    /// Models differ: some stop at `xhigh` and some go to `max`. Sending one
    /// more than it supports fails the turn, and the client cannot know which
    /// model it is talking to — it asked for a tier.
    pub fn highest_effort(&self) -> Option<codex_cc_proxy_core::responses::Effort> {
        self.efforts
            .iter()
            .filter_map(|effort| codex_cc_proxy_core::responses::Effort::parse(effort))
            .max()
    }

    /// Every effort this model accepts, as the translation understands them.
    ///
    /// Levels this proxy has no name for are dropped rather than guessed at:
    /// an effort it cannot represent is one it could not have sent anyway.
    pub fn supported_efforts(&self) -> Vec<codex_cc_proxy_core::responses::Effort> {
        self.efforts
            .iter()
            .filter_map(|effort| codex_cc_proxy_core::responses::Effort::parse(effort))
            .collect()
    }

    /// The window the guard actually enforces.
    ///
    /// `effective_percent` is resolved at parse time — either the share the
    /// catalog stated for this model, or the configured default where it stated
    /// none. There is no compiled-in figure left to fall back to, which is what
    /// stops the configured one from being quietly ignored.
    pub fn effective_window(&self) -> Option<u64> {
        let window = self.context_window?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some((window as f64 * self.effective_percent? / 100.0) as u64)
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
    /// What the backend actually sends: `list` to offer, `hide` to withhold.
    /// A boolean flag was the wrong shape, and the wrong shape read as
    /// "visible" for every entry — including the ones marked hidden.
    #[serde(default)]
    visibility: Option<String>,
    /// Efforts this model accepts, as `{"effort": "low", ...}` entries. A
    /// ceiling naming an effort the model does not support is a request that
    /// fails, so it is worth knowing what is on offer.
    #[serde(default)]
    supported_reasoning_levels: Vec<ReasoningLevel>,
}

#[derive(Debug, Deserialize)]
struct ReasoningLevel {
    #[serde(default)]
    effort: Option<String>,
}

/// What is known about the available models.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    models: BTreeMap<String, Model>,
    /// Whether this came from the backend or from the fallback list. Fetch
    /// failure is not the same claim as absence, and validation that depends on
    /// the catalog is skipped when it is unavailable rather than failed.
    pub authoritative: bool,
    /// The account this list was fetched for, where it was fetched at all.
    ///
    /// A catalog is one account's menu: the plan decides which models appear
    /// and which efforts each one offers. Nothing refetches it when the daemon
    /// starts serving a different account, so the attribution is what lets an
    /// answer say the list belongs to somebody else instead of presenting it
    /// as this account's. `None` means unattributed — a fallback list, or one
    /// parsed directly — and is never stale, because it never claimed an
    /// account in the first place.
    pub fetched_for: Option<String>,
}

impl Catalog {
    /// The list used when a fetch fails.
    ///
    /// Ids only. A fallback entry states no window, so the guard does not fire
    /// for it — the daemon starts and reports honestly rather than blocking on
    /// an unreachable catalog or inventing figures it does not have.
    ///
    /// The list needs updating when models are renamed or retired, and goes
    /// stale silently: nothing here can tell that an id has stopped existing.
    /// That is why it is only ever a fallback, and why the catalog it stands in
    /// for is marked non-authoritative when it is used.
    pub fn fallback() -> Self {
        let models = ["gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.4-mini"]
            .into_iter()
            .map(|id| {
                (
                    id.to_owned(),
                    Model {
                        id: id.to_owned(),
                        context_window: None,
                        effective_percent: None,
                        visible: true,
                        efforts: Vec::new(),
                    },
                )
            })
            .collect();

        Self {
            models,
            authoritative: false,
            fetched_for: None,
        }
    }

    /// Read a catalog, applying `default_percent` to every entry that stated no
    /// share of its own.
    ///
    /// The share is resolved here rather than at the point of use so there is
    /// only one place it can come from. A model whose entry states a percentage
    /// keeps it: that is the backend describing its own model, and the
    /// configured value is a default, not an override.
    /// Attribute this list to the account it was fetched for.
    pub fn fetched_for(mut self, account_id: String) -> Self {
        self.fetched_for = Some(account_id);
        self
    }

    /// Whether this list describes an account other than the one named.
    pub fn is_stale_for(&self, account_id: Option<&str>) -> bool {
        match (&self.fetched_for, account_id) {
            (Some(fetched), Some(serving)) => fetched != serving,
            // Unattributed, or nothing to compare it against: it makes no
            // claim about an account, so it cannot be wrong about one.
            _ => false,
        }
    }

    pub fn parse(body: &str, default_percent: f64) -> Result<Self, ProxyError> {
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
                    effective_percent: Some(
                        entry
                            .effective_context_window_percent
                            .unwrap_or(default_percent),
                    ),
                    visible: match (entry.is_visible, entry.visibility.as_deref()) {
                        (Some(visible), _) => visible,
                        (None, Some(visibility)) => visibility != "hide",
                        // Said nothing either way. Offering it is the safer
                        // default: withholding a model the operator can use is
                        // a worse error than listing one they cannot.
                        (None, None) => true,
                    },
                    efforts: entry
                        .supported_reasoning_levels
                        .into_iter()
                        .filter_map(|level| level.effort)
                        .collect(),
                    id: id.clone(),
                };
                Some((id, model))
            })
            .collect();

        Ok(Self {
            models,
            authoritative: true,
            fetched_for: None,
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

    /// Mapped models the catalog knows but withholds.
    ///
    /// `validate` asks a different question — whether the id exists at all —
    /// and a hidden entry exists, so a tier mapped onto one starts cleanly and
    /// then never appears among the models on offer. That is worth saying out
    /// loud rather than refusing: the backend may well still serve it, and an
    /// operator who mapped it deliberately should not be blocked.
    ///
    /// An unknown id is absent from this list. It is not withheld, it is
    /// unknown, and `validate` is where that is reported.
    pub fn unlisted(&self, mapped: &[String]) -> Vec<String> {
        if !self.authoritative {
            return Vec::new();
        }

        mapped
            .iter()
            .filter(|id| self.models.get(*id).is_some_and(|model| !model.visible))
            .cloned()
            .collect()
    }

    /// Replace defaulted models this account cannot see.
    ///
    /// A shipped default is a guess about an account this proxy has never seen.
    /// `gpt-5.6-sol` is plan-gated and absent from a free account's catalog, so
    /// a default naming it would refuse to start for most people — and a
    /// default that cannot start is worse than no default. The same happens
    /// whenever a model is renamed or retired out from under a released binary.
    ///
    /// A model the operator stated is never touched. They may know something
    /// this catalog does not, and quietly serving a different model than the
    /// one asked for is worse than refusing; `validate` is what speaks to that.
    ///
    /// Returns the tiers that were changed, so the caller can say so.
    pub fn substitute_unavailable_defaults(&self, tiers: &mut [ResolvedTier]) -> Vec<String> {
        if !self.authoritative {
            return Vec::new();
        }

        // Prefer another default that this account does have, so the
        // substitution stays close to the intended shape; otherwise anything
        // the catalog offers is better than a model that is not there.
        let Some(replacement) = DEFAULT_PREFERENCE
            .iter()
            .find(|id| self.models.get(**id).is_some_and(|model| model.visible))
            .map(|id| (*id).to_owned())
            .or_else(|| self.selectable().first().map(|model| model.id.clone()))
        else {
            return Vec::new();
        };

        let mut swapped = Vec::new();
        for tier in tiers.iter_mut() {
            if tier.defaulted && !self.models.contains_key(&tier.model) {
                tracing::warn!(
                    tier = tier.tier,
                    wanted = %tier.model,
                    using = %replacement,
                    "this account's catalog has no such model; the default was substituted"
                );
                tier.model = replacement.clone();
                swapped.push(tier.tier.to_owned());
            }
        }
        swapped
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

        // Deduplicated: four tiers pointing at one missing model is one
        // problem, and naming it four times buries whatever else is wrong.
        let unknown: Vec<&str> = mapped
            .iter()
            .map(String::as_str)
            .filter(|id| !self.models.contains_key(*id))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        if unknown.is_empty() {
            return Ok(());
        }

        // An authoritative catalog with nothing in it is not an account with no
        // models — it is almost always a client version the backend considers
        // too old to be told about any. It returns an empty list rather than an
        // error, so nothing upstream of here says so.
        if self.models.is_empty() {
            return Err(ProxyError::invalid_request(format!(
                "the backend returned an empty model catalog, so no mapping can be validated \
                 (asked for: {}).\n\nThis is usually `upstream.client_version` in config.toml \
                 being older than every model requires — the backend answers a version it \
                 considers too old with an empty list rather than an error.",
                unknown.join(", ")
            )));
        }

        Err(ProxyError::invalid_request(format!(
            "these mapped models are not in the catalog: {}. Available: {}",
            unknown.join(", "),
            self.ids().join(", ")
        )))
    }
}

/// Fetch the catalog, falling back rather than failing.
///
/// The identity headers of §2.8 are not only for the responses endpoint. This
/// request is rejected without them, and the rejection is a bare 400 that says
/// nothing about which header is missing.
/// Ask the backend for the model list.
///
/// `None` where it could not be had at all, so a caller can tell a failed
/// fetch from a list that came back empty — the two mean opposite things
/// (§7.0), and only one of them is a reason to keep what is already in force.
pub async fn fetch(
    client: &reqwest::Client,
    endpoint: &str,
    authorization: &crate::auth::authorize::Authorization,
    client_version: &str,
    default_percent: f64,
) -> Option<Catalog> {
    let request = client
        .get(endpoint)
        // Required, and its absence is a bare 400 that names nothing. The
        // backend also filters the list by it: each entry declares a minimum
        // client version, and a version below every minimum returns an empty
        // catalog rather than an error — which reads exactly like an account
        // with no models.
        .query(&[("client_version", client_version)])
        .header(
            axum::http::header::USER_AGENT,
            crate::upstream::http::USER_AGENT,
        );
    let request = authorization.apply(request);

    let attempt = request
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

    let catalog = body
        .as_deref()
        .and_then(|body| Catalog::parse(body, default_percent).ok());

    let account_id = authorization
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
        .map(|(_, value)| value.as_str());

    match (catalog, account_id) {
        // Attributed only when it really came back from the backend for a
        // named account. A fallback list describes no account.
        (Some(catalog), Some(account)) => Some(catalog.fetched_for(account.to_owned())),
        (Some(catalog), None) => Some(catalog),
        (None, _) => None,
    }
}

/// The catalog in force, and what it takes to fetch another.
///
/// A catalog is one account's menu (§7.0), so it stops describing what this
/// daemon serves the moment the daemon serves a different account. Held in a
/// slot rather than as a value so a switch can replace it, and shared with the
/// ingress so a replacement moves what *routes turns* rather than only what the
/// control socket reports.
pub struct CatalogSource {
    current: std::sync::RwLock<Arc<Catalog>>,
    /// One endpoint per credential kind. Held as a pair rather than resolved
    /// once, because the account serving turns can change kind while the
    /// daemon runs — and a list fetched from the other kind's host would send
    /// the credential somewhere it was never issued for. Empty where there is
    /// nothing to refetch from: a fixed catalog, which is what tests and the
    /// probe path hold.
    subscription: String,
    key: String,
    client_version: String,
    default_percent: f64,
}

impl CatalogSource {
    pub fn new(
        catalog: Catalog,
        subscription: impl Into<String>,
        key: impl Into<String>,
        client_version: impl Into<String>,
        default_percent: f64,
    ) -> Self {
        Self {
            current: std::sync::RwLock::new(Arc::new(catalog)),
            subscription: subscription.into(),
            key: key.into(),
            client_version: client_version.into(),
            default_percent,
        }
    }

    /// A catalog that never changes, for callers with nothing to refetch from.
    pub fn fixed(catalog: Catalog) -> Self {
        Self::new(catalog, String::new(), String::new(), String::new(), 0.0)
    }

    /// Where a credential of this kind asks for its list.
    fn endpoint(&self, kind: crate::auth::authorize::Kind) -> &str {
        match kind {
            crate::auth::authorize::Kind::Subscription => &self.subscription,
            crate::auth::authorize::Kind::Key => &self.key,
        }
    }

    /// The catalog in force. Cloned rather than borrowed: a reader holding a
    /// lock across a turn would block the switch it is racing with, and a
    /// turn keeps the catalog it started with either way.
    pub fn current(&self) -> Arc<Catalog> {
        match self.current.read() {
            Ok(current) => Arc::clone(&current),
            // A poisoned lock means a writer panicked mid-replacement. The
            // value is still a whole catalog, so this reads it rather than
            // failing a turn over it.
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Fetch the catalog for this account and put it in force.
    ///
    /// **A failed fetch keeps what is already there.** Fetch failure is not
    /// evidence that a model went away (§7.1), and replacing a real list with
    /// the fallback on a network blink would withdraw models the account has.
    /// The answer says which happened rather than leaving it to be discovered.
    pub async fn refresh(&self, authorization: &crate::auth::authorize::Authorization) -> bool {
        // The endpoint this credential belongs to, chosen from the credential
        // rather than from whichever kind was selected when the daemon
        // started. The pairing is structural here: there is no argument that
        // could cross it.
        let endpoint = self.endpoint(authorization.kind);
        if endpoint.is_empty() {
            return false;
        }

        let fetched = fetch(
            &reqwest::Client::new(),
            endpoint,
            authorization,
            &self.client_version,
            self.default_percent,
        )
        .await;

        let Some(catalog) = fetched else {
            tracing::warn!("could not refetch the model catalog; keeping the list in force");
            return false;
        };

        match self.current.write() {
            Ok(mut current) => *current = Arc::new(catalog),
            Err(poisoned) => *poisoned.into_inner() = Arc::new(catalog),
        }
        true
    }
}
