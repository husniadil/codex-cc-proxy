//! `docs/api.md` §2.1 — merging quota into a status line's payload.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy::statusline::merge;
use pretty_assertions::assert_eq;
use serde_json::json;

/// A payload shaped like the one the client hands a status-line script.
fn payload() -> serde_json::Value {
    json!({
        "model": { "display_name": "gpt-5.6-luna" },
        "context_window": { "used_percentage": 12 },
        "workspace": { "current_dir": "/tmp" },
    })
}

fn usage(windows: serde_json::Value) -> serde_json::Value {
    json!({
        "known": true,
        "plan": "plus",
        "limit_reached": false,
        "windows": windows,
    })
}

/// A window the client has a name for lands where a script already looks.
#[test]
fn a_five_hour_window_fills_the_field_a_script_reads() {
    let merged = merge(
        payload(),
        &usage(json!([
            { "used_percent": 42.0, "window_minutes": 300, "resets_at": 1789487264u64 },
        ])),
    );

    assert_eq!(
        merged["rate_limits"]["five_hour"]["used_percentage"],
        json!(42.0)
    );
    assert_eq!(
        merged["rate_limits"]["five_hour"]["resets_at"],
        json!(1789487264u64)
    );
    // And nothing was claimed about the window the backend never reported.
    assert!(merged["rate_limits"]["seven_day"].is_null());
}

#[test]
fn a_seven_day_window_fills_its_own_field() {
    let merged = merge(
        payload(),
        &usage(json!([
            { "used_percent": 13.0, "window_minutes": 10080, "resets_at": 1u64 },
        ])),
    );

    assert_eq!(
        merged["rate_limits"]["seven_day"]["used_percentage"],
        json!(13.0)
    );
    assert!(merged["rate_limits"]["five_hour"].is_null());
}

/// A window matching neither is reported with its real length, never filed
/// under a name that would misstate it.
///
/// The live account's window is thirty days. Put in the five-hour field it
/// would read as plenty of headroom resetting shortly, and both halves of that
/// are false.
#[test]
fn a_window_the_client_has_no_name_for_keeps_its_own_length() {
    let merged = merge(
        payload(),
        &usage(json!([
            { "used_percent": 9.0, "window_minutes": 43200, "resets_at": 1789487264u64 },
        ])),
    );

    assert!(merged["rate_limits"]["five_hour"].is_null());
    assert!(merged["rate_limits"]["seven_day"].is_null());
    assert_eq!(
        merged["rate_limits"]["windows"][0]["window_minutes"],
        json!(43200)
    );
    assert_eq!(merged["rate_limits"]["plan"], json!("plus"));
}

/// Everything the script was already given survives.
#[test]
fn the_payload_passes_through_otherwise_untouched() {
    let merged = merge(payload(), &usage(json!([])));

    assert_eq!(merged["model"]["display_name"], json!("gpt-5.6-luna"));
    assert_eq!(merged["context_window"]["used_percentage"], json!(12));
    assert_eq!(merged["workspace"]["current_dir"], json!("/tmp"));
}

/// No quota yet, or an answer that cannot be read, leaves the payload alone.
///
/// A status line renders constantly and must never be the thing that breaks. A
/// missing figure is a smaller failure than a wrong one, and far smaller than
/// no status line at all.
#[test]
fn an_unknown_or_unreadable_snapshot_changes_nothing() {
    let before = payload();

    assert_eq!(
        merge(before.clone(), &json!({ "known": false, "detail": "..." })),
        before
    );
    assert_eq!(merge(before.clone(), &json!("nonsense")), before);
    assert_eq!(merge(before.clone(), &json!({ "known": true })), before);
}

/// A payload that is not an object is handed back as it came.
#[test]
fn a_payload_that_is_not_an_object_is_untouched() {
    assert_eq!(
        merge(json!("not a payload"), &usage(json!([]))),
        json!("not a payload")
    );
}

/// An existing `rate_limits` object is added to, not replaced.
#[test]
fn fields_already_present_under_rate_limits_survive() {
    let mut input = payload();
    input["rate_limits"] = json!({ "something_else": 1 });

    let merged = merge(
        input,
        &usage(json!([{ "used_percent": 42.0, "window_minutes": 300 }])),
    );

    assert_eq!(merged["rate_limits"]["something_else"], json!(1));
    assert_eq!(
        merged["rate_limits"]["five_hour"]["used_percentage"],
        json!(42.0)
    );
}
