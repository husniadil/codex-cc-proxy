//! `docs/proxy-behavior.md` §9 — the Messages relay.
//!
//! A turn whose model id belongs to an account on the second provider is
//! forwarded rather than translated. The whole claim of that path is that
//! nothing in the middle touches the payload, and a payload that arrives
//! intact by accident is indistinguishable from one the proxy rebuilt
//! correctly — so these assert on the *bytes*, in both directions, against a
//! body no round trip through this proxy's own types would reproduce: keys out
//! of order, a field it does not model, and whitespace of its own.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use axum::http::HeaderMap;
use proxenos::auth::store::AccountStore;
use proxenos::auth::store::CredentialStore;
use proxenos::auth::store::Credentials;
use proxenos::auth::store::FileStore;
use proxenos::auth::store::Provider;
use proxenos::config::ResolvedTier;
use proxenos::ingress::AppState;
use proxenos::ingress::router;
use proxenos::upstream::http::HttpTransport;
use proxenos::upstream::relay::Relay;
use serde_json::Value;
use std::sync::Arc;
use std::sync::Mutex;

/// What the stub upstream was sent, one entry per request.
#[derive(Default)]
struct Seen {
    bodies: Vec<String>,
    headers: Vec<HeaderMap>,
}

type Recorded = Arc<Mutex<Seen>>;

/// The exact stream the stub answers with.
///
/// Not a well-formed conversation — deliberately. Anything the proxy might be
/// tempted to re-encode would normalise this, and the point is that it does
/// not: the blank lines, the two-space indent, and the trailing comment all
/// have to survive.
const UPSTREAM_STREAM: &str = "event: message_start\ndata: {\"type\":\"message_start\",  \"marker\":\"7QF3\"}\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\n";

/// A body no round trip through this proxy's request types would reproduce.
const CLIENT_BODY: &str = "{\"max_tokens\":64,\"model\":\"claude-sonnet-5\",\"messages\":[{\"role\":\"user\",\"content\":\"the marker is 4KD9\"}],\"stream\":true,\"an_unmodelled_field\":{\"z\":1,\"a\":2}}";

fn grant(access: &str, refresh: &str, account_id: &str) -> Credentials {
    Credentials {
        access_token: access.to_owned(),
        refresh_token: refresh.to_owned(),
        id_token: None,
        account_id: Some(account_id.to_owned()),
        expires_at: Some(u64::MAX / 2),
    }
}

fn tier(name: &'static str, model: &str, account: Option<&str>) -> ResolvedTier {
    ResolvedTier {
        defaulted: false,
        account: account.map(str::to_owned),
        tier: name,
        model: model.to_owned(),
    }
}

/// A Messages endpoint that records what it was sent and answers a fixed
/// stream. `refuse` makes it answer the way the real one refuses.
async fn upstream(refuse: bool) -> (String, Recorded) {
    let seen: Recorded = Arc::new(Mutex::new(Seen::default()));
    let sink = Arc::clone(&seen);

    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |headers: HeaderMap, body: String| {
            if let Ok(mut seen) = sink.lock() {
                seen.bodies.push(body);
                seen.headers.push(headers);
            }
            async move {
                if refuse {
                    return (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        "{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}"
                            .to_owned(),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    UPSTREAM_STREAM.to_owned(),
                )
            }
        }),
    );
    // The stub imposes no limit of its own, so a body-size assertion measures
    // the ingress under test rather than this stand-in for the backend.
    let app = app.layer(axum::extract::DefaultBodyLimit::disable());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/v1/messages"), seen)
}

/// The state a daemon shares with the rest of the process rather than keeps to
/// itself: the live policy, where captures land and whether they are on, and
/// the store a status line is answered from.
///
/// Held by the test rather than built inside the daemon, because every
/// observability assertion below is about what a turn left behind here.
struct Observed {
    policy: Arc<proxenos::policy::Policy>,
    recorder: Option<proxenos::recorder::Recorder>,
    capture: Arc<proxenos::recorder::Switches>,
    usage: Arc<proxenos::usage::UsageStore>,
}

impl Observed {
    fn new(tiers: Vec<ResolvedTier>) -> Self {
        Self {
            policy: Arc::new(proxenos::policy::Policy::new(
                proxenos::policy::Snapshot::new(
                    tiers,
                    None,
                    proxenos::config::CrossAccountTiers::Permitted,
                ),
            )),
            recorder: None,
            capture: Arc::new(proxenos::recorder::Switches::new(None)),
            usage: Arc::new(proxenos::usage::UsageStore::default()),
        }
    }

