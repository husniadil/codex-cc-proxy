//! `docs/api.md` §2 — `doctor`.
//!
//! Runs the probe suite and reports a capability matrix. Against the replay
//! corpus it costs nothing and proves the proxy's half; against a live backend
//! it spends quota and proves both. The matrix always states which it was, so a
//! run that contacted nothing cannot be mistaken for one that did.

use crate::error::ProxyError;
use crate::ingress::AppState;
use crate::ingress::ModelMapping;
use crate::ingress::router;
use crate::probe;
use crate::probe::Outcome;
use crate::probe::Status;
use crate::upstream::Transport;
use futures::StreamExt;
use proxenos_core::fixture::Fixture;
use proxenos_core::responses::ResponsesRequest;
use serde_json::Value;
use std::sync::Arc;
use std::sync::Mutex;

/// The corpus compiled into the binary, one entry per fixture.
///
/// `include_str!` rather than a build script, so the compiler tracks the files
/// and a re-recorded fixture cannot leave a stale copy behind. What it does not
/// track is a *new* file; `the_embedded_corpus_holds_every_fixture_on_disk`
/// covers that.
const EMBEDDED: &[(&str, &str)] = &[
    (
        "context-meter",
        include_str!("../../../fixtures/context-meter.json"),
    ),
    (
        "count-tokens",
        include_str!("../../../fixtures/count-tokens.json"),
    ),
    ("relay", include_str!("../../../fixtures/relay.json")),
    (
        "read-document",
        include_str!("../../../fixtures/read-document.json"),
    ),
    (
        "read-image",
        include_str!("../../../fixtures/read-image.json"),
    ),
    (
        "tool-calling",
        include_str!("../../../fixtures/tool-calling.json"),
    ),
    (
        "tool-search",
        include_str!("../../../fixtures/tool-search.json"),
    ),
    (
        "web-fetch",
        include_str!("../../../fixtures/web-fetch.json"),
    ),
    (
        "web-search",
        include_str!("../../../fixtures/web-search.json"),
    ),
];

/// Where a probe's fixture is read from.
///
/// An installed binary has no checkout to read `fixtures/` out of, and a first
/// `doctor` that skipped every probe would establish nothing at the one moment
/// it is most likely to be run. So the corpus travels with the binary — but a
/// directory the operator named is never substituted for it, because a
/// recording just captured by `record` must be what answers.
pub enum Corpus {
    /// A directory: `--fixtures`, or the checkout's own.
    Dir(std::path::PathBuf),
    /// The copy compiled in. Reads nothing.
    Embedded,
}

impl Corpus {
    /// `--fixtures` when the operator gave one; otherwise the checkout's
    /// directory when it is there, and the embedded copy when it is not.
    pub fn resolve(explicit: Option<std::path::PathBuf>) -> Self {
        match explicit {
            Some(path) => Self::Dir(path),
            None => {
                let default = std::path::PathBuf::from("fixtures");
                if default.is_dir() {
                    Self::Dir(default)
                } else {
                    Self::Embedded
                }
            }
        }
    }

    /// The fixtures compiled in, by name. For the drift check.
    pub fn embedded_names() -> Vec<&'static str> {
        EMBEDDED.iter().map(|(name, _)| *name).collect()
    }

    /// The corpus in one phrase, for the line under the matrix.
    pub fn describe(&self) -> String {
        match self {
            Self::Dir(path) => path.display().to_string(),
            Self::Embedded => "the corpus compiled into this binary".to_owned(),
        }
    }

    /// A fixture's text, or the reason the probe has to be skipped.
    fn read(&self, name: &str) -> Result<String, String> {
        match self {
            Self::Dir(dir) => {
                let path = dir.join(format!("{name}.json"));
                std::fs::read_to_string(&path)
                    .map_err(|_| format!("no fixture at {}", path.display()))
            }
            Self::Embedded => EMBEDDED
                .iter()
                .find(|(fixture, _)| *fixture == name)
                .map(|(_, body)| (*body).to_owned())
                .ok_or_else(|| format!("no fixture named {name} is compiled in")),
        }
    }
}

