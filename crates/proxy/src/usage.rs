//! `docs/api.md` §3 — what quota is left.
//!
//! The backend opens every stream with a snapshot of the account's rate limits,
//! before it says anything about the response. That is free — it rides along
//! with a turn already being made — and it is the only place this figure
//! appears, so it is read there rather than polled.
//!
//! **Nothing here is computed.** A window the backend did not report is absent
//! rather than zero, and a percentage is passed through as given. An invented
//! quota figure is worse than no figure: it reads as headroom that is not there.

use serde_json::Value;
use std::sync::Mutex;

/// One quota window as the backend reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub used_percent: f64,
    /// How long the window is. The backend has changed which windows it
    /// reports, so this is what identifies one — not its position.
    pub window_minutes: Option<u64>,
    /// Epoch seconds.
    pub resets_at: Option<u64>,
}

/// The account's quota, as of one turn.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub plan: Option<String>,
    pub limit_reached: bool,
    pub windows: Vec<Window>,
}

/// The header names this client reads a quota out of.
///
/// Two fixed windows, five hours and seven days. The backend's windows are not
/// fixed — it has reported a five-hour window in the past, does not now, and may
/// again — so windows are matched to slots by *duration*. A window that matches
/// neither slot is reported through the control socket, where it can state its
/// real length, and is not forced into a slot that would misname it.
const FIVE_HOURS: u64 = 5 * 60;
const SEVEN_DAYS: u64 = 7 * 24 * 60;

/// How far from the nominal duration still counts as that window.
///
/// Generous, because the point is to recognize a window the backend calls five
/// hours even if it reports 299 minutes — and narrow enough that a thirty-day
/// window can never be mistaken for either.
const TOLERANCE: f64 = 0.25;

fn matches(window_minutes: u64, nominal: u64) -> bool {
    #[allow(clippy::cast_precision_loss)]
    let (actual, nominal) = (window_minutes as f64, nominal as f64);
    (actual - nominal).abs() <= nominal * TOLERANCE
}

impl Snapshot {
    /// Read a snapshot out of one upstream event, if that is what it is.
    pub fn parse(payload: &str) -> Option<Self> {
        let event: Value = serde_json::from_str(payload).ok()?;
        if event.get("type").and_then(Value::as_str) != Some("codex.rate_limits") {
            return None;
        }

        let limits = event.get("rate_limits")?;
        let windows = ["primary", "secondary"]
            .into_iter()
            .filter_map(|name| parse_window(limits.get(name)?))
            .collect();

        Some(Self {
            plan: event
                .get("plan_type")
                .and_then(Value::as_str)
                .map(str::to_owned),
            limit_reached: limits
                .get("limit_reached")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            windows,
        })
    }

    /// The window matching a nominal duration, if the backend reported one.
    fn window_of(&self, nominal: u64) -> Option<&Window> {
        self.windows
            .iter()
            .find(|window| window.window_minutes.is_some_and(|m| matches(m, nominal)))
    }

    /// Response headers carrying this snapshot, in the form the client parses.
    ///
    /// Utilization is a fraction rather than a percentage — that is the form
    /// the header takes. Only windows that genuinely match a slot appear: a
    /// thirty-day window announced as a five-hour one would show a meter that
    /// is wrong in the reassuring direction.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = Vec::new();

        for (nominal, utilization, reset) in [
            (
                FIVE_HOURS,
                "anthropic-ratelimit-unified-5h-utilization",
                "anthropic-ratelimit-unified-5h-reset",
            ),
            (
                SEVEN_DAYS,
                "anthropic-ratelimit-unified-7d-utilization",
                "anthropic-ratelimit-unified-7d-reset",
            ),
        ] {
            let Some(window) = self.window_of(nominal) else {
                continue;
            };
            headers.push((utilization, format!("{:.4}", window.used_percent / 100.0)));
            if let Some(resets_at) = window.resets_at {
                headers.push((reset, resets_at.to_string()));
            }
        }

        // Said only when a window was reported, because "allowed" asserts
        // something about a limit, and no limit was seen means no assertion.
        if !headers.is_empty() {
            headers.push((
                "anthropic-ratelimit-unified-status",
                if self.limit_reached {
                    "rejected".to_owned()
                } else {
                    "allowed".to_owned()
                },
            ));
        }

        headers
    }

    /// The snapshot as the control socket reports it, windows and all.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "known": true,
            "plan": self.plan,
            "limit_reached": self.limit_reached,
            "windows": self.windows.iter().map(|window| serde_json::json!({
                "used_percent": window.used_percent,
                "window_minutes": window.window_minutes,
                "resets_at": window.resets_at,
            })).collect::<Vec<_>>(),
        })
    }
}

fn parse_window(value: &Value) -> Option<Window> {
    // A window with no percentage says nothing, and reporting it as zero used
    // would be a figure the backend never gave.
    let used_percent = value.get("used_percent").and_then(Value::as_f64)?;

    Some(Window {
        used_percent: used_percent.clamp(0.0, 100.0),
        window_minutes: value.get("window_minutes").and_then(Value::as_u64),
        resets_at: value.get("reset_at").and_then(Value::as_u64),
    })
}

/// The most recent snapshot, for whoever asks between turns.
#[derive(Debug, Default)]
pub struct UsageStore {
    latest: Mutex<Option<Snapshot>>,
    /// Every model id a turn has actually been made against.
    ///
    /// The configured tiers are the ids this daemon is *set up* to serve; an id
    /// the client sent itself passes straight through and is never one of them.
    /// Both are needed to answer "is this session mine" for a status line, and
    /// only a turn can report the second kind.
    served: Mutex<std::collections::BTreeSet<String>>,
}

impl UsageStore {
    pub fn record(&self, snapshot: &Snapshot) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(snapshot.clone());
        }
    }

    pub fn latest(&self) -> Option<Snapshot> {
        self.latest.lock().ok().and_then(|latest| latest.clone())
    }

    pub fn record_model(&self, model: &str) {
        if let Ok(mut served) = self.served.lock()
            && !served.contains(model)
        {
            served.insert(model.to_owned());
        }
    }

    pub fn served(&self) -> Vec<String> {
        self.served
            .lock()
            .map(|served| served.iter().cloned().collect())
            .unwrap_or_default()
    }
}
