//! What the control socket's methods actually do.
//!
//! The daemon holds authoritative state and every front-end is a client of this
//! interface. The CLI has no privileged path of its own, so a second front-end
//! needs no new daemon work.

use crate::auth::authorize::Authorizer;
use crate::auth::store::AccountStore;
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
    pub catalog: Arc<crate::catalog::CatalogSource>,
    pub credentials: Arc<dyn AccountStore>,
    /// The same switches the ingress path reads, so starting a capture here
    /// changes what the next turn does.
    pub capture: Arc<crate::recorder::Switches>,
    /// The same store the ingress path writes to, so this reports the quota as
    /// of the last turn rather than a figure of its own.
    pub usage: Arc<crate::usage::UsageStore>,
    /// The authorization flow, if one is running. Held here because there is
    /// exactly one callback port and every front-end shares it.
    pub login: Arc<crate::auth::daemon_login::LoginFlow>,
    /// The configuration this daemon started from.
    ///
    /// Read once at startup, and that is the model: nothing here routes a turn
    /// by itself, and a change meant to outlive the process belongs in the file
    /// where the comments explaining it live. What this socket can move on a
    /// running daemon it moves through `policy`, not by re-reading this.
    ///
    /// Held whole rather than as the two slices that were needed first. An
    /// account's tier mapping has to be resolved against the shared tables at
    /// the moment a switch happens, so the shared tables have to be here.
    pub config: Arc<crate::config::Config>,
    /// The stop signal the daemon's own run loop waits on. Shared, so asking
    /// here moves the process rather than only answering about it.
    pub shutdown: Arc<crate::daemon::Shutdown>,
    /// The grant, for the two things this socket reports about it: whether it
    /// has been refused, and the token a quota request is made with. `None`
    /// where the daemon holds no credentials at all.
    pub tokens: Option<Arc<crate::auth::tokens::TokenSource>>,
    pub usage_endpoint: String,
    /// The live conversations. A switch has to reach them: a conduit fixes its
    /// account on the connection at dial and reuses it for the conversation's
    /// life, so a session left alone keeps being served as the account it
    /// started on.
    pub sessions: Arc<crate::session::SessionStore>,
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
        // has always had; `settings` is additive, and **always present** — an
        // empty object where there is no policy. Absence is reserved for a
        // daemon that predates this, which is the ordinary state after an
        // upgrade that has not restarted anything.
        "env" => Ok(json!({
            "variables": environment(state),
            "settings": state.config.client.settings(),
        })),
        "usage" => Ok(usage(state)),
        "accounts.forget" => forget_account(state, params).await,
        "accounts" => accounts(state),
        "accounts.select" => select_account(state, params).await,
        "accounts.rename" => rename_account(state, params),
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
        // Answers, then goes. The release happens after this response is
        // written — see `Shutdown`. Under a supervisor this is how a running
        // daemon is replaced by the build on disk.
        "shutdown" => {
            state.shutdown.request();
            Ok(json!({ "stopping": true, "version": VERSION }))
        }
        "record.stop" => {
            state.capture.stop();
            Ok(json!({ "recording": false }))
        }
        "login" => login(state, params).await,
        "login.cancel" => Ok(json!({ "cancelled": state.login.cancel() })),
        "tiers.set" => set_tiers(state, params),
        "effort.set" => set_effort(state, params),
        "cross_account_tiers.set" => set_cross_account(state, params),
        "usage.refresh" => refresh_usage(state).await,
        "doctor" => Err(ProxyError::invalid_request(format!(
            "`{method}` is not implemented yet"
        ))),
        other => Err(ProxyError::not_found(format!("unknown method `{other}`"))),
    }
}

/// This binary's version, reported so a caller can see whether the daemon
/// answering it is the same build it was invoked from. One file is both, and
/// replacing it on disk does not restart what is already running.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Minted once, when this process starts.
///
/// A stop is observed by watching what answers afterwards, and "it went" cannot
/// be read from a socket falling silent: a supervisor that restarts inside the
/// first poll never leaves a gap, and one that throttles respawns leaves a gap
/// far longer than anything worth waiting for. Both make absence a statement
/// about timing rather than about the daemon. This changes exactly when the
/// process does, so the same observation holds either way.
static INSTANCE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| uuid::Uuid::new_v4().to_string());

