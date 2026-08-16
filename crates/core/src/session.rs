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
    pub fn plan<'a>(&self, candidate: &'a [InputItem]) -> Plan<'a> {
        match delta(&self.items, candidate) {
            Some(new_items) => Plan::Delta(new_items),
            None => Plan::Full,
        }
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
