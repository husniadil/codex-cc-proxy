//! `docs/api.md` §3 — the quota snapshot the backend opens a stream with.
//!
//! The payloads here are shaped like a real one, because they came from one.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use proxenos::usage::Snapshot;
use proxenos::usage::Source;
use serde_json::json;
use std::sync::Arc;

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

/// A snapshot with a percentage no other snapshot in a test can be confused
/// for.
fn snapshot(used_percent: f64) -> Snapshot {
    Snapshot {
        plan: Some("plus".to_owned()),
        limit_reached: false,
        windows: vec![proxenos::usage::Window {
            used_percent,
            window_minutes: Some(300),
            resets_at: Some(1_789_487_264),
        }],
    }
}

/// A store whose serving account is whatever this name says.
fn store_serving(name: &'static str) -> proxenos::usage::UsageStore {
    proxenos::usage::UsageStore::default()
        .serving(Arc::new(move || Some(name.to_owned())) as proxenos::usage::ServingAccount)
}

/// **Build 1, at the store.** Two accounts can serve one session, so a figure
/// is held under the account it belongs to rather than as the daemon's one
/// latest.
#[test]
fn each_accounts_figure_is_held_under_its_own_name() {
    let store = store_serving("main");

    store.record_for(None, &snapshot(11.0), Source::Turn);
    store.record_for(Some("spare"), &snapshot(77.0), Source::Turn);

    let held: Vec<(String, f64)> = store
        .accounts()
        .into_iter()
        .map(|(name, measured)| (name, measured.snapshot.windows[0].used_percent))
        .collect();
    assert_eq!(
        held,
        vec![("main".to_owned(), 11.0), ("spare".to_owned(), 77.0)],
        "a pinned tier's turn must not displace the serving account's figure"
    );
}

/// An unpinned turn is filed under the account that actually served it, by
/// name — which is what lets a later select report the right account's figure
/// rather than whatever the last turn happened to leave behind.
#[test]
fn an_unpinned_turn_is_filed_under_the_serving_accounts_name() {
    let store = store_serving("main");
    store.record_for(None, &snapshot(11.0), Source::Turn);

    assert_eq!(
        store.latest_for("main").map(|m| m.snapshot),
        Some(snapshot(11.0))
    );
}

/// The figure reported where one has always been reported is the serving
/// account's, never a pinned account's.
#[test]
fn the_top_level_figure_follows_the_serving_account() {
    let store = store_serving("main");
    store.record_for(Some("spare"), &snapshot(77.0), Source::Turn);

    assert_eq!(
        store.latest(),
        None,
        "the serving account has made no turn, and the pinned account's \
         headroom is not an answer for it"
    );

    store.record_for(None, &snapshot(11.0), Source::Turn);
    assert_eq!(store.latest(), Some(snapshot(11.0)));
}

/// How a figure was come by, and when, travels with it: one that rode a turn
/// and one that was asked for are both legitimate and differently stale.
#[test]
fn a_figure_carries_how_it_was_come_by_and_when() {
    let store = store_serving("main");
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    store.record_for(None, &snapshot(11.0), Source::Turn);
    store.record_for(Some("spare"), &snapshot(77.0), Source::Fetch);

    let main = store.latest_for("main").unwrap();
    assert_eq!(main.source, Source::Turn);
    assert!(main.at >= before, "the moment it was taken travels with it");
    assert_eq!(store.latest_for("spare").unwrap().source, Source::Fetch);
}

/// **Build 4, at the store.** Forgetting an account drops its figure and
/// nothing else: the rest stay valid because each is held under its own name.
#[test]
fn forgetting_an_account_drops_only_its_own_figure() {
    let store = store_serving("main");
    store.record_for(None, &snapshot(11.0), Source::Turn);
    store.record_for(Some("spare"), &snapshot(77.0), Source::Turn);

    store.forget("spare");

    assert!(store.latest_for("spare").is_none());
    assert_eq!(
        store.latest_for("main").map(|m| m.snapshot),
        Some(snapshot(11.0))
    );
}

