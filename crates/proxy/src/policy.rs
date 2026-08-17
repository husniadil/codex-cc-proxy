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
use codex_cc_proxy_core::responses::Effort;
use std::sync::Arc;
use std::sync::RwLock;

/// Everything a turn needs to know about operator policy, as one value.
///
/// Read together and replaced together: a reader that took the tiers and the
/// ceiling in two calls could see one from before a change and one from after.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub tiers: Vec<ResolvedTier>,
    /// Tier name to upstream model id, which is what the ingress routes on.
    /// Derived from `tiers` rather than stored beside it, so the two cannot
    /// disagree.
    pub models: Vec<ModelMapping>,
    pub effort_ceiling: Option<Effort>,
}

impl Snapshot {
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
        let ceiling = self.get().effort_ceiling;
        self.replace(Snapshot::new(tiers, ceiling));
    }

    pub fn set_effort_ceiling(&self, ceiling: Option<Effort>) {
        let tiers = self.get().tiers.clone();
        self.replace(Snapshot::new(tiers, ceiling));
    }

    fn replace(&self, snapshot: Snapshot) {
        match self.current.write() {
            Ok(mut current) => *current = Arc::new(snapshot),
            Err(poisoned) => *poisoned.into_inner() = Arc::new(snapshot),
        }
    }
}
