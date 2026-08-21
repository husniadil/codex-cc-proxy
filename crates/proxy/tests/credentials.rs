//! `docs/proxy-behavior.md` §8 — which credential reaches which endpoint.
//!
//! The whole point of a second credential kind is that the two are not
//! interchangeable, so these assert on what arrived at the other end rather
//! than on what the code meant to send. The replay server is real HTTP over
//! loopback; nothing here reaches the network.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

mod replay;

use futures::StreamExt;
use pretty_assertions::assert_eq;
use proxenos::auth::authorize::AccountAuthorizer;
use proxenos::auth::authorize::Authorizer;
use proxenos::auth::authorize::Kind;
use proxenos::auth::store::AccountStore;
use proxenos::auth::store::CredentialStore;
use proxenos::auth::store::FileStore;
use proxenos::auth::store::Provider;
use proxenos::auth::tokens::SystemClock;
use proxenos::auth::tokens::TokenSource;
use proxenos::upstream::Transport;
use proxenos::upstream::http::HttpTransport;
use std::sync::Arc;

/// A key nothing in this repository could produce from context: the assertion
/// turns on this string arriving verbatim, so a request that carried no
/// authorization at all cannot pass by looking plausible (non-negotiable #4).
const KEY: &str = "sk-probe-7f3a91c4e88b40d2-do-not-guess";

/// What the stub answers with, likewise unguessable.
const MARKER: &str = "resp_4d81f0ac-key-path";

fn request() -> proxenos_core::responses::ResponsesRequest {
    proxenos_core::responses::ResponsesRequest {
        model: "gpt-5.6-terra".to_owned(),
        ..proxenos_core::responses::ResponsesRequest::default()
    }
}

fn store_with_key(dir: &tempfile::TempDir) -> Arc<FileStore> {
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
    store.add_key("billing", KEY, Provider::Codex).unwrap();
    store
}

fn authorizer(store: &Arc<FileStore>) -> Arc<dyn Authorizer> {
    Arc::new(AccountAuthorizer::new(
        Arc::clone(store) as Arc<dyn AccountStore>,
        Arc::new(TokenSource::new(
            Arc::clone(store) as Arc<dyn CredentialStore>,
            String::new(),
            "client-abc",
            Arc::new(SystemClock),
        )),
    ))
}

/// A key-authenticated turn completes end to end, and arrives carrying its key
/// and nothing a subscription endpoint would have wanted.
#[tokio::test]
async fn a_key_authenticated_turn_completes_and_carries_only_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_key(&dir);

    let server = replay::ReplayServer::start(replay::Behavior::Events(vec![serde_json::json!({
        "type": "response.completed",
        "response": { "id": MARKER },
    })]))
    .await;

    let transport = HttpTransport::new(server.url.clone())
        .for_endpoint(Kind::Key)
        .with_credentials(authorizer(&store));

    let events: Vec<String> = transport
        .stream(&request(), None, None)
        .await
        .expect("the turn should open")
        .filter_map(|event| async move { event.ok() })
        .collect()
        .await;

    assert!(
        events.iter().any(|event| event.contains(MARKER)),
        "the turn did not complete: {events:?}"
    );

    let headers = server.headers();
    let sent = headers.first().expect("one request should have arrived");
    assert_eq!(
        sent.get("authorization").map(String::as_str),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(
        sent.get("originator"),
        None,
        "the originator identifies a subscription client: {sent:?}"
    );
    assert_eq!(
        sent.get("chatgpt-account-id"),
        None,
        "a key has no account to name: {sent:?}"
    );
}

/// A credential cannot be spent against the endpoint the other kind expects,
/// and the refusal names both halves.
///
/// It is a refusal rather than a fallback: sending a key to a subscription
/// endpoint is answered upstream with a message about an invalid token, which
/// sends whoever reads it looking for the wrong problem.
#[tokio::test]
async fn a_credential_is_refused_against_the_other_kinds_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_key(&dir);
    let server = replay::ReplayServer::start(replay::Behavior::Events(vec![])).await;

    // A key, against the endpoint a subscription grant belongs to.
    let transport = HttpTransport::new(server.url.clone()).with_credentials(authorizer(&store));

    let Err(error) = transport.stream(&request(), None, None).await else {
        panic!("a key against a subscription endpoint should be refused");
    };
    let message = error.to_string();
    assert!(message.contains("key"), "{message}");
    assert!(message.contains("subscription"), "{message}");

    assert!(
        server.headers().is_empty(),
        "the refusal must happen before anything is sent"
    );
}

/// A key endpoint is sent an uncompressed body.
///
/// zstd on the request body is measured against the subscription backend
/// (§4.4) and nothing else. Sent to an endpoint that does not decompress it,
/// the bytes are parsed as JSON and rejected — observed live as
/// `400 invalid_json`, "encountered a unicode decode error when parsing this
/// JSON value", which names neither compression nor the endpoint.
#[tokio::test]
async fn a_key_endpoint_is_never_sent_a_compressed_body() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_key(&dir);
    let server = replay::ReplayServer::start(replay::Behavior::Events(vec![serde_json::json!({
        "type": "response.completed",
        "response": { "id": MARKER },
    })]))
    .await;

    // Well past the size that would be compressed on the subscription path.
    let bulky = proxenos_core::responses::ResponsesRequest {
        model: "gpt-5.6-terra".to_owned(),
        instructions: Some("x".repeat(8_000)),
        ..proxenos_core::responses::ResponsesRequest::default()
    };

    let transport = HttpTransport::new(server.url.clone())
        .for_endpoint(Kind::Key)
        .with_credentials(authorizer(&store))
        .with_compression(true);

    let events: Vec<String> = transport
        .stream(&bulky, None, None)
        .await
        .expect("the turn should open")
        .filter_map(|event| async move { event.ok() })
        .collect()
        .await;
    assert!(events.iter().any(|event| event.contains(MARKER)));

    let sent = server.headers();
    let sent = sent.first().expect("one request should have arrived");
    assert_eq!(
        sent.get("content-encoding"),
        None,
        "a key endpoint was sent a compressed body: {sent:?}"
    );
    // And the body arrived as the JSON it is.
    assert!(
        server.requests()[0]["instructions"]
            .as_str()
            .is_some_and(|instructions| instructions.len() == 8_000),
        "the body did not arrive intact"
    );
}