/// A figure taken where no account could be named is reported at the top and
/// never as some account's, and a select drops it — presenting it as the newly
/// selected account's headroom is the error this whole keying exists to stop.
#[test]
fn a_figure_no_account_can_be_named_for_is_dropped_by_a_select() {
    let store = proxenos::usage::UsageStore::default();
    store.record_for(None, &snapshot(11.0), Source::Turn);

    assert_eq!(store.latest(), Some(snapshot(11.0)));
    assert!(
        store.accounts().is_empty(),
        "nothing can name it, so it names nothing"
    );

    store.forget_unattributed();
    assert_eq!(store.latest(), None);
}

/// The wiring the daemon actually uses: the serving account is whichever one
/// the credential store has selected, read when the figure is recorded.
///
/// Asserted through the store rather than through the switch, because a
/// resolver that was never handed a credential store files every figure as
/// unattributed and reports an empty per-account meter while every unit test
/// still passes.
#[test]
fn the_serving_account_is_the_one_the_credential_store_has_selected() {
    use proxenos::auth::store::AccountStore;

    let dir = tempfile::tempdir().unwrap();
    let credentials = Arc::new(proxenos::auth::store::FileStore::new(
        dir.path().join("credentials.json"),
    ));
    let grant = |account_id: &str| proxenos::auth::store::Credentials {
        access_token: "token".to_owned(),
        refresh_token: "refresh".to_owned(),
        id_token: None,
        account_id: Some(account_id.to_owned()),
        expires_at: Some(u64::MAX / 2),
    };
    credentials.add(&grant("acct_main"), Some("main")).unwrap();
    credentials
        .add(&grant("acct_spare"), Some("spare"))
        .unwrap();
    credentials.select("spare").unwrap();

    let store = proxenos::usage::UsageStore::for_accounts(
        Arc::clone(&credentials) as Arc<dyn AccountStore>
    );
    store.record_for(None, &snapshot(23.0), Source::Turn);

    assert_eq!(
        store.latest_for("spare").map(|m| m.snapshot),
        Some(snapshot(23.0))
    );
    assert!(store.latest_for("main").is_none());

    // And it follows the selection rather than remembering it.
    credentials.select("main").unwrap();
    store.record_for(None, &snapshot(41.0), Source::Turn);
    assert_eq!(
        store.latest_for("main").map(|m| m.snapshot),
        Some(snapshot(41.0))
    );
}

/// The second provider answers quota in response headers, and that is the only
/// place it answers one for this credential kind: its usage endpoint refuses a
/// subscription token for want of a scope. `fixtures/upstream/relay-quota-headers.json`
/// is one live turn's headers, captured rather than written.
#[test]
fn a_snapshot_is_read_from_the_second_providers_response_headers() {
    let captured: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/upstream/relay-quota-headers.json"
    ))
    .expect("the fixture is JSON");

    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in captured["headers"].as_object().expect("a header map") {
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.as_str().unwrap().parse().unwrap(),
        );
    }

    let snapshot = Snapshot::from_headers(&headers).expect("these headers carry a quota");

    assert!(!snapshot.limit_reached, "the turn was allowed");
    // Plan is genuinely unavailable on this path — no header states one — and
    // an invented plan name is worse than none.
    assert_eq!(snapshot.plan, None);

    let window = |minutes: u64| {
        snapshot
            .windows
            .iter()
            .find(|window| window.window_minutes == Some(minutes))
            .unwrap_or_else(|| panic!("no {minutes}-minute window in {:?}", snapshot.windows))
    };
    assert_eq!(window(300).used_percent, 13.0);
    assert_eq!(window(300).resets_at, Some(1_787_338_800));
    assert_eq!(window(10080).used_percent, 93.0);
    assert_eq!(window(10080).resets_at, Some(1_787_371_200));
}

/// A response carrying no quota header says nothing about quota, and saying
/// nothing is not the same as saying none is used.
#[test]
fn headers_without_a_quota_are_not_an_empty_snapshot() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "text/event-stream".parse().unwrap(),
    );
    assert!(Snapshot::from_headers(&headers).is_none());
}

/// A refused turn says so, and the meter has to show it.
#[test]
fn a_rejected_status_reads_as_the_limit_reached() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "1.0".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-status",
        "rejected".parse().unwrap(),
    );
    let snapshot = Snapshot::from_headers(&headers).expect("a window was reported");
    assert!(snapshot.limit_reached);
    assert_eq!(snapshot.windows[0].used_percent, 100.0);
}
