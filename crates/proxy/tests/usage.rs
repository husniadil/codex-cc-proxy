//! `docs/api.md` §3 — the quota snapshot the backend opens a stream with.
//!
//! The payloads here are shaped like a real one, because they came from one.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use proxenos::usage::Snapshot;
use serde_json::json;

/// A real snapshot, as the backend sends it: one long window, no second one.
fn free_plan() -> String {
    json!({
        "type": "codex.rate_limits",
        "plan_type": "free",
        "credits": { "balance": null, "has_credits": false },
        "rate_limits": {
            "allowed": true,
            "limit_reached": false,
            "primary": {
                "used_percent": 6,
                "window_minutes": 43200,
                "reset_at": 1789487264u64,
                "reset_after_seconds": 2554912u64,
            },
            "secondary": null,
        },
    })
    .to_string()
}

#[test]
fn a_snapshot_is_read_from_the_event_the_backend_opens_with() {
    let snapshot = Snapshot::parse(&free_plan()).expect("this is a rate-limit event");

    assert_eq!(snapshot.plan.as_deref(), Some("free"));
    assert!(!snapshot.limit_reached);
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].used_percent, 6.0);
    assert_eq!(snapshot.windows[0].window_minutes, Some(43200));
}

/// Anything else in the stream is not a snapshot.
#[test]
fn other_events_are_not_snapshots() {
    assert!(Snapshot::parse(&json!({ "type": "response.created" }).to_string()).is_none());
    assert!(Snapshot::parse("not json").is_none());
}

/// Windows are matched to header slots by how long they are, never by their
/// position in the payload.
///
/// The backend has changed which windows it reports — a five-hour window
/// existed, was removed, and may return — so `primary` is not a synonym for
/// "the five-hour one". Position-based mapping would put whatever is reported
/// first into the five-hour slot and be wrong the moment the set changes again.
#[test]
fn windows_map_to_slots_by_duration() {
    let payload = json!({
        "type": "codex.rate_limits",
        "rate_limits": {
            "limit_reached": false,
            // Deliberately the wrong way round: the long window first.
            "primary": { "used_percent": 40, "window_minutes": 10080, "reset_at": 200 },
            "secondary": { "used_percent": 10, "window_minutes": 300, "reset_at": 100 },
        },
    })
    .to_string();

    let headers = Snapshot::parse(&payload).unwrap().headers();
    let get = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    };

    assert_eq!(
        get("anthropic-ratelimit-unified-5h-utilization").as_deref(),
        Some("0.1000")
    );
    assert_eq!(
        get("anthropic-ratelimit-unified-5h-reset").as_deref(),
        Some("100")
    );
    assert_eq!(
        get("anthropic-ratelimit-unified-7d-utilization").as_deref(),
        Some("0.4000")
    );
}

/// A window matching no slot produces no header at all.
///
/// The live account's window is thirty days. Announcing that as five hours
/// would show a meter that is wrong in the reassuring direction — it would read
/// as plenty of headroom resetting shortly, when neither is true.
#[test]
fn a_window_that_fits_no_slot_is_not_forced_into_one() {
    let headers = Snapshot::parse(&free_plan()).unwrap().headers();
    assert!(
        headers.is_empty(),
        "a thirty-day window fits neither slot: {headers:?}"
    );
}

/// The control socket still reports it, with its real length — that is the
/// difference between the two surfaces, and why both exist.
#[test]
fn the_socket_reports_a_window_the_headers_cannot() {
    let reported = Snapshot::parse(&free_plan()).unwrap().to_json();

    assert_eq!(reported["known"], json!(true));
    assert_eq!(reported["plan"], json!("free"));
    assert_eq!(reported["windows"][0]["window_minutes"], json!(43200));
    assert_eq!(reported["windows"][0]["used_percent"], json!(6.0));
}

/// A window with no percentage is absent, not zero.
#[test]
fn a_window_without_a_percentage_is_not_reported_as_empty() {
    let payload = json!({
        "type": "codex.rate_limits",
        "rate_limits": {
            "limit_reached": false,
            "primary": { "window_minutes": 300, "reset_at": 100 },
            "secondary": null,
        },
    })
    .to_string();

    let snapshot = Snapshot::parse(&payload).unwrap();
    assert!(snapshot.windows.is_empty());
    assert!(snapshot.headers().is_empty());
}

/// A reached limit says so, in the word the client parses.
#[test]
fn a_reached_limit_is_reported_as_rejected() {
    let payload = json!({
        "type": "codex.rate_limits",
        "rate_limits": {
            "limit_reached": true,
            "primary": { "used_percent": 100, "window_minutes": 300, "reset_at": 100 },
        },
    })
    .to_string();

    let headers = Snapshot::parse(&payload).unwrap().headers();
    assert!(
        headers.contains(&("anthropic-ratelimit-unified-status", "rejected".to_owned())),
        "{headers:?}"
    );
}

/// The quota endpoint's own shape, read from a recorded response.
///
/// It lives under `fixtures/upstream/` rather than in the probe corpus: the
/// corpus is a set of replayable Messages exchanges, and this is a recorded
/// REST response. Putting it there made four corpus tests fail, which is the
/// corpus correctly refusing something that is not one of its own.
///
/// **This fixture was captured, not written.** The shape differs from the
/// stream event's in three ways that a guess would have got wrong: the windows
/// are `primary_window`/`secondary_window` rather than `primary`/`secondary`,
/// the length is stated in **seconds** rather than minutes, and the plan sits at
/// the top level rather than beside the limits. A parser written from the
/// stream shape would have parsed this into nothing and reported "no quota" on
/// an account that has one.
#[test]
fn a_recorded_quota_response_parses_into_the_same_snapshot_a_stream_produces() {
    let payload = std::fs::read_to_string("../../fixtures/upstream/quota-rest.json").unwrap();

    let snapshot = proxenos::usage::Snapshot::parse_rest(&payload)
        .expect("the recorded response should parse");

    assert_eq!(snapshot.plan.as_deref(), Some("free"));
    assert!(!snapshot.limit_reached);
    assert_eq!(snapshot.windows.len(), 1);

    let window = &snapshot.windows[0];
    assert_eq!(window.used_percent, 15.0);
    // Seconds on the wire, minutes in the snapshot — the unit every other
    // reader of a window already uses.
    assert_eq!(window.window_minutes, Some(43_200));
    assert_eq!(window.resets_at, Some(1_789_487_264));
}

/// A null window is absent, never a window reporting zero used.
#[test]
fn a_window_the_backend_did_not_report_is_absent_rather_than_zero() {
    let payload = std::fs::read_to_string("../../fixtures/upstream/quota-rest.json").unwrap();
    let snapshot = proxenos::usage::Snapshot::parse_rest(&payload).unwrap();

    assert!(
        snapshot
            .windows
            .iter()
            .all(|window| window.used_percent > 0.0),
        "a null secondary window must not become a zeroed one"
    );
}

/// Something that is not this response is refused rather than parsed into an
/// empty snapshot — which would read as "quota known, nothing used".
#[test]
fn an_unrecognized_body_is_not_read_as_an_empty_quota() {
    assert!(proxenos::usage::Snapshot::parse_rest("{}").is_none());
    assert!(proxenos::usage::Snapshot::parse_rest("not json").is_none());
    assert!(
        proxenos::usage::Snapshot::parse_rest(r#"{"rate_limit":{}}"#).is_none(),
        "a rate_limit with no window at all says nothing about quota"
    );
}
