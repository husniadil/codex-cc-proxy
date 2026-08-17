//! What the control socket's methods actually do.
//!
//! The daemon holds authoritative state and every front-end is a client of this
//! interface. The CLI has no privileged path of its own, so a second front-end
//! needs no new daemon work.

use crate::auth::store::CredentialStore;
use crate::catalog::Catalog;
use crate::config::ResolvedTier;
use crate::error::ProxyError;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

/// Everything the methods read.
#[derive(Clone)]
pub struct ControlState {
    pub port: u16,
    pub tiers: Arc<Vec<ResolvedTier>>,
    pub catalog: Arc<Catalog>,
    pub credentials: Arc<dyn CredentialStore>,
    /// The same switches the ingress path reads, so starting a capture here
    /// changes what the next turn does.
    pub recording: Arc<crate::recorder::Switches>,
}

/// Dispatch one method.
pub fn dispatch(
    state: &ControlState,
    method: &str,
    params: Option<&Value>,
) -> Result<Value, ProxyError> {
    match method {
        "status" => Ok(status(state)),
        "models" => Ok(models(state)),
        "tiers.get" => Ok(tiers(state)),
        "env" => Ok(json!({ "variables": environment(state) })),
        "usage" => Ok(usage(state)),
        "disconnect" => {
            state.credentials.clear()?;
            Ok(json!({ "disconnected": true }))
        }
        "record.start" => {
            // Defaulting to ingress is the safe default and the documented
            // one: it is the mode that needs no credentials and spends
            // nothing. Starting an upstream capture has to be asked for by
            // name, because it bills every turn that follows.
            let requested = params
                .and_then(|params| params.get("mode"))
                .and_then(Value::as_str)
                .unwrap_or("ingress");
            let mode = match requested {
                "ingress" => crate::recorder::Mode::Ingress,
                "upstream" => crate::recorder::Mode::Upstream,
                other => {
                    return Err(ProxyError::invalid_request(format!(
                        "unknown capture mode `{other}`; expected `ingress` or `upstream`"
                    )));
                }
            };
            state.recording.start(mode);
            Ok(json!({ "recording": true, "mode": requested }))
        }
        "record.stop" => {
            state.recording.stop();
            Ok(json!({ "recording": false }))
        }
        "login" | "doctor" | "tiers.set" => Err(ProxyError::invalid_request(format!(
            "`{method}` is not implemented yet"
        ))),
        other => Err(ProxyError::not_found(format!("unknown method `{other}`"))),
    }
}

fn status(state: &ControlState) -> Value {
    let authenticated = state
        .credentials
        .load()
        .ok()
        .flatten()
        .map(|credentials| credentials.account_id)
        .map(|account| json!({ "connected": true, "account_id": account }))
        .unwrap_or_else(|| json!({ "connected": false }));

    json!({
        "port": state.port,
        "base_url": format!("http://127.0.0.1:{}", state.port),
        "auth": authenticated,
        "tiers": tier_map(state),
        // Whether the catalog is the backend's or the fallback list. A caller
        // that cannot tell would report an unvalidated mapping as a validated
        // one.
        "catalog_authoritative": state.catalog.authoritative,
        "recording": state.recording.any(),
    })
}

fn models(state: &ControlState) -> Value {
    let entries: Vec<Value> = state
        .catalog
        .selectable()
        .iter()
        .map(|model| {
            json!({
                "id": model.id,
                // Null rather than a number where the window is unknown. A
                // figure here would be invented, and §7.0 forbids that.
                "context_window": model.context_window,
                "effective_window": model.effective_window(),
            })
        })
        .collect();

    json!({ "models": entries, "authoritative": state.catalog.authoritative })
}

fn tiers(state: &ControlState) -> Value {
    json!({ "tiers": tier_map(state) })
}

fn tier_map(state: &ControlState) -> Value {
    let mut map = serde_json::Map::new();
    for tier in state.tiers.iter() {
        map.insert(tier.tier.to_owned(), Value::from(tier.model.clone()));
    }
    Value::Object(map)
}

/// Quota is reported by the backend on live responses. Until one has been seen,
/// saying so is the honest answer — a zeroed window would read as "no quota
/// used" rather than "not yet known".
fn usage(_state: &ControlState) -> Value {
    json!({
        "known": false,
        "detail": "quota is reported by the backend on a completed response; none has been seen yet",
    })
}

/// `docs/api.md` §2.1 — the environment Claude Code needs.
///
/// All four tier variables are always emitted. `WebFetch` runs on the haiku
/// tier, so an unmapped haiku breaks it in a way that looks unrelated to tier
/// mapping.
pub fn environment(state: &ControlState) -> Vec<(String, String)> {
    let mut variables = vec![
        (
            "ANTHROPIC_BASE_URL".to_owned(),
            format!("http://127.0.0.1:{}", state.port),
        ),
        // Must be set for the client's sake. Its value is ignored.
        ("ANTHROPIC_AUTH_TOKEN".to_owned(), "unused".to_owned()),
    ];

    for tier in state.tiers.iter() {
        variables.push((
            format!("ANTHROPIC_DEFAULT_{}_MODEL", tier.tier.to_uppercase()),
            tier.model.clone(),
        ));
    }

    // §7.2 — the real window, where the catalog knows it.
    //
    // The client cannot recognize these model ids, so it assumes 200,000 and
    // says so. That assumption is safe but wrong: it compacts a session with a
    // quarter of its context still unused. Stating the figure replaces a guess
    // with a measurement.
    //
    // The smallest window across the mapped tiers, because one value covers
    // them all and the smallest is the only one that cannot overrun. The
    // effective window rather than the raw one, for the same reason the guard
    // uses it (§7.0): what is left after instructions, tools and output.
    if let Some(window) = state
        .tiers
        .iter()
        .filter_map(|tier| state.catalog.get(&tier.model))
        .filter_map(crate::catalog::Model::effective_window)
        .min()
    {
        variables.push((
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_owned(),
            window.to_string(),
        ));

        // And compact at it.
        //
        // Stating the window alone is worse than saying nothing: the client
        // stops applying its own 200,000 assumption and, not recognizing the
        // model, enforces no limit at all — the session grows until the backend
        // refuses it. Early compaction wastes context; late compaction fails
        // the session, and this is the setting that decides which (§7.2).
        variables.push((
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_owned(),
            window.to_string(),
        ));
    }

    // Inert for ordinary model ids, and set as a one-sided floor: should a
    // future client classify unknown ids as long-context, the assumption is
    // pinned down rather than raised (§7.2).
    variables.push(("CLAUDE_CODE_DISABLE_1M_CONTEXT".to_owned(), "1".to_owned()));
    variables
}
