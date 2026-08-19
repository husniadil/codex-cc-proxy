//! What the control socket's methods actually do.
//!
//! The daemon holds authoritative state and every front-end is a client of this
//! interface. The CLI has no privileged path of its own, so a second front-end
//! needs no new daemon work.

use crate::auth::store::CredentialStore;
use crate::catalog::Catalog;
use crate::error::ProxyError;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

/// The range the client accepts for `CLAUDE_CODE_AUTO_COMPACT_WINDOW`.
///
/// Read from the client, not chosen here: it answers anything else with
/// "Expected 'auto' or 100k–1M tokens", and the settings key of the same
/// meaning is declared to discard an out-of-range value silently. Both ends
/// matter — a small model can fall under the floor, and a large window with a
/// low `upstream.effective_window_percent` can too.
const COMPACT_WINDOW_FLOOR: u64 = 100_000;
const COMPACT_WINDOW_CEILING: u64 = 1_000_000;

/// Everything the methods read.
#[derive(Clone)]
pub struct ControlState {
    pub port: u16,
    /// The live policy: the tier mapping and the effort ceiling, shared with
    /// the ingress so a change here moves what routes turns rather than only
    /// what this socket reports.
    pub policy: Arc<crate::policy::Policy>,
    pub catalog: Arc<Catalog>,
    pub credentials: Arc<dyn CredentialStore>,
    /// The same switches the ingress path reads, so starting a capture here
    /// changes what the next turn does.
    pub capture: Arc<crate::recorder::Switches>,
    /// The same store the ingress path writes to, so this reports the quota as
    /// of the last turn rather than a figure of its own.
    pub usage: Arc<crate::usage::UsageStore>,
    /// The authorization flow, if one is running. Held here because there is
    /// exactly one callback port and every front-end shares it.
    pub login: Arc<crate::auth::daemon_login::LoginFlow>,
    /// Policy the client applies to itself, published for whoever starts it.
    ///
    /// Read once at startup, like the instructions: nothing routes a turn from
    /// it, so a change that outlives the process belongs in the file where the
    /// comments explaining it live.
    pub client: Arc<crate::config::ClientConfig>,
    /// The grant, for the two things this socket reports about it: whether it
    /// has been refused, and the token a quota request is made with. `None`
    /// where the daemon holds no credentials at all.
    pub tokens: Option<Arc<crate::auth::tokens::TokenSource>>,
    pub usage_endpoint: String,
    /// Where a persisted change is written. `None` in tests, which must never
    /// touch an operator's file.
    pub config_path: Option<std::path::PathBuf>,
}

