//! `docs/api.md` §3 — the parts of the configuration a running daemon can change.
//!
//! The tier mapping and the effort ceiling were read once at startup and copied
//! into two places: the control socket answered `status` and `env` from one, the
//! ingress routed turns with the other. Nothing could change either without a
//! restart, and nothing needed to.
//!
//! A front-end changes that. Setting a mapping over the socket has to move the
//! copy that *routes turns*, or the daemon would report one mapping and serve
//! another — a divergence that produces working turns against the wrong model,
//! which is the failure this project refuses everywhere else.
//!
//! **The file stays the source of truth at startup.** A runtime change is
//! written back to it where the caller asks for that, and where it is not, the
//! change lasts until the daemon stops. Both are stated to the caller rather
//! than left to be discovered.
//!
//! **A turn in flight keeps the mapping it started with.** Readers take a
//! snapshot, so a set that lands mid-turn cannot change the model a request is
//! already being translated for. A client that was handed
//! `ANTHROPIC_DEFAULT_*_MODEL` at spawn keeps asking for that id until it is
//! restarted, which is the same lifetime the ids always had.

use crate::config::ResolvedTier;
use crate::ingress::ModelMapping;
use proxenos_core::responses::Effort;
use std::sync::Arc;
use std::sync::RwLock;

/// Everything a turn needs to know about operator policy, as one value.
///
/// Read together and replaced together: a reader that took the tiers and the
/// ceiling in two calls could see one from before a change and one from after.
///
/// **The fields are private on purpose.** `models` is derived from `tiers`, and
/// the two must never be set independently — `status` reports the tiers while
/// the ingress routes on the models, so a snapshot where they disagree is a
/// daemon that reports one mapping and serves another. Constructing one is
/// therefore going through `new`, which derives the pair, or `routing_only`,
/// which says in its name that there are no tiers to report.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    tiers: Vec<ResolvedTier>,
    models: Vec<ModelMapping>,
    effort_ceiling: Option<Effort>,
}

impl Snapshot {
    /// A snapshot from the tier mapping, with the routing table derived from it.
    pub fn new(tiers: Vec<ResolvedTier>, effort_ceiling: Option<Effort>) -> Self {
        let models = tiers
            .iter()
            .map(|tier| ModelMapping {
                requested: tier.tier.to_owned(),
                upstream: tier.model.clone(),
            })
            .collect();
        Self {
            tiers,
            models,
            effort_ceiling,
        }
    }

    /// A snapshot that routes but has no tier mapping to report.
    ///
    /// For the probes and for tests, which route a fixed pair of ids without an
    /// operator's configuration behind them. Named rather than reachable by
    /// filling in a field, so a caller that wanted `new` cannot arrive here by
    /// accident and leave `status` reporting nothing.
    pub fn routing_only(models: Vec<ModelMapping>, effort_ceiling: Option<Effort>) -> Self {
        Self {
            tiers: Vec::new(),
            models,
            effort_ceiling,
        }
    }

    pub fn tiers(&self) -> &[ResolvedTier] {
        &self.tiers
    }

    /// Tier name to upstream model id, which is what the ingress routes on.
    pub fn models(&self) -> &[ModelMapping] {
        &self.models
    }

    pub fn effort_ceiling(&self) -> Option<Effort> {
        self.effort_ceiling
    }
}

/// The live policy, shared by the ingress and the control socket.
#[derive(Debug)]
pub struct Policy {
    current: RwLock<Arc<Snapshot>>,
}

impl Policy {
    pub fn new(snapshot: Snapshot) -> Self {
        Self {
            current: RwLock::new(Arc::new(snapshot)),
        }
    }

    /// What policy is, right now.
    ///
    /// A poisoned lock cannot happen here — nothing panics while holding it —
    /// but if it somehow did, refusing to answer would take the daemon down
    /// over a value it can still read.
    pub fn get(&self) -> Arc<Snapshot> {
        match self.current.read() {
            Ok(current) => Arc::clone(&current),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    pub fn set_tiers(&self, tiers: Vec<ResolvedTier>) {
        self.update(|current| Snapshot::new(tiers, current.effort_ceiling));
    }

    pub fn set_effort_ceiling(&self, ceiling: Option<Effort>) {
        self.update(|current| Snapshot::new(current.tiers.clone(), ceiling));
    }

    /// Read and replace under one lock.
    ///
    /// Each setter changes one field and has to carry the other across. Reading
    /// it through `get()` and writing through a second call leaves a window
    /// where the other field moves in between, and the write puts the stale
    /// value back.
    ///
    /// `tests/policy.rs` demonstrates it: one thread changes the ceiling
    /// exactly once while another spins on the mapping, and the ceiling that
    /// was set comes back absent. Two threads writing the *same* value do not
    /// show it — a stale read is indistinguishable from a fresh one there —
    /// which is why the detector changes a field once rather than in a loop.
    fn update(&self, next: impl FnOnce(&Snapshot) -> Snapshot) {
        match self.current.write() {
            Ok(mut current) => *current = Arc::new(next(&current)),
            Err(poisoned) => {
                let mut current = poisoned.into_inner();
                *current = Arc::new(next(&current));
            }
        }
    }
}
