//! `docs/proxy-behavior.md` §3.2 — per-conversation state.
//!
//! Claude Code sends no session identifier, so identity is derived from
//! content: a request belongs to an existing session when its input is a strict
//! extension of that session's baseline (§3.1). The same predicate governs
//! incremental upload, so matching and delta computation cannot disagree.

use crate::estimate::CalibratedEstimator;
use codex_cc_proxy_core::responses::InputItem;
use codex_cc_proxy_core::session::Baseline;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;

/// How many conversations to keep. Eviction is by least recent use, never by
/// refusing a request: a session store that fills up must degrade to
/// full sends, not to errors.
const CAPACITY: usize = 64;

pub struct Session {
    pub baseline: Mutex<Baseline>,
    pub estimator: CalibratedEstimator,
    /// Tools this conversation has seen discovered (§2.5).
    pub discovered_tools: Mutex<BTreeSet<String>>,
    /// A stable key for the life of the conversation. Cache hit rate depends on
    /// it directly (§2.7).
    pub cache_key: String,
}

impl Session {
    fn new(cache_key: String) -> Self {
        Self {
            baseline: Mutex::new(Baseline::new()),
            estimator: CalibratedEstimator::new(),
            discovered_tools: Mutex::new(BTreeSet::new()),
            cache_key,
        }
    }

    pub fn discovered(&self) -> BTreeSet<String> {
        self.discovered_tools
            .lock()
            .map(|names| names.clone())
            .unwrap_or_default()
    }

    /// Record what this turn sent, so the next turn is measured against it.
    ///
    /// Without this the baseline stays empty, and an empty baseline extends
    /// into anything — every conversation would resolve to the first session
    /// and inherit its calibration.
    ///
    /// Items the server added are folded in by the incremental path, which
    /// needs them for the same reason (§3.3).
    pub fn advance(&self, sent: &[InputItem], returned: &[InputItem]) {
        if let Ok(mut baseline) = self.baseline.lock() {
            baseline.advance(sent, returned);
        }
    }

    pub fn record_discovered(&self, names: BTreeSet<String>) {
        if names.is_empty() {
            return;
        }
        if let Ok(mut known) = self.discovered_tools.lock() {
            known.extend(names);
        }
    }
}

/// The live conversations.
#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<Vec<Arc<Session>>>,
    counter: std::sync::atomic::AtomicU64,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The session this request continues, or a new one.
    ///
    /// A request matching several sessions takes the one with the longest
    /// baseline: that is the most specific continuation, and any shorter match
    /// is a prefix of it.
    pub fn resolve(&self, input: &[InputItem]) -> Arc<Session> {
        let Ok(mut sessions) = self.sessions.lock() else {
            return Arc::new(Session::new(self.next_key()));
        };

        let matched = sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                session
                    .baseline
                    .lock()
                    .map(|baseline| codex_cc_proxy_core::session::extends(baseline.items(), input))
                    .unwrap_or(false)
            })
            .max_by_key(|(_, session)| {
                session
                    .baseline
                    .lock()
                    .map(|baseline| baseline.len())
                    .unwrap_or(0)
            })
            .map(|(index, _)| index);

        if let Some(index) = matched {
            // Most recently used moves to the front, so eviction takes the
            // conversation nobody is having.
            let session = sessions.remove(index);
            sessions.insert(0, Arc::clone(&session));
            return session;
        }

        let session = Arc::new(Session::new(self.next_key()));
        sessions.insert(0, Arc::clone(&session));
        sessions.truncate(CAPACITY);
        session
    }

    pub fn len(&self) -> usize {
        self.sessions
            .lock()
            .map(|sessions| sessions.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn next_key(&self) -> String {
        let index = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("session-{index}")
    }
}