fn status(state: &ControlState) -> Value {
    let stored = state.credentials.accounts().unwrap_or_default();
    let catalog = state.catalog.current();
    let serving = stored.iter().find(|account| account.selected);
    let authenticated = match serving {
        Some(account) => {
            // Two sources know the plan, and they can disagree. The backend
            // reports it on every turn; the grant reports what it was when the
            // operator last authenticated and is never updated unless a refresh
            // happens to return a new id token. The live one wins, and which
            // one answered is said out loud — a plan read to explain an
            // entitlement refusal is worth nothing if its age is unknown.
            //
            // A key has neither source. It reports no plan rather than a
            // plausible one, the same as every other field a grant carries and
            // it does not.
            let (plan, source) = match state.usage.latest().and_then(|latest| latest.plan) {
                Some(plan) => (Some(plan), Some("backend")),
                // Null rather than `grant` where there is no grant: naming a
                // source that does not exist for this account attributes a
                // missing figure to something that was never asked.
                None => match account.plan.clone() {
                    Some(plan) => (Some(plan), Some("grant")),
                    None => (None, None),
                },
            };

            json!({
                // Connected means there is a credential to spend, whichever
                // kind it is. Reading the grant alone reported a daemon that
                // could serve every turn as not connected, and advised a login
                // that would not have helped.
                "connected": true,
                // A grant the backend has refused is terminal: every turn after
                // it fails, and nothing else here would say so — `connected`
                // stays true because the credential is still there and still
                // readable. A front-end that could not tell would show a
                // healthy provider while every dispatch failed.
                "dead": state.tokens.as_ref().is_some_and(|tokens| tokens.is_dead()),
                // What this daemon calls the account, and what the backend
                // calls it. The first is what selects it; the second is what
                // appears on a request.
                "account": account.name.clone(),
                // What it authenticates with, because that decides which
                // endpoint it is spent against and what it can be asked for.
                "kind": account.kind,
                // The other half of that decision: which provider's endpoint.
                "provider": account.provider,
                "account_id": account.account_id.clone(),
                // Reported, never acted on. Null where neither source said
                // anything: a defaulted plan would either explain away a
                // refusal that has another cause or deny one that is real.
                "plan": plan,
                "plan_source": source,
                "email": account.email.clone(),
                "expires_at": account.expires_at,
                // Every stored account, so the answer that says a connection
                // exists also says what else could serve one. Present and
                // empty rather than absent.
                "accounts": &stored,
            })
        }
        // Present and empty rather than absent, on the answer a front-end most
        // wants it on: nothing is connected, and these are the accounts it
        // could connect as.
        None => json!({ "connected": false, "accounts": &stored }),
    };

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
        "unlisted_tiers": catalog.unlisted(&mapped_models(state)),
        // Whether the catalog is the backend's or the fallback list. A caller
        // that cannot tell would report an unvalidated mapping as a validated
        // one.
        "catalog_authoritative": catalog.authoritative,
        // Whether the list describes the account now serving turns. A switch
        // asks for it again, but best effort: a failed fetch keeps the list
        // already in force rather than withdrawing models the account has, so
        // it can still be the previous account's menu — and presenting that as
        // this one's would deny models this account has and offer models it
        // does not.
        "catalog_stale": catalog.is_stale_for(serving_account(state).as_deref()),
        "catalog_account": catalog.fetched_for.clone(),
        // The build actually serving this socket, which is not necessarily the
        // build the caller was invoked from.
        "version": VERSION,
        // This process, as distinct from any other that serves the same socket.
        "instance": &*INSTANCE,
        // Policy this daemon publishes for whoever starts the client. Reported
        // under the configuration's own key names, because the person reading
        // this arrived holding "Skill execution blocked by permission rules" —
        // a message that names nobody — and what they need next is the key that
        // undoes it.
        "client": {
            "deny_skills": state.config.client.deny_skills,
            "disable_connectors": state.config.client.disable_connectors,
        },
        "recording": state.capture.any(),
    })
}

fn models(state: &ControlState) -> Value {
    let catalog = state.catalog.current();
    let entries: Vec<Value> = catalog
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

    json!({
        "models": entries,
        "authoritative": catalog.authoritative,
        "stale": catalog.is_stale_for(serving_account(state).as_deref()),
    })
}

fn tiers(state: &ControlState) -> Value {
    json!({ "tiers": tier_map(state) })
}

/// Every model a tier points at, once each.
fn mapped_models(state: &ControlState) -> Vec<String> {
    // Relayed tiers excluded, for the reason validation excludes them: the
    // catalog is the first provider's menu (§9.1), so it has nothing to say
    // about whether an id on the other provider is offered or withheld.
    crate::upstream::relay::validated_models(
        &state.credentials.accounts().unwrap_or_default(),
        state.policy.get().tiers(),
    )
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>()
    .into_iter()
    .collect()
}