    /// Capturing what the client sends, into this directory.
    fn capturing_ingress(mut self, directory: &std::path::Path) -> Self {
        self.recorder = Some(proxenos::recorder::Recorder::new(directory));
        self.capture = Arc::new(proxenos::recorder::Switches::new(Some(
            proxenos::recorder::Mode::Ingress,
        )));
        self
    }
}

/// The daemon's wiring for a relay turn. No conduit: this path never reaches
/// one, and giving it one would let a routing mistake pass as a success.
async fn daemon(
    store: Arc<FileStore>,
    tiers: Vec<ResolvedTier>,
    refuse: bool,
) -> (String, Recorded) {
    daemon_with(store, &Observed::new(tiers), refuse).await
}

async fn daemon_with(
    store: Arc<FileStore>,
    observed: &Observed,
    refuse: bool,
) -> (String, Recorded) {
    let (endpoint, seen) = upstream(refuse).await;

    let authorizer: Arc<dyn proxenos::auth::authorize::Authorizer> =
        Arc::new(proxenos::auth::authorize::AccountAuthorizer::new(
            Arc::clone(&store) as Arc<dyn AccountStore>,
            Arc::new(proxenos::auth::tokens::TokenSource::new(
                Arc::clone(&store) as Arc<dyn CredentialStore>,
                String::new(),
                "client-abc",
                Arc::new(proxenos::auth::tokens::SystemClock),
            )),
        ));

    let state = AppState {
        policy: Arc::clone(&observed.policy),
        catalog: Arc::new(proxenos::catalog::CatalogSource::fixed(
            proxenos::catalog::Catalog::fallback(),
        )),
        // Deliberately unreachable. A turn that took the translating path
        // would fail to connect rather than quietly answer.
        transport: Arc::new(HttpTransport::new("http://127.0.0.1:1/unused")),
        conduits: None,
        recorder: observed.recorder.clone(),
        capture: Arc::clone(&observed.capture),
        usage: Arc::clone(&observed.usage),
        instructions: Arc::new(proxenos::config::InstructionsConfig {
            identity: true,
            append: None,
            working_budget: true,
        }),
        sessions: Arc::new(proxenos::session::SessionStore::new()),
        relay: Some(Arc::new(Relay::new(
            endpoint,
            Arc::clone(&store) as Arc<dyn AccountStore>,
            authorizer,
        ))),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    (format!("http://{addr}"), seen)
}

/// One codex grant serving turns, one key for the second provider.
fn store_with_a_relay_account(dir: &tempfile::TempDir) -> Arc<FileStore> {
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
    store
        .add(
            &grant("serving-token", "serving-refresh", "acct_serving"),
            None,
        )
        .unwrap();
    store
        .add_key("relay", "relay-key-value", Provider::Anthropic)
        .unwrap();
    store.select("acct_serving").unwrap();
    store
}

/// Post a raw body, exactly as written.
async fn turn(base: &str, body: &'static str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer the-client-placeholder")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "claude-code-20250219,effort-2025-11-24")
        .header("x-app", "cli")
        // Never sent by the recorded client, and never forwarded: a turn
        // authenticated as whatever the caller happened to hold is a turn this
        // proxy did not route.
        .header("x-api-key", "a-key-the-caller-brought")
        .body(body)
        .send()
        .await
        .expect("the ingress should answer")
}

/// **Build 3.** The body the client sent is the body the upstream receives,
/// byte for byte.
#[tokio::test]
async fn the_relay_forwards_the_ingress_body_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);

    let (base, seen) = daemon(
        store,
        vec![tier("sonnet", "claude-sonnet-5", Some("relay"))],
        false,
    )
    .await;

    assert_eq!(turn(&base, CLIENT_BODY).await.status(), 200);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.bodies.len(), 1);
    assert_eq!(seen.bodies[0], CLIENT_BODY);
}

