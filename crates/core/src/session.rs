//! `docs/proxy-behavior.md` §3 and §4.3 — conversation identity and deltas.
//!
//! Pure. The predicate that decides whether a request continues a session is
//! the same one that decides whether a delta is valid, so the two cannot
//! disagree.

use crate::responses::InputItem;

/// Whether `candidate` is a strict extension of `baseline`.
///
/// "Strict" means every baseline item appears at the same index, unchanged. Not
/// a longest-common-prefix score: a partial match is a different conversation
/// that happens to start the same way, and treating it as a continuation
/// silently grafts one conversation onto another.
pub fn extends(baseline: &[InputItem], candidate: &[InputItem]) -> bool {
    if candidate.len() < baseline.len() {
        return false;
    }
    baseline
        .iter()
        .zip(candidate.iter())
        .all(|(before, now)| items_equal(before, now))
}

/// The items `candidate` adds to `baseline`, when it extends it.
///
/// `None` when it does not — and the caller sends everything. **Falling back is
/// always safe; a wrong delta is not.** A full send costs bandwidth. A wrong
/// delta corrupts the conversation and does not fail visibly.
pub fn delta<'a>(baseline: &[InputItem], candidate: &'a [InputItem]) -> Option<&'a [InputItem]> {
    if !extends(baseline, candidate) {
        return None;
    }
    candidate.get(baseline.len()..)
}

/// Items are compared by their content, ignoring server-assigned ids.
///
/// An id assigned by one response is not present when the client replays the
/// same conversation, so comparing ids would make every turn look like a
/// divergence and defeat the delta entirely.
fn items_equal(left: &InputItem, right: &InputItem) -> bool {
    strip_ids(left) == strip_ids(right)
}

fn strip_ids(item: &InputItem) -> serde_json::Value {
    let mut value = serde_json::to_value(item).unwrap_or(serde_json::Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.remove("id");
    }
    value
}

/// Whether the client could ever have replayed this item.
///
/// Reasoning is returned by the server and cannot survive a round trip: the
/// client drops `thinking` on the way in and would not return encrypted
/// upstream reasoning anyway (§3.3). So it sits in the baseline with no
/// counterpart in what the client sends, and a comparison that expects one
/// finds a mismatch on every turn after the first.
fn is_server_only(item: &InputItem) -> bool {
    matches!(item, InputItem::Reasoning { .. })
}

/// The conversation as the backend should see it, given what the client
/// replayed.
///
/// §3.3 — server-only items are put back where the server produced them. This
/// is the one place the proxy adds content the client did not send, and it is
/// additive and upstream-only: nothing here is surfaced back as model output.
#[derive(Debug, PartialEq)]
pub struct Reconciled {
    /// Everything to send on a full upload — the baseline with its server-only
    /// items intact, followed by whatever the client added.
    pub input: Vec<InputItem>,
    /// How many of those are new this turn. The tail of `input`.
    pub new_items: usize,
}

/// Match a client replay against a baseline that may contain server-only items.
///
/// `None` when the replay does not line up, which means a full send of exactly
/// what the client asked for. Falling back is always safe; a wrong delta is not.
pub fn reconcile(baseline: &[InputItem], candidate: &[InputItem]) -> Option<Reconciled> {
    let mut consumed = 0usize;

    for item in baseline {
        if is_server_only(item) {
            // The client never had this one, so it consumes nothing.
            continue;
        }
        let next = candidate.get(consumed)?;
        if !items_equal(item, next) {
            return None;
        }
        consumed = consumed.saturating_add(1);
    }

    let new = candidate.get(consumed..)?;
    let mut input = baseline.to_vec();
    input.extend_from_slice(new);

    Some(Reconciled {
        input,
        new_items: new.len(),
    })
}

/// What a session remembers between turns.
#[derive(Debug, Default, Clone)]
pub struct Baseline {
    /// Everything the backend has been told, in order: what was sent, plus the
    /// items the server added in reply.
    items: Vec<InputItem>,
}

impl Baseline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[InputItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// What to send for this request: the new items, or everything.
    ///
    /// This is a strict comparison, and deliberately so. It operates on input
    /// that has already been reconciled — the conversation as the *backend*
    /// holds it, server-only items included — so there is nothing left to match
    /// past. `reconcile` is what converts a client replay into that form, and
    /// running it twice misaligns exactly the items it put back.
    ///
    /// §3.1 asks that continuation be decided once rather than by two rules
    /// that can disagree. It is: `reconcile` decides, and this follows from it
    /// mechanically.
    pub fn plan<'a>(&self, candidate: &'a [InputItem]) -> Plan<'a> {
        match delta(&self.items, candidate) {
            Some(new_items) => Plan::Delta(new_items),
            None => Plan::Full,
        }
    }

    /// The same, allowing for server-only items the client cannot replay.
    pub fn reconcile(&self, candidate: &[InputItem]) -> Option<Reconciled> {
        reconcile(&self.items, candidate)
    }

    /// Record what was sent and what the server added.
    ///
    /// Server-returned items become part of the baseline in exactly the same
    /// way sent items do, so the incremental and full-send paths agree on what
    /// the conversation contains (§3.3).
    pub fn advance(&mut self, sent: &[InputItem], returned: &[InputItem]) {
        self.items = sent.to_vec();
        self.items.extend_from_slice(returned);
    }
}

#[derive(Debug, PartialEq)]
pub enum Plan<'a> {
    /// Send exactly these.
    Delta(&'a [InputItem]),
    /// Send everything. Always valid.
    Full,
}
