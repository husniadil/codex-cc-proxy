//! `docs/api.md` §3 — the policy a running daemon routes turns from.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use proxenos::config::ResolvedTier;
use proxenos::policy::Policy;
use proxenos::policy::Snapshot;
use proxenos_core::responses::Effort;
use std::sync::Arc;

fn tier(name: &'static str, model: &str) -> ResolvedTier {
    ResolvedTier {
        defaulted: false,
        tier: name,
        model: model.to_owned(),
    }
}

/// The routing table is derived from the tiers, never stored beside them.
///
/// `status` reports the tiers and the ingress routes on the models. If those
/// two can be set independently, a daemon can report one mapping and serve
/// another — working turns against the wrong model, which is the failure this
/// project refuses everywhere else.
#[test]
fn the_routing_table_always_matches_the_tiers() {
    let policy = Policy::new(Snapshot::new(vec![tier("opus", "a")], None));

    policy.set_tiers(vec![tier("opus", "b"), tier("sonnet", "c")]);

    let snapshot = policy.get();
    let routed: Vec<(&str, &str)> = snapshot
        .models()
        .iter()
        .map(|mapping| (mapping.requested.as_str(), mapping.upstream.as_str()))
        .collect();
    assert_eq!(routed, vec![("opus", "b"), ("sonnet", "c")]);
}

/// Setting one leaves the other alone — under real contention.
///
/// Each setter has to carry across the field it is not changing. Read through
/// one call and written through another, a mapping write that read the ceiling
/// *before* another caller changed it puts the old value back, and the change
/// is gone for good. The control socket spawns a task per connection, so two
/// front-ends really can arrive at once.
///
/// The detector is a ceiling that changes exactly ONCE while a mapping thread
/// spins: a stale carry-across reverts it to `None` permanently, so the single
/// check after both threads join is unambiguous. An earlier version sampled the
/// ceiling inside the loop and counted every `None` as a revert — including the
/// ones before the other thread had set it at all, which made it fail under
/// suite load for a reason that had nothing to do with the race.
#[test]
fn a_concurrent_set_never_reverts_the_field_it_did_not_touch() {
    for attempt in 0..40 {
        let policy = Arc::new(Policy::new(Snapshot::new(vec![tier("opus", "a")], None)));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mapping = {
            let policy = Arc::clone(&policy);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    policy.set_tiers(vec![tier("opus", "a")]);
                }
            })
        };

        // Long enough for the mapping thread to be mid-flight, short enough
        // that forty attempts stay quick.
        std::thread::sleep(std::time::Duration::from_micros(200 + attempt * 20));
        policy.set_effort_ceiling(Some(Effort::High));

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        mapping.join().unwrap();

        assert_eq!(
            policy.get().effort_ceiling(),
            Some(Effort::High),
            "attempt {attempt}: a mapping write carried a stale ceiling over the change"
        );
    }
}