/// Dispatch one method.
pub async fn dispatch(
    state: &ControlState,
    method: &str,
    params: Option<&Value>,
) -> Result<Value, ProxyError> {
    match method {
        "status" => Ok(status(state)),
        "models" => Ok(models(state)),
        "tiers.get" => Ok(tiers(state)),
        // Two halves, because the client has two configuration surfaces and
        // only one of them is the environment. `variables` keeps the shape it
        // has always had; `settings` is additive, and absent when there is no
        // policy to state.
        "env" => Ok(match state.client.settings() {
            Some(settings) => json!({ "variables": environment(state), "settings": settings }),
            None => json!({ "variables": environment(state) }),
        }),
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
            state.capture.start(mode);
            Ok(json!({ "recording": true, "mode": requested }))
        }
        "record.stop" => {
            state.capture.stop();
            Ok(json!({ "recording": false }))
        }
        "login" => login(state).await,
        "login.cancel" => Ok(json!({ "cancelled": state.login.cancel() })),
        "tiers.set" => set_tiers(state, params),
        "effort.set" => set_effort(state, params),
        "usage.refresh" => refresh_usage(state).await,
        "doctor" => Err(ProxyError::invalid_request(format!(
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
        .map(|credentials| {
            let id_token = credentials.id_token.as_deref();

            // Two sources know the plan, and they can disagree. The backend
            // reports it on every turn; the grant reports what it was when the
            // operator last authenticated and is never updated unless a refresh
            // happens to return a new id token. The live one wins, and which
            // one answered is said out loud — a plan read to explain an
            // entitlement refusal is worth nothing if its age is unknown.
            let (plan, source) = match state.usage.latest().and_then(|latest| latest.plan) {
                Some(plan) => (Some(plan), "backend"),
                None => (crate::auth::jwt::plan(id_token), "grant"),
            };

            json!({
                "connected": true,
                // A grant the backend has refused is terminal: every turn after
                // it fails, and nothing else here would say so — `connected`
                // stays true because the credential file is still readable. A
                // front-end that could not tell would show a healthy provider
                // while every dispatch failed.
                "dead": state.tokens.as_ref().is_some_and(|tokens| tokens.is_dead()),
                "account_id": credentials.account_id,
                // Reported, never acted on. Null where neither source said
                // anything: a defaulted plan would either explain away a
                // refusal that has another cause or deny one that is real.
                "plan": plan,
                "plan_source": source,
                "email": crate::auth::jwt::email(id_token),
                "expires_at": credentials.expires_at,
            })
        })
        .unwrap_or_else(|| json!({ "connected": false }));

    json!({
        "port": state.port,
        "base_url": format!("http://127.0.0.1:{}", state.port),
        "auth": authenticated,
        "tiers": tier_map(state),
        // The ceiling, because a capped turn SUCCEEDS. It is simply shallower
        // than it was asked to be, and nothing else anywhere would ever mention
        // that every request a front-end makes is being capped. Null means no
        // ceiling, which is not the same as a ceiling at the highest value.
        "effort_ceiling": state
            .policy
            .get()
            .effort_ceiling()
            .and_then(|effort| serde_json::to_value(effort).ok()),
        // Mapped models the catalog knows but withholds. These pass validation,
        // so without this nothing would ever mention that a tier points at a
        // model the backend does not offer. Present and empty rather than
        // absent, so "nothing withheld" is distinguishable from "not reported".
        "unlisted_tiers": state.catalog.unlisted(&mapped_models(state)),
        // Whether the catalog is the backend's or the fallback list. A caller
        // that cannot tell would report an unvalidated mapping as a validated
        // one.
        "catalog_authoritative": state.catalog.authoritative,
        // Policy this daemon publishes for whoever starts the client. Reported
        // under the configuration's own key names, because the person reading
        // this arrived holding "Skill execution blocked by permission rules" —
        // a message that names nobody — and what they need next is the key that
        // undoes it.
        "client": {
            "deny_skills": state.client.deny_skills,
            "disable_connectors": state.client.disable_connectors,
        },
        "recording": state.capture.any(),
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

/// Every model a tier points at, once each.
fn mapped_models(state: &ControlState) -> Vec<String> {
    state
        .policy
        .get()
        .tiers()
        .iter()
        .map(|tier| tier.model.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn tier_map(state: &ControlState) -> Value {
    let mut map = serde_json::Map::new();
    for tier in state.policy.get().tiers().iter() {
        map.insert(tier.tier.to_owned(), Value::from(tier.model.clone()));
    }
    Value::Object(map)
}

/// What quota is left, as of the last turn.
///
/// Read from the snapshot the backend opens each stream with, never polled and
/// never computed. Before a turn has been made there is nothing to report, and
/// that is said rather than answered with zeroes — an invented quota figure
/// reads as headroom that may not be there.
fn usage(state: &ControlState) -> Value {
    let mut answer = match state.usage.latest() {
        Some(snapshot) => snapshot.to_json(),
        None => json!({
            "known": false,
            "detail": "the backend reports quota when a turn is made; none has been made yet",
        }),
    };

    // Which sessions this quota belongs to. A status line is configured once and
    // renders for every session the client runs, including sessions pointed
    // somewhere else entirely; without this it would paint one account's quota
    // over another's. Reported whether or not a quota is known, because the
    // question is about who is asking rather than about the answer.
    //
    // The configured tiers and the ids turns were actually made against: a
    // client that names a model itself passes it straight through, so the tiers
    // alone would not recognize its own session.
    let mut served: Vec<String> = state
        .policy
        .get()
        .tiers()
        .iter()
        .map(|tier| tier.model.clone())
        .chain(state.usage.served())
        .collect();
    served.sort_unstable();
    served.dedup();
    if let Some(object) = answer.as_object_mut() {
        object.insert("models".to_owned(), json!(served));
    }
    answer
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

    for tier in state.policy.get().tiers().iter() {
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
        .policy
        .get()
        .tiers()
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
        //
        // Only within the range the client will accept. Its own parser answers
        // anything else with "Expected 'auto' or 100k–1M tokens", and the
        // equivalent settings key is declared to *discard* an out-of-range
        // value rather than reject it. A figure outside the range is therefore
        // not an early compaction or a late one — it is no setting at all, and
        // nothing would say so. Reported instead, where a reader can see it.
        if (COMPACT_WINDOW_FLOOR..=COMPACT_WINDOW_CEILING).contains(&window) {
            variables.push((
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_owned(),
                window.to_string(),
            ));
        } else {
            tracing::warn!(
                window,
                floor = COMPACT_WINDOW_FLOOR,
                ceiling = COMPACT_WINDOW_CEILING,
                "the effective window is outside the range the client accepts for \
                 auto-compaction, so it is not set; the client will use its own default \
                 and may compact later than this window allows"
            );
        }
    }

    // Inert for ordinary model ids, and set as a one-sided floor: should a
    // future client classify unknown ids as long-context, the assumption is
    // pinned down rather than raised (§7.2).
    variables.push(("CLAUDE_CODE_DISABLE_1M_CONTEXT".to_owned(), "1".to_owned()));
    variables
}

/// `tiers.set` — point a tier at a different model, on a running daemon.
///
/// **Validated against the catalog, exactly as startup validates it.** That
/// check is the entire reason this daemon owns the mapping rather than a
/// front-end: it is the side holding the catalog. Skipping it here would let a
/// caller point a tier at a model the backend will not serve, and the failure
/// would arrive a turn later as a refusal the client cannot act on.
///
/// **Partial.** Naming one tier changes that tier. The alternative — treating
/// the argument as the whole mapping — would let a caller that knows about one
/// tier silently unset the three it did not mention.
///
/// **It does not touch the configuration file.** The file is what the daemon
/// reads at startup, so a change made here lasts until it stops; that is
/// reported back rather than left to be discovered. Writing the file would mean
/// rewriting a document whose comments explain why each key is what it is, and
/// a change that outlives the process should be made where those comments are.
fn set_tiers(state: &ControlState, params: Option<&Value>) -> Result<Value, ProxyError> {
    let requested = params
        .and_then(|params| params.get("tiers"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ProxyError::invalid_request(
                "`tiers.set` needs a `tiers` object, for example {\"tiers\":{\"sonnet\":\"…\"}}",
            )
        })?;

    if requested.is_empty() {
        return Err(ProxyError::invalid_request(
            "`tiers.set` was given no tiers to set",
        ));
    }

    let mut tiers = state.policy.get().tiers().to_vec();

    for (name, model) in requested {
        let model = model.as_str().ok_or_else(|| {
            ProxyError::invalid_request(format!("the model for `{name}` must be a string"))
        })?;
        // A blank is a mistake rather than a preference — the same rule the
        // configuration applies, for the same reason.
        if model.trim().is_empty() {
            return Err(ProxyError::invalid_request(format!(
                "the model for `{name}` is blank; a tier cannot be unset, only pointed elsewhere"
            )));
        }

        let entry = tiers
            .iter_mut()
            .find(|tier| tier.tier == name.as_str())
            .ok_or_else(|| {
                ProxyError::invalid_request(format!(
                    "unknown tier `{name}`; expected one of: {}",
                    crate::config::TIER_NAMES.join(", ")
                ))
            })?;

        entry.model = model.to_owned();
        // Set by the operator, so the catalog may never overrule it — the same
        // meaning `defaulted` carries when the mapping comes from the file.
        entry.defaulted = false;
    }

    state.catalog.validate(
        &tiers
            .iter()
            .map(|tier| tier.model.clone())
            .collect::<Vec<_>>(),
    )?;

    // Persisting is asked for, never assumed. A front-end changing a mapping to
    // try something is not the same as an operator changing what this daemon is,
    // and only the caller knows which it is doing.
    let persist = params
        .and_then(|params| params.get("persist"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Written BEFORE it is applied. A write that fails leaves the daemon as it
    // was, so the error the caller gets is the whole story; applying first
    // would leave it running a policy nobody chose, reported as a failure, and
    // gone at the next restart.
    let persisted = if persist {
        write_config(state, |document| {
            let mut document = document.to_owned();
            for (name, model) in requested {
                // Every value was checked to be a non-blank string above, so
                // this cannot be reached with anything else.
                let model = model.as_str().unwrap_or_default();
                document = crate::config::edit::set_tier(&document, name, model)?;
            }
            Ok(document)
        })?
    } else {
        false
    };

    state.policy.set_tiers(tiers);

    Ok(json!({
        "tiers": tier_map(state),
        // Said out loud, every time. A caller that believed this persisted
        // would find the old mapping back after a restart and have no way to
        // know why.
        "persisted": persisted,
        "detail": if persisted {
            "in effect now, and written to the configuration file"
        } else {
            "in effect until the daemon stops; the configuration file is unchanged"
        },
    }))
}

/// Apply an edit to the configuration file's text.
///
/// The file is read fresh rather than rewritten from anything held in memory:
/// the operator may have edited it since this daemon started, and overwriting
/// those edits to persist an unrelated one is not a trade this makes.
fn write_config(
    state: &ControlState,
    edit: impl FnOnce(&str) -> Result<String, ProxyError>,
) -> Result<bool, ProxyError> {
    let Some(path) = state.config_path.as_ref() else {
        return Err(ProxyError::invalid_request(
            "this daemon has no configuration file to write",
        ));
    };

    // A missing file is a first run, not a failure — every key has a default.
    // Starting from the shipped example rather than an empty document keeps the
    // comments that explain what else can be set.
    let document = match std::fs::read_to_string(path) {
        Ok(document) => document,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::config::EXAMPLE.to_owned()
        }
        Err(error) => {
            return Err(ProxyError::invalid_request(format!(
                "could not read {}: {error}",
                path.display()
            )));
        }
    };

    let written = edit(&document)?;

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, written).map_err(|error| {
        ProxyError::invalid_request(format!("could not write {}: {error}", path.display()))
    })?;

    Ok(true)
}

/// `effort.set` — raise, lower, or remove the operator's ceiling on reasoning
/// effort, on a running daemon.
///
/// The ceiling caps every turn regardless of what the client asked for, and a
/// capped turn **succeeds** — it is simply shallower than it was asked to be.
/// Nothing about that is visible to the caller that asked for more, which is
/// why it is worth being able to change without a restart: a ceiling set once
/// for one purpose otherwise silently governs every front-end that arrives
/// later.
///
/// `null` removes it. That is not the same as setting it to the highest value
/// the catalog happens to list: with no ceiling the only cap left is the
/// model's own, which is the thing the client cannot anticipate and this proxy
/// applies for it.
fn set_effort(state: &ControlState, params: Option<&Value>) -> Result<Value, ProxyError> {
    let requested = params
        .and_then(|params| params.get("effort"))
        .ok_or_else(|| {
            ProxyError::invalid_request(
                "`effort.set` needs an `effort`, or null to remove the ceiling",
            )
        })?;

    let ceiling = match requested {
        Value::Null => None,
        Value::String(name) => Some(crate::config::parse_effort(name)?),
        other => {
            return Err(ProxyError::invalid_request(format!(
                "`effort` must be a string or null, not {other}"
            )));
        }
    };

    let persist = params
        .and_then(|params| params.get("persist"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Written before it is applied, for the same reason `tiers.set` is.
    let persisted = if persist {
        let effort = requested.as_str().map(str::to_owned);
        write_config(state, |document| {
            crate::config::edit::set_effort(document, effort.as_deref())
        })?
    } else {
        false
    };

    state.policy.set_effort_ceiling(ceiling);

    Ok(json!({
        "effort": requested.clone(),
        "persisted": persisted,
        "detail": if persisted {
            "in effect now, and written to the configuration file"
        } else {
            "in effect until the daemon stops; the configuration file is unchanged"
        },
    }))
}

/// `login` — start the authorization flow, or join the one already running.
///
/// Answers with the URL and returns; the flow completes in the background and
/// `status` is what says whether it did. A control call that blocked until the
/// operator finished in a browser would hold the socket for minutes and give a
/// front-end nothing to render in the meantime.
async fn login(state: &ControlState) -> Result<Value, ProxyError> {
    let started = state.login.start(Arc::clone(&state.credentials)).await?;

    Ok(json!({
        "authorization_url": started.url,
        // Whether this call started the flow or joined one. A caller that
        // cannot tell would report a second operator's login as its own.
        "already_in_flight": started.already_in_flight,
        "detail": "open the URL to authorize; `status` reports when it completed",
    }))
}

/// `usage.refresh` — ask the backend for a quota figure now.
///
/// The free path stays the primary one: the backend volunteers a snapshot at
/// the head of every stream, and `usage` reports that. This is for the case
/// that path cannot cover — a front-end with a figure to show on a daemon that
/// has served no turn yet.
async fn refresh_usage(state: &ControlState) -> Result<Value, ProxyError> {
    let Some(tokens) = state.tokens.as_ref() else {
        return Err(ProxyError::authentication(
            "there are no credentials to ask with; run `login` first",
        ));
    };

    let token = tokens.access_token().await?;
    let snapshot = crate::usage::fetch(
        &reqwest::Client::new(),
        &state.usage_endpoint,
        &token,
        tokens.account_id().as_deref(),
    )
    .await?;

    // Recorded where the stream path records its own, so everything that reads
    // a quota reads one value. A figure asked for and a figure volunteered are
    // the same figure from the same account.
    state.usage.record(&snapshot);
    Ok(snapshot.to_json())
}
