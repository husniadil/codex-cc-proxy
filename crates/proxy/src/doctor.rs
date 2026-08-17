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
use codex_cc_proxy_core::fixture::Fixture;
use codex_cc_proxy_core::responses::ResponsesRequest;
use futures::StreamExt;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

/// What the probes ran against, in words the matrix prints verbatim.
pub const AGAINST_REPLAY: &str = "replayed fixtures (the backend was not contacted)";

/// The live counterpart. It spends quota, and the matrix says so.
pub const AGAINST_LIVE: &str = "a live backend (the backend answered, and was billed)";

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
    ) -> Result<crate::upstream::EventStream, ProxyError> {
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(request.clone());
        }
        self.inner.stream(request).await
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
        effort_ceiling: Option<codex_cc_proxy_core::responses::Effort>,
    },
}

/// Run the probe suite against the fixture corpus.
pub async fn run(fixtures: &Path, only: Option<&str>) -> Result<Vec<Outcome>, ProxyError> {
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
    fixtures: &Path,
    only: Option<&str>,
    transport: Arc<dyn Transport>,
    models: Arc<Vec<ModelMapping>>,
    effort_ceiling: Option<codex_cc_proxy_core::responses::Effort>,
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
    fixtures: &Path,
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

        let path = fixtures.join(format!("{}.json", probe.fixture));
        let Ok(raw) = std::fs::read_to_string(&path) else {
            outcomes.push(Outcome {
                name: probe.name.to_owned(),
                capability: probe.capability,
                status: Status::Skipped(format!("no fixture at {}", path.display())),
            });
            continue;
        };

        let fixture: Fixture = match serde_json::from_str(&raw) {
            Ok(fixture) => fixture,
            Err(error) => {
                outcomes.push(Outcome {
                    name: probe.name.to_owned(),
                    capability: probe.capability,
                    status: Status::Skipped(format!("the fixture will not parse: {error}")),
                });
                continue;
            }
        };

        // Only a replayed run needs the recording. Live, the backend supplies
        // the stream and the fixture is only there for its request.
        if matches!(backend, Backend::Replay)
            && probe.surface == crate::probe::Surface::Messages
            && fixture.upstream.is_empty()
        {
            outcomes.push(Outcome {
                name: probe.name.to_owned(),
                capability: probe.capability,
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
        effort_ceiling,
        catalog: Arc::new(crate::catalog::Catalog::fallback()),
        transport: Arc::clone(&transport) as Arc<dyn Transport>,
        conduits: None,
        models,
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
    };

    let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
        return Outcome {
            name: probe.name.to_owned(),
            capability: probe.capability,
            status: Status::Skipped("could not bind a loopback port".to_owned()),
        };
    };
    let Ok(addr) = listener.local_addr() else {
        return Outcome {
            name: probe.name.to_owned(),
            capability: probe.capability,
            status: Status::Skipped("could not read the bound port".to_owned()),
        };
    };
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });

    let path = match probe.surface {
        crate::probe::Surface::Messages => "/v1/messages",
        crate::probe::Surface::CountTokens => "/v1/messages/count_tokens",
    };

    let response = reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .json(&fixture.request)
        .send()
        .await;

    let body = match response {
        Ok(response) => response.text().await.unwrap_or_default(),
        Err(error) => {
            return Outcome {
                name: probe.name.to_owned(),
                capability: probe.capability,
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
