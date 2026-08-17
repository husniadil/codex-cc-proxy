//! `docs/api.md` §2.1 — filling in a status line's quota.
//!
//! The status line is whatever script the user configured, and it is handed a
//! JSON payload on stdin. That payload has no quota in it when the client is
//! pointed at a proxy: the client tracks a subscription quota for its own
//! account, and a proxy is not one. Measured — setting the response headers it
//! parses is not enough.
//!
//! So the data is put where the script already looks. This wraps the user's own
//! command: read their payload, merge in what the backend reported, hand it on.
//! A script written against the client's shape keeps working and gains a figure
//! it could not otherwise have.
//!
//! **Merging never invents.** A window the backend did not report leaves the
//! field absent rather than zero, and a window whose length matches neither of
//! the two the client names is not filed under either — it is reported under a
//! key of our own, where its real length travels with it.

use serde_json::Value;

/// The two windows the client's payload names, in minutes.
const FIVE_HOURS: u64 = 5 * 60;
const SEVEN_DAYS: u64 = 7 * 24 * 60;
const TOLERANCE: f64 = 0.25;

fn matches(window_minutes: u64, nominal: u64) -> bool {
    #[allow(clippy::cast_precision_loss)]
    let (actual, nominal) = (window_minutes as f64, nominal as f64);
    (actual - nominal).abs() <= nominal * TOLERANCE
}

/// Merge a quota snapshot into the payload a status line receives.
///
/// `usage` is the control socket's answer. Anything unrecognizable leaves the
/// payload exactly as it arrived: a status line that renders every second must
/// never be the thing that breaks, and a missing figure is a smaller failure
/// than a wrong one.
pub fn merge(mut payload: Value, usage: &Value) -> Value {
    if usage.get("known").and_then(Value::as_bool) != Some(true) {
        return payload;
    }
    let Some(windows) = usage.get("windows").and_then(Value::as_array) else {
        return payload;
    };
    let Some(object) = payload.as_object_mut() else {
        return payload;
    };

    let mut rate_limits = object
        .get("rate_limits")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // Everything the backend reported, with its real length. A script that
    // wants the truth about a window the client has no name for reads this.
    rate_limits.insert("windows".to_owned(), Value::Array(windows.clone()));
    if let Some(plan) = usage.get("plan") {
        rate_limits.insert("plan".to_owned(), plan.clone());
    }
    if let Some(reached) = usage.get("limit_reached") {
        rate_limits.insert("limit_reached".to_owned(), reached.clone());
    }

    // And the two the client names, where a window genuinely is one of them.
    for (nominal, key) in [(FIVE_HOURS, "five_hour"), (SEVEN_DAYS, "seven_day")] {
        let Some(window) = windows.iter().find(|window| {
            window
                .get("window_minutes")
                .and_then(Value::as_u64)
                .is_some_and(|minutes| matches(minutes, nominal))
        }) else {
            continue;
        };

        let mut slot = serde_json::Map::new();
        if let Some(used) = window.get("used_percent") {
            slot.insert("used_percentage".to_owned(), used.clone());
        }
        if let Some(resets_at) = window.get("resets_at") {
            slot.insert("resets_at".to_owned(), resets_at.clone());
        }
        if !slot.is_empty() {
            rate_limits.insert(key.to_owned(), Value::Object(slot));
        }
    }

    object.insert("rate_limits".to_owned(), Value::Object(rate_limits));
    payload
}