/// A turn larger than the body extractor's 2 MB default reaches the backend
/// rather than being refused at the door.
///
/// A real client's turn — a full system prompt and a large tool set — runs well
/// past 2 MB, and the 413 the ingress returned was not even an Anthropic error
/// shape: the client read it as retryable and looped forever, the turn never
/// reaching the backend. A small body could never catch this, which is why this
/// one is deliberately over the limit.
#[tokio::test]
async fn a_turn_over_the_default_body_limit_is_relayed() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);

    let (base, seen) = daemon(
        store,
        vec![tier("sonnet", "claude-sonnet-5", Some("relay"))],
        false,
    )
    .await;

    // Comfortably past axum's 2 MB default for the body extractor.
    let filler = "x".repeat(3 * 1024 * 1024);
    let body = format!(
        r#"{{"model":"claude-sonnet-5","messages":[{{"role":"user","content":"{filler}"}}]}}"#
    );

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer placeholder")
        .header("anthropic-version", "2023-06-01")
        .body(body.clone())
        .send()
        .await
        .expect("the ingress should answer");

    assert_eq!(
        response.status(),
        200,
        "a body over 2 MB must reach the backend, not be refused at the door"
    );
    assert_eq!(
        seen.lock().unwrap().bodies[0].len(),
        body.len(),
        "the whole body was relayed, byte for byte"
    );
}

/// **Build 3, the other direction.** What upstream streamed is what the client
/// reads, byte for byte.
#[tokio::test]
async fn the_relay_streams_the_response_back_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);

    let (base, _seen) = daemon(
        store,
        vec![tier("sonnet", "claude-sonnet-5", Some("relay"))],
        false,
    )
    .await;

    let response = turn(&base, CLIENT_BODY).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(response.text().await.unwrap(), UPSTREAM_STREAM);
}

/// **Build 4.** The bearer is the account's, and the client's own
/// `anthropic-*` headers arrive as it sent them.
#[tokio::test]
async fn the_relay_replaces_the_bearer_and_passes_the_client_headers_through() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);

    let (base, seen) = daemon(
        store,
        vec![tier("sonnet", "claude-sonnet-5", Some("relay"))],
        false,
    )
    .await;

    assert_eq!(turn(&base, CLIENT_BODY).await.status(), 200);

    let seen = seen.lock().unwrap();
    let headers = &seen.headers[0];
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("<absent>")
            .to_owned()
    };

    assert_eq!(header("authorization"), "Bearer relay-key-value");
    assert_eq!(header("anthropic-version"), "2023-06-01");
    assert_eq!(
        header("anthropic-beta"),
        "claude-code-20250219,effort-2025-11-24"
    );
    assert_eq!(header("x-app"), "cli");
    // The client's placeholder never reaches the backend, and neither does a
    // key it brought of its own.
    assert!(!header("authorization").contains("placeholder"));
    assert_eq!(header("x-api-key"), "<absent>");
}

/// **Build 2.** A model id two accounts claim is refused, naming both.
///
/// Never a pick: the body carries an id rather than a tier name, so there is
/// nothing left to tell the two apart, and choosing one would spend an
/// account nobody pointed at that turn.
#[tokio::test]
async fn a_model_id_two_accounts_claim_refuses_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);
    store
        .add_key("spare", "spare-key-value", Provider::Anthropic)
        .unwrap();
    store.select("acct_serving").unwrap();

    let (base, seen) = daemon(
        store,
        vec![
            tier("sonnet", "claude-sonnet-5", Some("relay")),
            tier("opus", "claude-sonnet-5", Some("spare")),
        ],
        false,
    )
    .await;

    let response = turn(&base, CLIENT_BODY).await;
    assert_eq!(response.status(), 400);

    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("claude-sonnet-5"), "{message}");
    assert!(message.contains("relay"), "{message}");
    assert!(message.contains("spare"), "{message}");

    assert!(
        seen.lock().unwrap().bodies.is_empty(),
        "nothing reached the backend as either of them"
    );
}

/// **Build 5.** An upstream refusal is already in the client's error shape, so
/// it passes through as it is — status and body.
///
/// Rewrapping it would restate a message the backend wrote, and a rewrap that
/// loses the type takes the client's own retry logic with it.
#[tokio::test]
async fn an_upstream_refusal_reaches_the_client_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);

    let (base, _seen) = daemon(
        store,
        vec![tier("sonnet", "claude-sonnet-5", Some("relay"))],
        true,
    )
    .await;

    let response = turn(&base, CLIENT_BODY).await;
    assert_eq!(response.status(), 429);
    assert_eq!(
        response.text().await.unwrap(),
        "{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}"
    );
}

