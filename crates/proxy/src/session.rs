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
    /// Whether a turn has ever completed on this conversation. Until one has,
    /// the baseline is provisional and may be replaced; afterwards it is what
    /// the backend is known to hold.
    confirmed: std::sync::atomic::AtomicBool,
    /// §4.2 — the transport binding, created on this conversation's first turn
    /// and kept for its life. Latching lives here, so a session that fell back
    /// stays fallen back.
    pub conduit: tokio::sync::OnceCell<Arc<crate::upstream::conduit::Conduit>>,
    /// What the last turn sent and what came back, which is what a delta
    /// continues (§4.3).
    pub last_request: Mutex<Option<codex_cc_proxy_core::responses::ResponsesRequest>>,
    pub last_response_id: Mutex<Option<String>>,
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
            confirmed: std::sync::atomic::AtomicBool::new(false),
            conduit: tokio::sync::OnceCell::new(),
            last_request: Mutex::new(None),
            last_response_id: Mutex::new(None),
            estimator: CalibratedEstimator::new(),
            discovered_tools: Mutex::new(BTreeSet::new()),
            cache_key,
        }
    }

    /// The conduit for this conversation, built once.
    pub async fn conduit(
        &self,
        build: impl FnOnce() -> Arc<crate::upstream::conduit::Conduit>,
    ) -> Arc<crate::upstream::conduit::Conduit> {
        Arc::clone(self.conduit.get_or_init(|| async { build() }).await)
    }

    pub fn previous(
        &self,
    ) -> (
        Option<codex_cc_proxy_core::responses::ResponsesRequest>,
        Option<String>,
    ) {
        (
            self.last_request.lock().ok().and_then(|held| held.clone()),
            self.last_response_id
                .lock()
                .ok()
                .and_then(|held| held.clone()),
        )
    }

    pub fn remember_request(&self, request: &codex_cc_proxy_core::responses::ResponsesRequest) {
        if let Ok(mut held) = self.last_request.lock() {
            *held = Some(request.clone());
        }
    }

    pub fn remember_response(&self, id: String) {
        if let Ok(mut held) = self.last_response_id.lock() {
            *held = Some(id);
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
        self.confirmed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Claim a brand-new session so a concurrent request cannot match its empty
    /// baseline and join a conversation it has nothing to do with.
    ///
    /// Only ever applied to a session no turn has completed on. Overwriting a
    /// confirmed baseline with a turn that has not been accepted yet is what
    /// makes a *failed* turn corrupt the next delta: the backend never saw the
    /// items, but the baseline says it did, so the next delta skips them and
    /// the question silently vanishes from the conversation.
    pub fn seed_if_unconfirmed(&self, sent: &[InputItem]) {
        if self.confirmed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if let Ok(mut baseline) = self.baseline.lock() {
            baseline.advance(sent, &[]);
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

        let matched = Self::best_match(&sessions, input);

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

    /// The session this input continues, if there already is one.
    ///
    /// Unlike `resolve` this creates nothing and reorders nothing. A read-only
    /// caller — pre-flight sizing (§5) — must not enter a conversation into the
    /// store: the entry would never advance, it would match every first turn
    /// that followed, and at capacity it would evict a conversation someone is
    /// actually having.
    pub fn lookup(&self, input: &[InputItem]) -> Option<Arc<Session>> {
        let sessions = self.sessions.lock().ok()?;
        Self::best_match(&sessions, input)
            .and_then(|index| sessions.get(index))
            .map(Arc::clone)
    }

    /// The longest baseline this input continues: the most specific match, of
    /// which any shorter one is a prefix.
    ///
    /// Continuation is judged by the reconciling predicate, the same one the
    /// incremental path uses (§3.1). Judging it strictly abandons the
    /// conversation the moment the model reasons: the baseline then holds an
    /// item the client cannot replay, no replay extends it again, and every
    /// later turn silently starts a new session — losing its calibration, its
    /// discovered tools, and every delta with it.
    fn best_match(sessions: &[Arc<Session>], input: &[InputItem]) -> Option<usize> {
        sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                session
                    .baseline
                    .lock()
                    .map(|baseline| baseline.reconcile(input).is_some())
                    .unwrap_or(false)
            })
            .max_by_key(|(_, session)| {
                session
                    .baseline
                    .lock()
                    .map(|baseline| baseline.len())
                    .unwrap_or(0)
            })
            .map(|(index, _)| index)
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
