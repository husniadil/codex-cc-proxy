//! Turning control-socket results into terminal output.
//!
//! Presentation only. The daemon holds the state and decides what is true; this
//! decides how it reads.

use serde_json::Value;

/// `docs/api.md` §2.1 — shell exports.
pub fn env_shell(result: &Value) -> String {
    variables(result)
        .into_iter()
        .map(|(name, value)| format!("export {name}={}", quote(&value)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The same, as a settings fragment.
pub fn env_json(result: &Value) -> String {
    let map: serde_json::Map<String, Value> = variables(result)
        .into_iter()
        .map(|(name, value)| (name, Value::from(value)))
        .collect();

    serde_json::to_string_pretty(&serde_json::json!({ "env": map })).unwrap_or_default()
}

fn variables(result: &Value) -> Vec<(String, String)> {
    result
        .get("variables")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry.get(0)?.as_str()?.to_owned(),
                        entry.get(1)?.as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value.get(name)
}

/// Quote only when the value needs it, so the common case stays readable.
fn quote(value: &str) -> String {
    let safe = value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':')
    });
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

pub fn status(result: &Value) -> String {
    let mut lines = Vec::new();

    lines.push(format!(
        "base url   {}",
        field(result, "base_url")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    ));

    let auth = field(result, "auth");
    let connected = auth
        .and_then(|auth| field(auth, "connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    lines.push(if connected {
        let account = auth
            .and_then(|auth| field(auth, "account_id"))
            .and_then(Value::as_str)
            .unwrap_or("account unknown");
        format!("auth       connected ({account})")
    } else {
        "auth       not connected — run `codex-cc-proxy login`".to_owned()
    });

    if let Some(tiers) = field(result, "tiers").and_then(Value::as_object) {
        for (tier, model) in tiers {
            lines.push(format!(
                "{tier:<10} {}",
                model.as_str().unwrap_or("unmapped")
            ));
        }
    }

    // Whether the mapping was checked against the backend's own catalog. A
    // reader who cannot tell would take an unvalidated mapping for a validated
    // one.
    if field(result, "catalog_authoritative").and_then(Value::as_bool) == Some(false) {
        lines.push("catalog    unavailable — the tier mapping has not been validated".to_owned());
    }

    if field(result, "recording").and_then(Value::as_bool) == Some(true) {
        lines.push("recording  on".to_owned());
    }

    lines.join("\n")
}

pub fn models(result: &Value) -> String {
    let mut lines = Vec::new();

    if field(result, "authoritative").and_then(Value::as_bool) == Some(false) {
        lines.push("(the catalog could not be fetched; this is the fallback list)".to_owned());
    }

    for model in field(result, "models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = field(model, "id").and_then(Value::as_str).unwrap_or("?");
        // "unknown" rather than a number. Printing a figure nobody measured is
        // how an assumption becomes a fact.
        let window = field(model, "context_window")
            .and_then(Value::as_u64)
            .map(|window| format!("{window} tokens"))
            .unwrap_or_else(|| "window unknown".to_owned());
        lines.push(format!("{id:<24} {window}"));
    }

    lines.join("\n")
}
