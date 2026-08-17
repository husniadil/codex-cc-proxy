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

    /// Read a snapshot out of the quota endpoint's response.
    ///
    /// **The shape is not the stream event's**, and the three differences are
    /// exactly where a guess would have gone wrong: the windows are
    /// `primary_window`/`secondary_window` rather than `primary`/`secondary`,
    /// their length is stated in **seconds**, and the plan sits at the top
    /// level. A parser written from the stream shape parses this into nothing
    /// and reports no quota on an account that has one — which is why the
    /// fixture behind this was captured before any of it was written.
    ///
    /// `None` where the body is not this response at all. An empty snapshot
    /// would read as "quota known, nothing used", which is the reassuring
    /// direction to be wrong in.
    pub fn parse_rest(payload: &str) -> Option<Self> {
        let body: Value = serde_json::from_str(payload).ok()?;
        let limits = body.get("rate_limit")?;

        let windows: Vec<Window> = ["primary_window", "secondary_window"]
            .into_iter()
            .filter_map(|name| parse_rest_window(limits.get(name)?))
            .collect();

        // A response carrying no window at all says nothing about quota, and
        // saying nothing is not the same as saying none is used.
        if windows.is_empty() {
            return None;
        }

        Some(Self {
            plan: body
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

/// One window from the quota endpoint.
///
/// Seconds on the wire, minutes in the snapshot — the unit every other reader
/// of a window already uses, and converting here is what lets one `Snapshot`
/// serve both sources.
fn parse_rest_window(value: &Value) -> Option<Window> {
    let used_percent = value.get("used_percent").and_then(Value::as_f64)?;

    Some(Window {
        used_percent: used_percent.clamp(0.0, 100.0),
        window_minutes: value
            .get("limit_window_seconds")
            .and_then(Value::as_u64)
            .map(|seconds| seconds / 60),
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

/// Ask the backend for a quota figure, rather than waiting for one.
///
/// **This is not the primary path and does not replace it.** The backend
/// volunteers a snapshot at the head of every stream; that one is free, rides a
/// turn already being made, and is what `usage` reports. This exists for the
/// case that one cannot cover: a front-end showing a figure on a daemon that
/// has served no turn yet, where the alternative is showing nothing at all.
///
/// Nothing here is computed. The response is projected into the same `Snapshot`
/// the stream path produces, and a window the backend did not report is absent
/// rather than zero.
pub async fn fetch(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    account_id: Option<&str>,
) -> Result<Snapshot, crate::error::ProxyError> {
    let mut request = client
        .get(endpoint)
        .bearer_auth(token)
        .header("originator", crate::upstream::http::ORIGINATOR)
        .header(
            axum::http::header::USER_AGENT,
            crate::upstream::http::USER_AGENT,
        );

    if let Some(account) = account_id {
        request = request.header("chatgpt-account-id", account);
    }

    let response = request.send().await.map_err(|error| {
        crate::error::ProxyError::upstream(
            axum::http::StatusCode::BAD_GATEWAY,
            format!("could not ask for a quota figure: {error}"),
        )
    })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(crate::error::ProxyError::upstream(
            status,
            format!("the quota endpoint answered {status}"),
        ));
    }

    Snapshot::parse_rest(&body).ok_or_else(|| {
        crate::error::ProxyError::upstream(
            axum::http::StatusCode::BAD_GATEWAY,
            "the quota endpoint answered with a shape this proxy does not recognize",
        )
    })
}