/// A model id no relay account claims is untouched: it takes the translating
/// path, exactly as it did before this path existed.
#[tokio::test]
async fn an_unclaimed_model_id_still_takes_the_translating_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);

    let (base, seen) = daemon(store, vec![tier("sonnet", "gpt-5.6-terra", None)], false).await;

    // The translating path's transport points nowhere, so this fails — and
    // failing there is the assertion: it never reached the relay.
    let response = turn(&base, CLIENT_BODY).await;
    assert_ne!(response.status(), 200);
    assert!(seen.lock().unwrap().bodies.is_empty());
}

/// A tier that pins nobody belongs to the account serving turns, and if that
/// account is on the second provider its turns are relayed too.
///
/// Otherwise an operator who stored a key for this provider and selected it
/// would have every turn sent to the other provider's endpoint, where it is
/// refused as a credential of the wrong kind — a message about the credential,
/// which is not the half that is wrong.
#[tokio::test]
async fn an_unpinned_tier_is_relayed_when_the_serving_account_is_the_relay_account() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);
    store.select("relay").unwrap();

    let (base, seen) = daemon(
        Arc::clone(&store),
        vec![tier("sonnet", "claude-sonnet-5", None)],
        false,
    )
    .await;

    assert_eq!(turn(&base, CLIENT_BODY).await.status(), 200);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.bodies[0], CLIENT_BODY);
    assert_eq!(
        seen.headers[0]
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer relay-key-value")
    );
}

/// Selecting an account on the first provider puts the same unpinned tier back
/// on the translating path. The selection is what moved, and nothing else.
#[tokio::test]
async fn an_unpinned_tier_follows_the_selection_back_to_the_translating_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);
    store.select("acct_serving").unwrap();

    let (base, seen) = daemon(
        Arc::clone(&store),
        vec![tier("sonnet", "claude-sonnet-5", None)],
        false,
    )
    .await;

    // The translating path's transport points nowhere, so this fails — and
    // failing there is the assertion.
    assert_ne!(turn(&base, CLIENT_BODY).await.status(), 200);
    assert!(seen.lock().unwrap().bodies.is_empty());
}

/// §9.1 — a catalog is one provider's menu, so a relay-bound tier is not
/// measured against it.
///
/// The ids on this path belong to the second provider and are absent from the
/// first provider's list by construction. Validating them there would refuse a
/// mapping that is correct, at startup and at `tiers.set` alike, and the
/// refusal would name a menu the id was never on.
#[test]
fn relay_bound_tiers_are_left_out_of_the_catalog_validation() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);
    let accounts = store.accounts().unwrap();

    // Pinned to the relay account: its model belongs to that account's menu.
    assert_eq!(
        proxenos::upstream::relay::validated_models(
            &accounts,
            &[
                tier("opus", "gpt-5.6-terra", None),
                tier("sonnet", "claude-sonnet-5", Some("relay")),
            ],
        ),
        vec!["gpt-5.6-terra".to_owned()]
    );

    // Unpinned, with the relay account serving: every tier is on that path, so
    // there is nothing left for the first provider's catalog to speak for.
    store.select("relay").unwrap();
    assert!(
        proxenos::upstream::relay::validated_models(
            &store.accounts().unwrap(),
            &[tier("sonnet", "claude-sonnet-5", None)],
        )
        .is_empty()
    );

    // A pin to an account of the first provider is left out too, for the
    // neighbouring reason (§7.1): a pin names another account's menu, and this
    // list is the serving account's.
    assert!(
        proxenos::upstream::relay::validated_models(
            &accounts,
            &[tier("sonnet", "gpt-5.4-mini", Some("acct_serving"))],
        )
        .is_empty()
    );
}

/// A capture file, read back with the request kept as the bytes on disk.
///
/// `Value` would answer this question wrong: it reorders keys and normalises
/// whitespace, so a capture that had been re-encoded would still compare equal
/// to one that had not.
#[derive(serde::Deserialize)]
struct Captured {
    request: Box<serde_json::value::RawValue>,
    headers: Vec<(String, String)>,
}