/// A transport that forwards to the real one and remembers the request.
///
/// The probes assert on what the backend was sent as well as on what came
/// back, and there is no other place to observe it: by the time the response
/// exists the request is gone.
struct WatchingTransport {
    inner: Arc<dyn Transport>,
    seen: Mutex<Option<ResponsesRequest>>,
}

#[async_trait::async_trait]
impl Transport for WatchingTransport {
    async fn stream(
        &self,
        request: &ResponsesRequest,
        session_id: Option<&str>,
        account: Option<&str>,
    ) -> Result<crate::upstream::EventStream, ProxyError> {
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(request.clone());
        }
        self.inner.stream(request, session_id, account).await
    }
}

/// A transport that answers from a recorded stream and records what it was
/// asked.
struct ReplayTransport {
    events: Vec<Value>,
}

#[async_trait::async_trait]
impl Transport for ReplayTransport {
    async fn stream(
        &self,
        _request: &ResponsesRequest,
        _session_id: Option<&str>,
        _account: Option<&str>,
    ) -> Result<crate::upstream::EventStream, ProxyError> {
        let payloads: Vec<Result<String, ProxyError>> = self
            .events
            .iter()
            .map(|event| Ok(event.to_string()))
            .collect();
        Ok(futures::stream::iter(payloads).boxed())
    }
}

/// What the probes are answered by.
enum Backend {
    /// The fixture's own recorded stream. Contacts nothing, costs nothing.
    Replay,
    /// The real transport, the real tier mapping, and the operator's own
    /// effort ceiling. Spends quota.
    Live {
        transport: Arc<dyn Transport>,
        models: Arc<Vec<ModelMapping>>,
        effort_ceiling: Option<proxenos_core::responses::Effort>,
    },
}

/// Run the probe suite against the fixture corpus.
pub async fn run(fixtures: &Corpus, only: Option<&str>) -> Result<Vec<Outcome>, ProxyError> {
    run_against(fixtures, only, Backend::Replay).await
}

/// Run the probe suite against a real backend.
///
/// Each probe's request is the corpus's — the same unguessable markers — but it
/// is answered by the backend rather than by a recording, so what passes here
/// is the backend doing its half as well as the proxy doing its own. It spends
/// inference quota, one turn per probe.
///
/// The effort ceiling is the operator's, and is applied here for the same
/// reason it is applied to a turn: this is the one command that bills by
/// design, so it is the last place a configured cap should be quietly ignored.
pub async fn run_live(
    fixtures: &Corpus,
    only: Option<&str>,
    transport: Arc<dyn Transport>,
    models: Arc<Vec<ModelMapping>>,
    effort_ceiling: Option<proxenos_core::responses::Effort>,
) -> Result<Vec<Outcome>, ProxyError> {
    run_against(
        fixtures,
        only,
        Backend::Live {
            transport,
            models,
            effort_ceiling,
        },
    )
    .await
}

