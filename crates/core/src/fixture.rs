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

/// The capabilities of `docs/proxy-behavior.md` §1, plus the transport path of
/// §9 — each of which fails silently rather than loudly when the proxy gets it
/// wrong.
///
/// §9 is not a harness capability and is not listed in §1's table. It is here
/// because it shares §1's defining property and the corpus's rule: a path whose
/// mistake still returns 200 needs an exchange proving it, or nothing catches
/// the mistake.
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
    /// A turn forwarded rather than translated (§9). Its silent failure is a
    /// body round-tripped through this proxy's own types, which drops every
    /// field they do not model somewhere no test looks.
    Relay,
    /// The launch surface's load-bearing variables (`docs/api.md` §2.2). The
    /// only capability here proven by rendering rather than by an exchange:
    /// there is no turn to record, because what breaks is what the client is
    /// launched with. Deliberately absent from `ALL` for that reason — a
    /// corpus fixture for it would be a recording of nothing.
    EnvContract,
}

impl Capability {
    /// Every capability the corpus must cover.
    pub const ALL: [Self; 9] = [
        Self::ReadImage,
        Self::ReadDocument,
        Self::WebSearch,
        Self::WebFetch,
        Self::ToolSearch,
        Self::ContextMeter,
        Self::CountTokens,
        Self::ToolCalling,
        Self::Relay,
    ];
}