/// The one capture in a directory, or a failure naming what was there instead.
fn one_capture(directory: &std::path::Path) -> Captured {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(directory)
        .expect("the capture directory should exist")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 1, "exactly one capture, found {files:?}");
    let body = std::fs::read_to_string(&files[0]).expect("the capture should be readable");
    serde_json::from_str(&body).expect("the capture should be in the corpus format")
}

/// **Build 1.** A relayed turn is captured by `record ingress`, and what it
/// captures is the bytes that were relayed.
///
/// A capture is the only record of what a client actually sent, and a path
/// that leaves none is invisible to it. Asserted against a body no round trip
/// through this proxy's own types would reproduce — a field it does not model,
/// and keys in an order it would not write — so a capture rebuilt from parsed
/// types fails here rather than passing by resemblance.
#[tokio::test]
async fn a_relayed_turn_is_captured_by_ingress_capture_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);
    let captures = dir.path().join("captures");
    let observed = Observed::new(vec![tier("sonnet", "claude-sonnet-5", Some("relay"))])
        .capturing_ingress(&captures);

    let (base, seen) = daemon_with(Arc::clone(&store), &observed, false).await;
    assert_eq!(turn(&base, CLIENT_BODY).await.status(), 200);
    assert_eq!(seen.lock().unwrap().bodies[0], CLIENT_BODY);

    let capture = one_capture(&captures);
    assert_eq!(
        capture.request.get(),
        CLIENT_BODY,
        "the capture holds the bytes that were relayed, not a re-encoding of them"
    );

    let header = |name: &str| {
        capture
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or("<absent>")
            .to_owned()
    };
    // The names survive, the values do not: that the client sent one is the
    // datum, and a capture is a file that is not the credential store.
    assert_eq!(header("authorization"), "(redacted)");
    assert_eq!(header("x-api-key"), "(redacted)");
    assert_eq!(header("anthropic-version"), "2023-06-01");
    assert_eq!(header("x-app"), "cli");
}

/// **Build 2.** A relayed turn's model id joins the served list a status line
/// answers "is this session mine?" from, read back over the control socket.
///
/// The mapping alone cannot answer it. A client is handed final ids at launch
/// and sends them for the session's life, so an operator who remaps the tier
/// mid-run leaves the mapping naming an id no running session sends — and the
/// running session's status line stops recognizing its own quota. What a turn
/// was actually made against is the only durable record of it, and the
/// translating path has always kept one.
#[tokio::test]
async fn a_relayed_turn_joins_the_served_models_a_status_line_reads() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_a_relay_account(&dir);
    let observed = Observed::new(vec![tier("sonnet", "claude-sonnet-5", Some("relay"))]);

    let (base, seen) = daemon_with(Arc::clone(&store), &observed, false).await;
    assert_eq!(turn(&base, CLIENT_BODY).await.status(), 200);
    assert_eq!(seen.lock().unwrap().bodies.len(), 1);

    // The operator remaps the tier while the session runs. The mapping now
    // names an id nobody sends; the session still sends the one it was
    // launched with.
    observed
        .policy
        .set_tiers(vec![tier("sonnet", "claude-sonnet-6", Some("relay"))]);

    let socket = dir.path().join("control.sock");
    let state = proxenos::control::handler::ControlState {
        port: 8787,
        policy: Arc::clone(&observed.policy),
        catalog: Arc::new(proxenos::catalog::CatalogSource::fixed(
            proxenos::catalog::Catalog::fallback(),
        )),
        credentials: Arc::clone(&store) as Arc<dyn AccountStore>,
        capture: Arc::clone(&observed.capture),
        usage: Arc::clone(&observed.usage),
        login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
        config: Arc::new(proxenos::config::Config::default()),
        shutdown: Arc::new(proxenos::daemon::Shutdown::default()),
        tokens: None,
        usage_endpoint: String::new(),
        sessions: Arc::new(proxenos::session::SessionStore::new()),
        config_path: None,
    };
    let serving = socket.clone();
    tokio::spawn(async move {
        let _ = proxenos::control::serve(&serving, state).await;
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let answer = proxenos::control::call(&socket, "usage", None)
        .await
        .expect("the control socket should answer");
    let models = answer["models"]
        .as_array()
        .expect("usage states which models this quota belongs to")
        .iter()
        .filter_map(|model| model.as_str())
        .collect::<Vec<_>>();
    assert!(
        models.contains(&"claude-sonnet-5"),
        "the relayed turn's own id is missing from {models:?}"
    );
}
