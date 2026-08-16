//! The recorded-exchange format the suite replays.
//!
//! A fixture is evidence. Where it came from determines how much weight it
//! carries, so provenance is a required field rather than a comment: a reader
//! must never have to guess whether a shape was observed or invented.

use serde::Deserialize;
use serde::Serialize;

/// One recorded exchange: what the client sent, and what the backend said back.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Fixture {
    /// Stable identifier, matching the file stem.
    pub name: String,
    /// The capability in `docs/proxy-behavior.md` §1 this exercises.
    pub capability: Capability,
    pub provenance: Provenance,
    /// Why this fixture exists, and for a derived one, what it was derived
    /// from.
    pub note: String,
    /// The inbound Messages request.
    pub request: serde_json::Value,
    /// The upstream stream, one event per entry, in order.
    #[serde(default)]
    pub upstream: Vec<serde_json::Value>,
}

/// Where a fixture's content came from, in descending order of authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Restated from the upstream protocol definitions. Not a guess — the
    /// contract, in another form.
    Derived,
    /// Captured from a live exchange.
    Captured,
    /// Written by hand for a shape neither source covers. The weakest evidence
    /// there is, and it says so.
    Authored,
}

/// The capabilities of `docs/proxy-behavior.md` §1, each of which fails
/// silently rather than loudly when a translator gets it wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    ReadImage,
    ReadDocument,
    WebSearch,
    WebFetch,
    ToolSearch,
    ContextMeter,
    CountTokens,
    ToolCalling,
}

impl Capability {
    /// Every capability the corpus must cover.
    pub const ALL: [Self; 8] = [
        Self::ReadImage,
        Self::ReadDocument,
        Self::WebSearch,
        Self::WebFetch,
        Self::ToolSearch,
        Self::ContextMeter,
        Self::CountTokens,
        Self::ToolCalling,
    ];
}