fn tier_map(state: &ControlState) -> Value {
    let mut map = serde_json::Map::new();
    for tier in state.policy.get().tiers().iter() {
        // The same two shapes the configuration takes: a string for the
        // serving account, an object where the tier pins another one.
        let value = match &tier.account {
            Some(account) => json!({ "account": account, "model": tier.model }),
            None => Value::from(tier.model.clone()),
        };
        map.insert(tier.tier.to_owned(), value);
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
    let catalog = state.catalog.current();
    let mut variables = vec![
        (
            "ANTHROPIC_BASE_URL".to_owned(),
            format!("http://127.0.0.1:{}", state.port),
        ),
        // Must be set for the client's sake. Its value is ignored.
        ("ANTHROPIC_AUTH_TOKEN".to_owned(), "unused".to_owned()),
    ];

    // The environment half of `client.disable_connectors`. The settings key
    // (`disableClaudeAiConnectors`) silences the client's connector notice;
    // this variable is the client's own documented opt-out for the
    // claude.ai-hosted servers themselves. One configuration key drives both,
    // because they are one intent — and without this half, a launch configured
    // by exports alone runs with the connectors it asked to disable. Whether
    // the current client still honours it is a §L question (`docs/roadmap.md`),
    // like the notice.
    if state.config.client.disable_connectors {
        variables.push(("ENABLE_CLAUDEAI_MCP_SERVERS".to_owned(), "false".to_owned()));
    }

    for tier in state.policy.get().tiers().iter() {
        variables.push((
            format!("ANTHROPIC_DEFAULT_{}_MODEL", tier.tier.to_uppercase()),
            tier.model.clone(),
        ));
    }

    // Which side of §9.1 each tier is on, asked once. Everything below turns on
    // it: the two window variables and the long-context flag are global to the
    // client, so a mapping that is not entirely on one provider has to pick.
    let accounts = state.credentials.accounts().unwrap_or_default();
    let relaying = |tier: &crate::config::ResolvedTier| {
        crate::upstream::relay::relays(&accounts, tier.account.as_deref())
    };
    let translating = state
        .policy
        .get()
        .tiers()
        .iter()
        .any(|tier| !relaying(tier));
    let relayed = state.policy.get().tiers().iter().any(&relaying);

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
    //
    // Not stated at all once any tier is relayed (§7.2). The client recognizes
    // those ids itself, this catalog is not their menu, and one variable
    // governs every tier — so the figure that covers a translated tier would
    // also govern a relayed one, where nothing else checks it: the translating
    // path has this proxy's own window guard behind it and the relay path has
    // none.
    if let Some(window) = state
        .policy
        .get()
        .tiers()
        .iter()
        .filter(|_| !relayed)
        .filter_map(|tier| catalog.get(&tier.model))
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

    // Load-bearing for an id the client cannot recognize: without it the client
    // appends `[1m]` and assumes four times the context the model has (§7.2).
    //
    // Omitted where every tier is relayed. The flag also strips
    // `context-1m-2025-08-07` from the beta list the client sends, so on that
    // path it denies an entitlement the account may hold, and there is no
    // unrecognized id left for it to protect. A split mapping keeps it: losing
    // an entitlement is a smaller session than it could have been, while a
    // fabricated million-token window is a session that overruns.
    if translating {
        variables.push(("CLAUDE_CODE_DISABLE_1M_CONTEXT".to_owned(), "1".to_owned()));
    }
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

    let snapshot = state.policy.get();
    let mut tiers = snapshot.tiers().to_vec();

    // Each value in the same two forms the configuration file takes: a model
    // id, or `{ account, model }` pinning the tier to another account.
    let mut changes: Vec<(&String, String, Option<String>)> = Vec::new();
    for (name, value) in requested {
        let (model, pin) = match value {
            Value::String(model) => (model.clone(), None),
            Value::Object(pinned) => {
                let field = |field: &str| {
                    pinned.get(field).and_then(Value::as_str).ok_or_else(|| {
                        ProxyError::invalid_request(format!(
                            "a pinned tier is {{\"account\": …, \"model\": …}}; \
                             `{name}` is missing `{field}`"
                        ))
                    })
                };
                (
                    field("model")?.to_owned(),
                    Some(field("account")?.to_owned()),
                )
            }
            _ => {
                return Err(ProxyError::invalid_request(format!(
                    "the value for `{name}` must be a model id, or \
                     {{\"account\": …, \"model\": …}}"
                )));
            }
        };

        // A blank is a mistake rather than a preference — the same rule the
        // configuration applies, for the same reason.
        if model.trim().is_empty() || pin.as_deref().is_some_and(|pin| pin.trim().is_empty()) {
            return Err(ProxyError::invalid_request(format!(
                "the model for `{name}` is blank; a tier cannot be unset, only pointed elsewhere"
            )));
        }

        // The write-time half of the consent gate. The startup half refuses
        // the file; this refuses the socket, so a front-end cannot write what
        // a restart would then refuse to load. Read from the live policy, not
        // the startup configuration: consent granted over the socket applies
        // to the next call, not the next restart.
        if pin.is_some() && snapshot.cross_account() == crate::config::CrossAccountTiers::Refused {
            return Err(ProxyError::invalid_request(format!(
                "pinning `{name}` to another account routes this client's traffic across \
                 accounts. That is a decision the operator owns: set \
                 `cross_account_tiers = true` in config.toml to permit it."
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

        entry.model = model.clone();
        entry.account = pin.clone();
        // Set by the operator, so the catalog may never overrule it — the same
        // meaning `defaulted` carries when the mapping comes from the file.
        entry.defaulted = false;
        changes.push((name, model, pin));
    }

    let (target, applies_now) = write_target(state, params)?;

    // Only against a catalog that can speak for the account being changed. The
    // list in force is the serving account's menu (§7.0), and a mapping written
    // for another account is not a claim about it — refusing `gpt-5.5` for a
    // spare account because the account serving turns is not offered it is the
    // exact case per-account mappings exist for. A pinned tier is that case in
    // one entry: its model belongs to the pinned account's menu, so it is
    // excluded here rather than refused over somebody else's list.
    if applies_now {
        // And only over the tiers this catalog is a menu for: a pinned entry
        // belongs to another account's list and a relayed one to another
        // provider's. `validated_models` holds both exclusions, and holding
        // them in one place is what keeps this door and the daemon's start
        // from disagreeing about what is valid.
        state
            .catalog
            .current()
            .validate(&crate::upstream::relay::validated_models(
                &state.credentials.accounts()?,
                &tiers,
            ))?;
    }

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
    if !applies_now && !persist {
        return Err(ProxyError::invalid_request(
            "that account is not the one serving turns, so this would change nothing \
             unless it is written: pass `persist`, or select the account first",
        ));
    }

    let persisted = if persist {
        write_config(state, |document| {
            let mut document = document.to_owned();
            for (name, model, pin) in &changes {
                // Written where the value is read from. An account section
                // shadows the shared table for the tiers it names (§4), so a
                // change written to the shared one would be in force on this
                // daemon and gone at the next start — written and left looking
                // applied, which is the one thing this method must not do.
                let under = target
                    .clone()
                    .or_else(|| shadowing_account(state, |tiers| tier_named(tiers, name)));
                document = crate::config::edit::set_tier(
                    &document,
                    under.as_deref(),
                    name,
                    model,
                    pin.as_deref(),
                )?;
            }
            Ok(document)
        })?
    } else {
        false
    };

    if applies_now {
        state.policy.set_tiers(tiers);
    }

    Ok(json!({
        "tiers": tier_map(state),
        // Said out loud, every time. A caller that believed this persisted
        // would find the old mapping back after a restart and have no way to
        // know why.
        "persisted": persisted,
        // Whose mapping this was. Null is the shared table, which is what a
        // caller naming no account changed.
        "account": target,
        // Three answers, not two. A change written for an account that is not
        // serving turns is deliberately not applied here, and calling that "in
        // effect now" tells a front-end a mapping is live for an account this
        // daemon is not making requests as.
        "detail": match (persisted, applies_now) {
            (true, true) => "in effect now, and written to the configuration file",
            (true, false) => {
                "written to the configuration file; that account is not serving turns, \
                 so nothing here changed"
            }
            (false, _) => "in effect until the daemon stops; the configuration file is unchanged",
        },
    }))
}

/// Whose configuration a persisted change belongs in, and whether it takes
/// effect on this daemon now.
///
/// `None` is the shared table, which is what a caller naming no account means
/// and what a single-account operator wants to see edited. A named account that
/// is not the one serving turns changes nothing here — the mapping in force
/// belongs to the account being served — so such a call is only meaningful when
/// it is written.
fn write_target(
    state: &ControlState,
    params: Option<&Value>,
) -> Result<(Option<String>, bool), ProxyError> {
    let Some(named) = params
        .and_then(|params| params.get("account"))
        .and_then(Value::as_str)
    else {
        return Ok((None, true));
    };

    let stored = state.credentials.accounts()?;
    if !stored.iter().any(|account| account.name == named) {
        return Err(ProxyError::invalid_request(format!(
            "no account named `{named}`; stored: {}",
            stored
                .iter()
                .map(|account| account.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let applies_now = serving_name(state).as_deref() == Some(named);
    Ok((Some(named.to_owned()), applies_now))
}

/// The configuration as it is on disk, falling back to what this daemon started
/// from.
///
/// The account tables are the one part this daemon **writes** — `tiers.set` and
/// `effort.set` persist into them and a rename moves them — so resolving them
/// from a startup snapshot means a daemon that cannot see its own writes. The
/// two failures that produced are both silent: a mapping persisted for an
/// account and then ignored when that account is selected, and a later change
/// written to the shared table because the section this daemon just created is
/// not in the snapshot that decides where to write.
///
/// The rest of the configuration is still read once at startup. Nothing else
/// here is written by the daemon, so nothing else can disagree with itself.
///
/// A file that no longer parses keeps the snapshot: the daemon is already
/// running on it, and refusing a switch over a file the operator has half
/// edited would be a worse answer than using what is in force.
fn configuration(state: &ControlState) -> Arc<crate::config::Config> {
    let Some(path) = state.config_path.as_ref() else {
        return Arc::clone(&state.config);
    };
    let Ok(document) = std::fs::read_to_string(path) else {
        return Arc::clone(&state.config);
    };
    match toml::from_str::<crate::config::Config>(&document) {
        Ok(config) => Arc::new(config),
        Err(error) => {
            tracing::warn!(%error, "configuration on disk does not parse; using the one this daemon started with");
            Arc::clone(&state.config)
        }
    }
}

/// The serving account, where its own section already states the thing about to
/// be written — which is what decides where that value is read from.
fn shadowing_account(
    state: &ControlState,
    states_it: impl Fn(&crate::config::Tiers) -> bool,
) -> Option<String> {
    let serving = serving_name(state)?;
    let section = configuration(state).accounts.get(&serving).cloned()?;
    states_it(&section.tiers).then_some(serving)
}

/// The same, for the ceiling.
fn shadowing_effort(state: &ControlState) -> Option<String> {
    let serving = serving_name(state)?;
    let section = configuration(state).accounts.get(&serving).cloned()?;
    section.effort.is_some().then_some(serving)
}

/// Whether a mapping states this tier by name.
fn tier_named(tiers: &crate::config::Tiers, tier: &str) -> bool {
    match tier {
        "opus" => tiers.opus.is_some(),
        "sonnet" => tiers.sonnet.is_some(),
        "haiku" => tiers.haiku.is_some(),
        "fable" => tiers.fable.is_some(),
        _ => false,
    }
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
    let (target, applies_now) = write_target(state, params)?;
    if !applies_now && !persist {
        return Err(ProxyError::invalid_request(
            "that account is not the one serving turns, so this would change nothing \
             unless it is written: pass `persist`, or select the account first",
        ));
    }

    // Written before it is applied, for the same reason `tiers.set` is.
    let persisted = if persist {
        let effort = requested.as_str().map(str::to_owned);
        // Written where the value is read from, exactly as a tier is: an
        // account section stating `effort` replaces the shared line for that
        // account (§4).
        let under = target.clone().or_else(|| shadowing_effort(state));
        write_config(state, |document| {
            crate::config::edit::set_effort(document, under.as_deref(), effort.as_deref())
        })?
    } else {
        false
    };

    // What is in force after this, which is not always what was asked for. A
    // `null` written under an account removes that account's override, and the
    // shared ceiling applies again (§4) — reporting "no ceiling" there would be
    // a figure that lasts until the next start and then quietly comes back.
    let effective = if persisted {
        configuration(state).effort_ceiling_for(serving_name(state).as_deref())?
    } else {
        ceiling
    };

    if applies_now {
        state.policy.set_effort_ceiling(effective);
    }

    Ok(json!({
        "effort": effective
            .and_then(|effort| serde_json::to_value(effort).ok())
            .unwrap_or(Value::Null),
        "persisted": persisted,
        "account": target,
        // Three answers, not two. A change written for an account that is not
        // serving turns is deliberately not applied here, and calling that "in
        // effect now" tells a front-end a mapping is live for an account this
        // daemon is not making requests as.
        "detail": match (persisted, applies_now) {
            (true, true) => "in effect now, and written to the configuration file",
            (true, false) => {
                "written to the configuration file; that account is not serving turns, \
                 so nothing here changed"
            }
            (false, _) => "in effect until the daemon stops; the configuration file is unchanged",
        },
    }))
}

/// `login` — start the authorization flow, or join the one already running.
///
/// Answers with the URL and returns; the flow completes in the background and
/// `status` is what says whether it did. A control call that blocked until the
/// operator finished in a browser would hold the socket for minutes and give a
/// front-end nothing to render in the meantime.
async fn login(state: &ControlState, params: Option<&Value>) -> Result<Value, ProxyError> {
    // What to call the account this authorization produces. Without one it is
    // named by the account id the grant carries.
    let label = params
        .and_then(|params| params.get("label"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let started = state
        .login
        .start(Arc::clone(&state.credentials), label)
        .await?;

    Ok(json!({
        "authorization_url": started.url,
        // Whether this call started the flow or joined one. A caller that
        // cannot tell would report a second operator's login as its own.
        "already_in_flight": started.already_in_flight,
        // The name the account will get. A call that joined a flow already
        // running gets that flow's label, which is not necessarily the one it
        // asked for — and a caller that could not tell would go looking for an
        // account that was never going to exist.
        "label": started.label,
        "detail": "open the URL to authorize; `status` reports when it completed",
    }))
}

/// The account id of the account serving turns, where there is one.
fn serving_account(state: &ControlState) -> Option<String> {
    state
        .credentials
        .load()
        .ok()
        .flatten()
        .and_then(|credentials| credentials.account_id)
}

/// `accounts` — every stored grant, and which one serves turns.
fn accounts(state: &ControlState) -> Result<Value, ProxyError> {
    let accounts = state.credentials.accounts()?;
    Ok(json!({
        "selected": accounts
            .iter()
            .find(|account| account.selected)
            .map(|account| account.name.clone()),
        "accounts": accounts,
    }))
}

/// `accounts.rename` — change what this daemon calls an account.
///
/// The grant is untouched and so is the account id the backend knows it by;
/// what moves is the name `accounts.select` takes. A login carrying no label
/// names the account by that id, which is not something anyone wants to type.
fn rename_account(state: &ControlState, params: Option<&Value>) -> Result<Value, ProxyError> {
    let named = |key: &str| {
        params
            .and_then(|params| params.get(key))
            .and_then(Value::as_str)
    };
    let (Some(from), Some(to)) = (named("account"), named("name")) else {
        return Err(ProxyError::invalid_request(
            "name both halves: {\"account\": \"...\", \"name\": \"...\"}",
        ));
    };

    // The store first, because it is the half that can refuse: an unknown name,
    // or one already taken (`auth/store.rs`). Writing the file first meant a
    // refused rename still moved the section, so the account that already held
    // the new name silently inherited another account's mapping and the other
    // lost its own.
    //
    // The file is what must not be left behind, so a write that fails puts the
    // name back rather than leaving the account and its section apart.
    state.credentials.rename(from, to)?;
    let moved = match move_account_section(state, from, to) {
        Ok(moved) => moved,
        Err(error) => {
            let _ = state.credentials.rename(to, from);
            return Err(error);
        }
    };

    Ok(json!({
        "renamed": from,
        "name": to,
        // Whether anything in the configuration file moved with it. False is
        // the ordinary answer: most accounts have no section at all.
        "moved_configuration": moved,
    }))
}

/// Move an account's own tables in the configuration file, if it has any.
///
/// Nothing is written where there is nothing to move, so an account that never
/// had a section does not cause a file to be created from the shipped example.
/// A daemon with no configuration file to write is only a failure if there was
/// something to write.
fn move_account_section(state: &ControlState, from: &str, to: &str) -> Result<bool, ProxyError> {
    let Some(path) = state.config_path.as_ref() else {
        return Ok(false);
    };
    let document = match std::fs::read_to_string(path) {
        Ok(document) => document,
        // No file is nothing to move. Anything else is a file this could not
        // read, and treating the two alike renames the account and leaves its
        // section under the old name while reporting that nothing moved.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(ProxyError::invalid_request(format!(
                "could not read {}: {error}",
                path.display()
            )));
        }
    };
    let Some(moved) = crate::config::edit::rename_account(&document, from, to)? else {
        return Ok(false);
    };

    std::fs::write(path, moved).map(|()| true).map_err(|error| {
        ProxyError::invalid_request(format!(
            "could not write {}: {error}. The account was not renamed, because its \
                 tier mapping is filed under the old name and would have been left behind.",
            path.display()
        ))
    })
}

/// `accounts.select` — choose the account every following turn is made as.
///
/// The selection is written to the store the ingress authenticates through, so
/// it moves what routes turns rather than only what this socket reports. Two
/// things travel with it: the quota, which belongs to the account that was
/// serving, and a refusal, which belongs to the grant that was being spent.
async fn select_account(state: &ControlState, params: Option<&Value>) -> Result<Value, ProxyError> {
    let name = params
        .and_then(|params| params.get("account"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ProxyError::invalid_request("name the account to select: {\"account\": \"...\"}")
        })?;

    // What to go back to if the account cannot be served. Read before the
    // switch, because after it there is nothing left that remembers.
    let previous = serving_name(state);

    state.credentials.select(name)?;

    // The catalog first, because the mapping is validated against it and it is
    // the new account's menu that decides (§7.0).
    let catalog_refreshed = refresh_catalog(state).await;

    if let Err(refusal) = put_mapping_in_force(state, name) {
        // Back where it was, catalog included. A daemon left serving an account
        // whose every turn is dispatched to a model the backend will not answer
        // for fails one turn later, upstream, saying nothing about tier mapping.
        if let Some(previous) = previous
            && state.credentials.select(&previous).is_ok()
        {
            refresh_catalog(state).await;
            return Err(refusal);
        }
        return Err(ProxyError::invalid_request(format!(
            "{refusal}\n\nThe previous account could not be restored, so this daemon is \
             now serving `{name}` with a mapping it cannot serve. Fix the mapping, or \
             select another account."
        )));
    }

    state.usage.clear();
    // The conversations already bound to the previous account. Each pays a
    // full upload on its next turn, which is what §4.3 resolves every
    // ambiguity toward anyway — and the alternative is a conversation billed
    // to an account the operator has just moved off.
    state.sessions.clear();

    Ok(json!({
        "selected": name,
        // The catalog is one account's menu (`proxy-behavior.md` §7.0), so it
        // is asked for again as the account now serving. Said out loud because
        // a fetch that failed leaves the previous account's list in force, and
        // everything downstream of it — the models offered, the efforts
        // allowed, what `tiers.set` will accept — still describes that account.
        "catalog_refreshed": catalog_refreshed,
        // The mapping this account is served by, which is not necessarily the
        // one that was routing turns a moment ago.
        "tiers": tier_map(state),
    }))
}

/// `cross_account_tiers.set` — grant or revoke consent for pinned tiers.
///
/// **Always persisted.** The other setters persist on request because a
/// front-end trying something is not an operator changing what the daemon is;
/// consent is the operator changing what the daemon is, by definition, and a
/// grant that evaporated at the next restart would leave the file refusing a
/// mapping the operator explicitly permitted.
///
/// Written before it is applied, like every persisted change here: a write
/// that fails leaves the daemon as it was, so the error is the whole story.
fn set_cross_account(state: &ControlState, params: Option<&Value>) -> Result<Value, ProxyError> {
    let enabled = params
        .and_then(|params| params.get("enabled"))
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            ProxyError::invalid_request("`cross_account_tiers.set` needs `enabled`: true or false")
        })?;

    // Revoking while a pin is in force would write a file this daemon will
    // refuse to start from — a refusal the operator only meets at the next
    // restart, with no way back but an edit. Refused now instead, naming what
    // still needs the consent.
    if !enabled {
        let pinned: Vec<&str> = state
            .policy
            .get()
            .tiers()
            .iter()
            .filter(|tier| tier.account.is_some())
            .map(|tier| tier.tier)
            .collect();
        if !pinned.is_empty() {
            return Err(ProxyError::invalid_request(format!(
                "consent cannot be revoked while {} still pin{} another account; point the \
                 tier{} back at a model first",
                pinned.join(", "),
                if pinned.len() == 1 { "s" } else { "" },
                if pinned.len() == 1 { "" } else { "s" },
            )));
        }
    }

    let persisted = write_config(state, |document| {
        crate::config::edit::set_cross_account_tiers(document, enabled)
    })?;

    state.policy.set_cross_account(if enabled {
        crate::config::CrossAccountTiers::Permitted
    } else {
        crate::config::CrossAccountTiers::Refused
    });

    Ok(json!({ "cross_account_tiers": enabled, "persisted": persisted }))
}

/// Resolve one account's mapping, check the catalog can serve it, and put it in
/// force.
///
/// A catalog is one account's menu (§7.0), so the mapping that was routing
/// turns a moment ago describes the account just moved off.
///
/// **Validated only against a catalog that describes this account.** Two things
/// stop it otherwise, and both are §7.1: a fallback list is not the backend's
/// answer, which `validate` skips on its own; and a refetch that failed leaves
/// the previous account's list in force, which would refuse this account's
/// mapping over a menu belonging to somebody else. Where that happens the
/// switch goes ahead and `catalog_stale` says the list is not this account's,
/// which is the honest report and the documented one.
fn put_mapping_in_force(state: &ControlState, account: &str) -> Result<(), ProxyError> {
    let config = configuration(state);
    let tiers = config
        .tiers_for(Some(account))
        .resolve(config.cross_account_policy())?;
    let ceiling = config.effort_ceiling_for(Some(account))?;

    let catalog = state.catalog.current();
    if !catalog.is_stale_for(serving_account(state).as_deref()) {
        // The third door onto the rule the daemon's start and `tiers.set` use,
        // through the same function: this list is the account being switched
        // to, and a pinned or relayed tier names another menu entirely.
        catalog.validate(&crate::upstream::relay::validated_models(
            &state.credentials.accounts().unwrap_or_default(),
            &tiers,
        ))?;
    }

    state.policy.set_tiers(tiers);
    state.policy.set_effort_ceiling(ceiling);
    Ok(())
}

/// The name the store files the account serving turns under.
///
/// The name rather than the account id, because that is what an account section
/// in the configuration is keyed by and what every account verb takes — and a
/// key account has no id at all.
fn serving_name(state: &ControlState) -> Option<String> {
    state
        .credentials
        .accounts()
        .ok()?
        .into_iter()
        .find(|account| account.selected)
        .map(|account| account.name)
}

/// Fetch the catalog again for the account now serving turns.
///
/// Best effort. A failure keeps the list already in force rather than
/// replacing it with the fallback: fetch failure is not evidence that a model
/// went away (§7.1), and withdrawing models the account has would be the worse
/// wrong answer. The caller is told which happened.
async fn refresh_catalog(state: &ControlState) -> bool {
    let Some(authorizer) = authorizer(state) else {
        return false;
    };
    let Ok(authorization) = authorizer.authorize(None).await else {
        return false;
    };
    state.catalog.refresh(&authorization).await
}

/// The credential of the account serving turns, resolved the same way every
/// upstream path resolves it.
///
/// Built here rather than held, because it is two handles and no state: a
/// stored one would be a third place the selection could be read from.
fn authorizer(state: &ControlState) -> Option<crate::auth::authorize::AccountAuthorizer> {
    state.tokens.as_ref().map(|tokens| {
        crate::auth::authorize::AccountAuthorizer::new(
            Arc::clone(&state.credentials),
            Arc::clone(tokens),
        )
    })
}

/// `accounts.forget` — forget one account.
///
/// With nothing named it clears the account serving turns, which is what a
/// caller that knows of only one means by it. The rest stay usable, and the
/// answer says which one went: a front-end that could not tell would have to
/// guess what it just did.
async fn forget_account(state: &ControlState, params: Option<&Value>) -> Result<Value, ProxyError> {
    let named = params
        .and_then(|params| params.get("account"))
        .and_then(Value::as_str);

    let serving = state
        .credentials
        .accounts()?
        .into_iter()
        .find(|account| account.selected)
        .map(|account| account.name);

    let cleared = match named {
        Some(name) => {
            state.credentials.remove(name)?;
            Some(name.to_owned())
        }
        None => {
            // Clearing what is already gone is not an error: forgetting has
            // always been safe to run twice.
            state.credentials.clear()?;
            serving.clone()
        }
    };

    // Only if the grant being spent is the one that went. Forgetting an idle
    // account changes nothing about the account serving turns, and discarding
    // its quota there costs a figure for nothing — while forgetting its
    // refusal would leave `status` reporting a healthy grant while every
    // dispatch failed.
    // Handing over to another account is a switch by another name, so what
    // travels with a switch travels here: the quota that belonged to the grant
    // that went, the conversations bound to it, and the catalog that described
    // its plan.
    let handed_over = cleared.is_some() && cleared == serving;
    if handed_over {
        state.usage.clear();
        state.sessions.clear();
    }

    Ok(json!({
        "forgotten": cleared,
        // Who serves turns now. Forgetting the account that was serving hands
        // over to another, and a caller that has to ask a second question to
        // learn which is a caller that will report the wrong one.
        "serving": state
            .credentials
            .accounts()?
            .into_iter()
            .find(|account| account.selected)
            .map(|account| account.name),
        "catalog_refreshed": handed_over && refresh_catalog(state).await,
    }))
}

/// `usage.refresh` — ask the backend for a quota figure now.
///
/// The free path stays the primary one: the backend volunteers a snapshot at
/// the head of every stream, and `usage` reports that. This is for the case
/// that path cannot cover — a front-end with a figure to show on a daemon that
/// has served no turn yet.
async fn refresh_usage(state: &ControlState) -> Result<Value, ProxyError> {
    let Some(authorizer) = authorizer(state) else {
        return Err(ProxyError::authentication(
            "there are no credentials to ask with; run `login` first",
        ));
    };

    let snapshot = crate::usage::fetch(
        &reqwest::Client::new(),
        &state.usage_endpoint,
        &authorizer.authorize(None).await?,
    )
    .await?;

    // Recorded where the stream path records its own, so everything that reads
    // a quota reads one value. A figure asked for and a figure volunteered are
    // the same figure from the same account.
    state.usage.record(&snapshot);
    Ok(snapshot.to_json())
}