async fn run_against(
    fixtures: &Corpus,
    only: Option<&str>,
    backend: Backend,
) -> Result<Vec<Outcome>, ProxyError> {
    let mut outcomes = Vec::new();

    for probe in probe::all() {
        if let Some(only) = only
            && probe.name != only
        {
            continue;
        }

        // Nothing to replay: the launch surface is rendered rather than
        // recorded, so this probe is answered before a corpus is opened. It
        // costs the same on both modes, which is why it is not skipped live.
        if probe.surface == crate::probe::Surface::Environment {
            outcomes.push(run_environment(&probe));
            continue;
        }

        let raw = match fixtures.read(probe.fixture) {
            Ok(raw) => raw,
            Err(reason) => {
                outcomes.push(Outcome {
                    name: probe.name.to_owned(),
                    capability: probe.capability,
                    surface: probe.surface,
                    rationale: probe.rationale,
                    status: Status::Skipped(reason),
                });
                continue;
            }
        };

        let fixture: Fixture = match serde_json::from_str(&raw) {
            Ok(fixture) => fixture,
            Err(error) => {
                outcomes.push(Outcome {
                    name: probe.name.to_owned(),
                    capability: probe.capability,
                    surface: probe.surface,
                    rationale: probe.rationale,
                    status: Status::Skipped(format!("the fixture will not parse: {error}")),
                });
                continue;
            }
        };

        // §9 live is not wired. Driving the relay against the real backend
        // needs the account serving turns switched to the second provider for
        // the length of the run — a change to what the daemon is serving while
        // it is serving it. Skipped rather than run against the translating
        // path, which would report the wrong path's health under this name.
        if matches!(backend, Backend::Live { .. }) && probe.surface == crate::probe::Surface::Relay
        {
            outcomes.push(Outcome {
                name: probe.name.to_owned(),
                capability: probe.capability,
                surface: probe.surface,
                rationale: probe.rationale,
                status: Status::Skipped(
                    "a live relay run needs the serving account switched to the second \
                     provider, which is not wired"
                        .to_owned(),
                ),
            });
            continue;
        }

        // Only a replayed run needs the recording. Live, the backend supplies
        // the stream and the fixture is only there for its request.
        if matches!(backend, Backend::Replay)
            && probe.surface == crate::probe::Surface::Messages
            && fixture.upstream.is_empty()
        {
            outcomes.push(Outcome {
                name: probe.name.to_owned(),
                capability: probe.capability,
                surface: probe.surface,
                rationale: probe.rationale,
                status: Status::Skipped(
                    "the fixture carries no upstream stream; this path never reaches the backend"
                        .to_owned(),
                ),
            });
            continue;
        }

        outcomes.push(run_one(&probe, &fixture, &backend).await);
    }

    if outcomes.is_empty() {
        return Err(ProxyError::not_found(format!(
            "no probe named `{}`. Known probes: {}",
            only.unwrap_or(""),
            probe::all()
                .iter()
                .map(|probe| probe.name)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    Ok(outcomes)
}

async fn run_one(probe: &probe::Probe, fixture: &Fixture, backend: &Backend) -> Outcome {
    if probe.surface == crate::probe::Surface::Relay {
        return run_relay(probe, fixture).await;
    }

    // Both arms watch the request, because half the checks are about what the
    // backend was sent rather than about what it said.
    let (transport, models, effort_ceiling) = match backend {
        Backend::Replay => (
            Arc::new(WatchingTransport {
                inner: Arc::new(ReplayTransport {
                    events: fixture.upstream.clone(),
                }),
                seen: Mutex::new(None),
            }),
            Arc::new(vec![ModelMapping {
                requested: "claude-sonnet-5".to_owned(),
                upstream: "gpt-5.6-terra".to_owned(),
                account: None,
            }]),
            None,
        ),
        Backend::Live {
            transport,
            models,
            effort_ceiling,
        } => (
            Arc::new(WatchingTransport {
                inner: Arc::clone(transport),
                seen: Mutex::new(None),
            }),
            Arc::clone(models),
            *effort_ceiling,
        ),
    };

    let state = AppState {
        // Routing only: a probe has no operator tier mapping behind it, and
        // saying so by name is what keeps it from looking like one that lost
        // its tiers.
        policy: Arc::new(crate::policy::Policy::new(
            crate::policy::Snapshot::routing_only(models.as_ref().clone(), effort_ceiling),
        )),
        catalog: Arc::new(crate::catalog::CatalogSource::fixed(
            crate::catalog::Catalog::fallback(),
        )),
        transport: Arc::clone(&transport) as Arc<dyn Transport>,
        conduits: None,
        recorder: None,
        capture: Arc::new(crate::recorder::Switches::default()),
        // A probe measures the translation, not the operator's prompt policy.
        // Injecting here would make a probe's request differ from the fixture
        // it is derived from, for no gain in what the probe establishes.
        usage: Arc::new(crate::usage::UsageStore::default()),
        instructions: Arc::new(crate::config::InstructionsConfig {
            identity: false,
            append: None,
            // Off here for the same reason as the identity line: a probe's
            // request must match the fixture it is derived from.
            working_budget: false,
        }),
        sessions: Arc::new(crate::session::SessionStore::new()),
        relay: None,
    };

    let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
        return Outcome {
            name: probe.name.to_owned(),
            capability: probe.capability,
            surface: probe.surface,
            rationale: probe.rationale,
            status: Status::Skipped("could not bind a loopback port".to_owned()),
        };
    };
    let Ok(addr) = listener.local_addr() else {
        return Outcome {
            name: probe.name.to_owned(),
            capability: probe.capability,
            surface: probe.surface,
            rationale: probe.rationale,
            status: Status::Skipped("could not read the bound port".to_owned()),
        };
    };
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let path = match probe.surface {
        crate::probe::Surface::Messages => "/v1/messages",
        crate::probe::Surface::CountTokens => "/v1/messages/count_tokens",
        // Both answered above, before any of this was built.
        crate::probe::Surface::Relay | crate::probe::Surface::Environment => "/v1/messages",
    };

    // The probe reads frames, so it asks for them. A fixture's recorded request
    // is the client's, and the client always streams; the field is set here
    // rather than edited into the recordings, which are evidence of what was
    // sent and not a place to add what was not.
    let mut request = fixture.request.clone();
    if matches!(probe.surface, crate::probe::Surface::Messages)
        && let Some(object) = request.as_object_mut()
    {
        object.insert("stream".to_owned(), Value::Bool(true));
    }

    let response = reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .json(&request)
        .send()
        .await;

    let body = match response {
        Ok(response) => response.text().await.unwrap_or_default(),
        Err(error) => {
            return Outcome {
                name: probe.name.to_owned(),
                capability: probe.capability,
                surface: probe.surface,
                rationale: probe.rationale,
                status: Status::Failed(format!("the proxy did not answer: {error}")),
            };
        }
    };

    let frames = match probe.surface {
        crate::probe::Surface::Messages => frames_of(&body),
        // One object, presented as a single frame so the same checks apply.
        crate::probe::Surface::CountTokens => serde_json::from_str::<Value>(&body)
            .map(|mut value| {
                if let Some(object) = value.as_object_mut() {
                    object.insert("type".to_owned(), Value::from("count_tokens"));
                }
                vec![value]
            })
            .unwrap_or_default(),
        crate::probe::Surface::Relay | crate::probe::Surface::Environment => Vec::new(),
    };
    let sent = transport
        .seen
        .lock()
        .ok()
        .and_then(|seen| seen.clone())
        .and_then(|request| serde_json::to_value(request).ok())
        .unwrap_or(Value::Null);

    let status = match backend {
        Backend::Replay => probe::evaluate(probe, &sent, &frames),
        Backend::Live { .. } => probe::evaluate_live(probe, &sent, &frames),
    };

    Outcome {
        name: probe.name.to_owned(),
        capability: probe.capability,
        surface: probe.surface,
        rationale: probe.rationale,
        status,
    }
}

fn frames_of(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|block| {
            block
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .and_then(|data| serde_json::from_str(data).ok())
        })
        .collect()
}

/// One account, holding a key for the second provider and selected.
///
/// The relay reads the store per request to decide whether a turn belongs on
/// its path, so a probe of that path needs a store — but not a real one, and
/// not a file: the value below is a placeholder that never leaves loopback,
/// and everything a login would write is refused rather than written.
struct ProbeStore;

const PROBE_ACCOUNT: &str = "the relay probe";

fn probe_only(verb: &str) -> ProxyError {
    ProxyError::invalid_request(format!("the relay probe's store cannot {verb}"))
}

impl crate::auth::store::CredentialStore for ProbeStore {
    fn load(&self) -> Result<Option<crate::auth::store::Credentials>, ProxyError> {
        Ok(None)
    }
    fn save(&self, _credentials: &crate::auth::store::Credentials) -> Result<(), ProxyError> {
        Err(probe_only("save a grant"))
    }
    fn clear(&self) -> Result<(), ProxyError> {
        Err(probe_only("clear"))
    }
}

impl crate::auth::store::AccountStore for ProbeStore {
    fn accounts(&self) -> Result<Vec<crate::auth::store::Account>, ProxyError> {
        Ok(vec![crate::auth::store::Account {
            name: PROBE_ACCOUNT.to_owned(),
            kind: "key",
            provider: crate::auth::store::Provider::Anthropic.as_str(),
            account_id: None,
            email: None,
            plan: None,
            expires_at: None,
            selected: true,
        }])
    }
    fn add(
        &self,
        _credentials: &crate::auth::store::Credentials,
        _label: Option<&str>,
    ) -> Result<String, ProxyError> {
        Err(probe_only("add an account"))
    }
    fn select(&self, _name: &str) -> Result<(), ProxyError> {
        Err(probe_only("select an account"))
    }
    fn remove(&self, _name: &str) -> Result<(), ProxyError> {
        Err(probe_only("remove an account"))
    }
    fn credential(&self) -> Result<Option<crate::auth::store::Credential>, ProxyError> {
        Ok(None)
    }
    fn credential_for(&self, name: &str) -> Result<crate::auth::store::Credential, ProxyError> {
        Err(probe_only(&format!("resolve the credential of `{name}`")))
    }
    fn add_key(
        &self,
        _name: &str,
        _key: &str,
        _provider: crate::auth::store::Provider,
    ) -> Result<(), ProxyError> {
        Err(probe_only("add a key"))
    }
    fn save_for(
        &self,
        _name: &str,
        _credentials: &crate::auth::store::Credentials,
    ) -> Result<(), ProxyError> {
        Err(probe_only("save a grant"))
    }
    fn rename(&self, _from: &str, _to: &str) -> Result<(), ProxyError> {
        Err(probe_only("rename an account"))
    }
}

/// The header a key would put on the relayed request.
///
/// A placeholder value, because the stand-in backend below authenticates
/// nothing. What the probe is measuring is the body, and the body is the one
/// thing this must not touch.
struct ProbeAuthorizer;

#[async_trait::async_trait]
impl crate::auth::authorize::Authorizer for ProbeAuthorizer {
    async fn authorize(
        &self,
        _account: Option<&str>,
    ) -> Result<crate::auth::authorize::Authorization, ProxyError> {
        Ok(crate::auth::authorize::Authorization {
            kind: crate::auth::authorize::Kind::Key,
            account: Some(PROBE_ACCOUNT.to_owned()),
            headers: vec![("x-api-key".to_owned(), "probe-placeholder".to_owned())],
        })
    }
}

/// Render the launch environment twice and hold it to `docs/api.md` §2.2.
///
/// A representative mapping either way: four tiers on this proxy's own defaults
/// with a translating account serving them, and the same four served by an
/// account on the second provider, where every turn is relayed. Half the
/// contract is an absence, and an absence can only be shown against a mapping
/// where the variable would otherwise be there.
///
/// Nothing is contacted. This asserts what a launch hands the client, which is
/// the thing that breaks — a probe of the configuration behind it would stay
/// green over a launch that rendered nothing.
fn run_environment(probe: &probe::Probe) -> Outcome {
    let tiers: Vec<crate::config::ResolvedTier> = ["opus", "sonnet", "haiku", "fable"]
        .into_iter()
        .zip([
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.6-luna",
            "gpt-5.6-terra",
        ])
        .map(|(tier, model)| crate::config::ResolvedTier {
            tier,
            model: model.to_owned(),
            account: None,
            defaulted: true,
        })
        .collect();

    let catalog = crate::catalog::Catalog::fallback();
    let account = |provider: crate::auth::store::Provider| {
        vec![crate::auth::store::Account {
            name: "the launch probe".to_owned(),
            kind: "key",
            provider: provider.as_str(),
            account_id: None,
            email: None,
            plan: None,
            expires_at: None,
            selected: true,
        }]
    };

    let render = |accounts: &[crate::auth::store::Account]| {
        crate::control::handler::environment_for(0, false, &tiers, &catalog, accounts)
    };

    let translating = render(&account(crate::auth::store::Provider::Codex));
    let all_relay = render(&account(crate::auth::store::Provider::Anthropic));

    Outcome {
        name: probe.name.to_owned(),
        capability: probe.capability,
        surface: probe.surface,
        rationale: probe.rationale,
        status: probe::check_environment(&translating, &all_relay),
    }
}

/// Drive the §9 relay branch against a recording.
///
/// Everything else here goes through `AppState` with `relay: None`, so the
/// branch that forwards a turn instead of translating it was reached by
/// nothing in the suite. The stand-in backend records the bytes it was sent
/// and answers the fixture's own stream, which is what lets the same checks
/// ask both halves of the question the relay's claim rests on: that what left
/// the client arrived unaltered, and that what the backend said came back the
/// same way.
async fn run_relay(probe: &probe::Probe, fixture: &Fixture) -> Outcome {
    let skipped = |reason: &str| Outcome {
        name: probe.name.to_owned(),
        capability: probe.capability,
        surface: probe.surface,
        rationale: probe.rationale,
        status: Status::Skipped(reason.to_owned()),
    };

    // The id the client sends is the id the backend sees: this path never
    // rewrites the model (§9), so the mapping names it on both sides.
    let Some(model) = fixture
        .request
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return skipped("the fixture's request carries no model to route on");
    };

    // The stream as the second provider would write it. Rebuilt from the
    // fixture's events rather than stored as text, so one corpus format
    // answers both paths.
    let stream = fixture
        .upstream
        .iter()
        .map(|event| {
            let name = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message");
            format!("event: {name}\ndata: {event}\n\n")
        })
        .collect::<String>();

    let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&seen);
    let backend = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: String| {
            if let Ok(mut sink) = sink.lock() {
                *sink = Some(body);
            }
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    stream,
                )
            }
        }),
    );

    let Ok(backend_listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
        return skipped("could not bind a loopback port for the stand-in backend");
    };
    let Ok(backend_addr) = backend_listener.local_addr() else {
        return skipped("could not read the stand-in backend's port");
    };
    tokio::spawn(async move {
        let _ = axum::serve(backend_listener, backend).await;
    });

    let state = AppState {
        policy: Arc::new(crate::policy::Policy::new(
            crate::policy::Snapshot::routing_only(
                vec![ModelMapping {
                    requested: model.clone(),
                    upstream: model,
                    account: Some(PROBE_ACCOUNT.to_owned()),
                }],
                None,
            ),
        )),
        catalog: Arc::new(crate::catalog::CatalogSource::fixed(
            crate::catalog::Catalog::fallback(),
        )),
        // Deliberately unreachable. A turn that took the translating path
        // would fail to connect rather than quietly answer, so a routing
        // mistake cannot pass as a relayed success.
        transport: Arc::new(crate::upstream::http::HttpTransport::new(
            "http://127.0.0.1:1/unused",
        )),
        conduits: None,
        recorder: None,
        capture: Arc::new(crate::recorder::Switches::default()),
        usage: Arc::new(crate::usage::UsageStore::default()),
        instructions: Arc::new(crate::config::InstructionsConfig {
            identity: false,
            append: None,
            working_budget: false,
        }),
        sessions: Arc::new(crate::session::SessionStore::new()),
        relay: Some(Arc::new(crate::upstream::relay::Relay::new(
            format!("http://{backend_addr}/v1/messages"),
            Arc::new(ProbeStore) as Arc<dyn crate::auth::store::AccountStore>,
            Arc::new(ProbeAuthorizer) as Arc<dyn crate::auth::authorize::Authorizer>,
        ))),
    };

    let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
        return skipped("could not bind a loopback port");
    };
    let Ok(addr) = listener.local_addr() else {
        return skipped("could not read the bound port");
    };
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(fixture.request.to_string())
        .send()
        .await;

    let body = match response {
        Ok(response) => response.text().await.unwrap_or_default(),
        Err(error) => {
            return Outcome {
                name: probe.name.to_owned(),
                capability: probe.capability,
                surface: probe.surface,
                rationale: probe.rationale,
                status: Status::Failed(format!("the proxy did not answer: {error}")),
            };
        }
    };

    // What the backend was sent, as it was sent. Parsed only to apply the
    // checks: a body this probe re-encoded before checking it would be exactly
    // the mistake the probe exists to catch.
    let sent = seen
        .lock()
        .ok()
        .and_then(|seen| seen.clone())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);

    Outcome {
        name: probe.name.to_owned(),
        capability: probe.capability,
        surface: probe.surface,
        rationale: probe.rationale,
        status: probe::evaluate(probe, &sent, &frames_of(&body)),
    }
}
