//! `docs/proxy-behavior.md` §8 — credentials.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy::auth::pkce::Pkce;
use codex_cc_proxy::auth::store::CredentialStore;
use codex_cc_proxy::auth::store::Credentials;
use codex_cc_proxy::auth::store::FileStore;
use pretty_assertions::assert_eq;
use serde_json::Value;

fn sample() -> Credentials {
    Credentials {
        access_token: "access-secret".to_owned(),
        refresh_token: "refresh-secret".to_owned(),
        id_token: Some("id-secret".to_owned()),
        account_id: Some("acct_123".to_owned()),
        expires_at: Some(1_800_000_000),
    }
}

/// PKCE is verified against a known vector rather than by recomputing the
/// derivation the way the code does. A test that recomputes it passes by
/// construction and can never disagree with a mistake.
#[test]
fn the_challenge_matches_the_specification_example() {
    // RFC 7636 appendix B.
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");

    assert_eq!(
        pkce.challenge(),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    assert_eq!(pkce.method(), "S256");
}

/// A generated verifier is long enough to be worth generating, and no two are
/// alike.
#[test]
fn generated_verifiers_are_unique_and_long_enough() {
    let first = Pkce::generate();
    let second = Pkce::generate();

    assert!(
        first.verifier().len() >= 43,
        "shorter than the specification allows"
    );
    assert!(
        first.verifier().len() <= 128,
        "longer than the specification allows"
    );
    assert_ne!(first.verifier(), second.verifier());
}

/// Neither the verifier nor the tokens may reach a log. `Debug` is the easiest
/// way for them to get there, so it is implemented by hand and tested.
#[test]
fn debug_output_carries_no_secrets() {
    let rendered = format!("{:?}", sample());

    for secret in ["access-secret", "refresh-secret", "id-secret"] {
        assert!(
            !rendered.contains(secret),
            "Debug leaked {secret}: {rendered}"
        );
    }
    // What is safe to show still shows, or the output is useless.
    assert!(rendered.contains("acct_123"));

    let pkce = Pkce::from_verifier("verifier-secret");
    assert!(!format!("{pkce:?}").contains("verifier-secret"));
}

#[test]
fn credentials_round_trip_through_the_file_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    assert!(store.load().unwrap().is_none(), "nothing stored yet");

    store.save(&sample()).unwrap();
    let loaded = store.load().unwrap().expect("credentials should load");

    assert_eq!(loaded.access_token, "access-secret");
    assert_eq!(loaded.account_id.as_deref(), Some("acct_123"));
}

/// Created `0600` from the outset. Writing first and tightening afterwards
/// leaves a window in which the file is world-readable, and that window is
/// enough.
#[cfg(unix)]
#[test]
fn the_credential_file_is_private_the_moment_it_exists() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("nested").join("credentials.json"));
    store.save(&sample()).unwrap();

    let mode = std::fs::metadata(store.path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "credentials must not be readable by others"
    );
}

#[test]
fn clearing_removes_the_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    store.save(&sample()).unwrap();
    store.clear().unwrap();

    assert!(store.load().unwrap().is_none());
    // Clearing what is already gone is not an error: `disconnect` must be safe
    // to run twice.
    store.clear().unwrap();
}

/// Refresh begins ahead of expiry. A token that expires mid-request fails the
/// request, and the margin is what keeps that from being routine.
#[test]
fn refresh_is_due_before_the_token_actually_expires() {
    let credentials = Credentials {
        expires_at: Some(1_000),
        ..sample()
    };

    assert!(!credentials.needs_refresh(800, 60), "not due yet");
    assert!(credentials.needs_refresh(950, 60), "inside the margin");
    assert!(credentials.needs_refresh(1_200, 60), "already expired");
}

/// An unknown expiry counts as expired. Refreshing needlessly costs one
/// request; using a dead token fails the turn.
#[test]
fn an_unknown_expiry_is_treated_as_expired() {
    let credentials = Credentials {
        expires_at: None,
        ..sample()
    };

    assert!(credentials.needs_refresh(0, 60));
}

// ---------------------------------------------------------------------------
// Refresh, against a mock authorization server on loopback.
// ---------------------------------------------------------------------------

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use codex_cc_proxy::auth::tokens::Clock;
use codex_cc_proxy::auth::tokens::TokenSource;
use std::sync::Arc;
use std::sync::Mutex;

struct FixedClock(u64);

impl Clock for FixedClock {
    fn now_unix(&self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
struct AuthServerState {
    /// Every form body the server received, so a test can assert what was sent
    /// — and what was not.
    bodies: Arc<Mutex<Vec<String>>>,
    response: Arc<(u16, String)>,
    /// Delays the reply, so concurrent callers genuinely overlap.
    delay_ms: u64,
}

struct AuthServer {
    url: String,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl AuthServer {
    async fn start(status: u16, body: &str, delay_ms: u64) -> Self {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let state = AuthServerState {
            bodies: Arc::clone(&bodies),
            response: Arc::new((status, body.to_owned())),
            delay_ms,
        };

        async fn token(
            State(state): State<AuthServerState>,
            body: String,
        ) -> axum::response::Response {
            if let Ok(mut bodies) = state.bodies.lock() {
                bodies.push(body);
            }
            if state.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(state.delay_ms)).await;
            }
            let (status, payload) = state.response.as_ref();
            let status = axum::http::StatusCode::from_u16(*status).unwrap();
            (status, payload.clone()).into_response()
        }

        let app = Router::new().route("/token", post(token)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            url: format!("http://{addr}/token"),
            bodies,
        }
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies
            .lock()
            .map(|got| got.clone())
            .unwrap_or_default()
    }
}

fn expired_store(dir: &tempfile::TempDir) -> Arc<FileStore> {
    let store = FileStore::new(dir.path().join("credentials.json"));
    store
        .save(&Credentials {
            expires_at: Some(1_000),
            ..sample()
        })
        .unwrap();
    Arc::new(store)
}

/// A refresh reply. The access token is a JWT because the claim inside it is
/// the expiry that decides; the body's own `expires_in` is only a fallback.
fn fresh_token_response(exp: u64) -> String {
    serde_json::json!({
        "access_token": token_with(serde_json::json!({ "exp": exp })),
        "refresh_token": "new-refresh",
    })
    .to_string()
}

/// §8 — a refresh sends `grant_type`, `refresh_token`, and `client_id`, and
/// **never `scope`**. Including it causes the authorization server to re-scope
/// the grant and invalidate sibling refresh-token families, which surfaces as
/// another tool being logged out for no visible reason.
#[tokio::test]
async fn a_refresh_never_sends_scope() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(200, &fresh_token_response(9_000), 0).await;
    let source = TokenSource::new(
        expired_store(&dir),
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );

    source.access_token().await.unwrap();

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1);

    // JSON, not form encoding. The authorization code exchange is
    // form-encoded and this is not; sending the wrong one is rejected.
    let body: Value = serde_json::from_str(&bodies[0]).expect("the refresh body should be JSON");

    assert_eq!(body["grant_type"], serde_json::json!("refresh_token"));
    assert_eq!(body["refresh_token"], serde_json::json!("refresh-secret"));
    assert_eq!(body["client_id"], serde_json::json!("client-abc"));
    assert!(
        body.get("scope").is_none(),
        "scope must never be sent on a refresh: {body}"
    );
}

/// A rotated refresh token replaces the stored one.
///
/// Not because the old one stops working — a superseded token was measured
/// still redeeming successfully — but because the new one carries the current
/// lifetime, and a family left to age out eventually cannot be renewed.
#[tokio::test]
async fn a_rotated_refresh_token_is_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(200, &fresh_token_response(5_600), 0).await;
    let store = expired_store(&dir);
    let source = TokenSource::new(
        Arc::clone(&store) as Arc<dyn CredentialStore>,
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );

    source.access_token().await.unwrap();

    let stored = store.load().unwrap().unwrap();
    assert_eq!(stored.refresh_token, "new-refresh");
    // Expiry is taken from the new access token's own claim, not from any
    // field of the response.
    assert_eq!(stored.expires_at, Some(5_600));
}

/// §8 — when the access token carries no readable claim, the response's own
/// `expires_in` is used rather than nothing.
///
/// An absent expiry is treated as expired, so falling through to `None` makes
/// every single request refresh: a rotation per turn against an authorization
/// server that rate-limits them. The field is measured, not assumed — a live
/// refresh returns `expires_in`, and it agrees with the token's `exp`.
#[tokio::test]
async fn an_opaque_access_token_takes_its_expiry_from_the_response() {
    let dir = tempfile::tempdir().unwrap();
    let body = serde_json::json!({
        "access_token": "opaque-not-a-jwt",
        "refresh_token": "new-refresh",
        "expires_in": 864_000,
    })
    .to_string();
    let server = AuthServer::start(200, &body, 0).await;
    let store = expired_store(&dir);
    let source = TokenSource::new(
        Arc::clone(&store) as Arc<dyn CredentialStore>,
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );

    source.access_token().await.unwrap();

    let stored = store.load().unwrap().unwrap();
    assert_eq!(stored.expires_at, Some(866_000));
}

/// A claim inside the token wins over the response field when both are present.
///
/// They agreed to within a second when measured, but the token is what the
/// backend validates, so it is the one that decides.
#[tokio::test]
async fn the_token_claim_outranks_the_response_field() {
    let dir = tempfile::tempdir().unwrap();
    let body = serde_json::json!({
        "access_token": token_with(serde_json::json!({ "exp": 5_600 })),
        "refresh_token": "new-refresh",
        "expires_in": 99_999,
    })
    .to_string();
    let server = AuthServer::start(200, &body, 0).await;
    let store = expired_store(&dir);
    let source = TokenSource::new(
        Arc::clone(&store) as Arc<dyn CredentialStore>,
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );

    source.access_token().await.unwrap();

    assert_eq!(store.load().unwrap().unwrap().expires_at, Some(5_600));
}

/// Concurrent callers collapse to one upstream refresh. Ten simultaneous turns
/// must not produce ten refresh requests: each rotates the family and each is
/// a request against an authorization server that rate-limits them, so nine of
/// them are pure cost. The last writer also wins the store, so the other nine
/// results are discarded even when every one of them succeeded.
#[tokio::test]
async fn concurrent_refreshes_collapse_to_one_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(200, &fresh_token_response(9_000), 120).await;
    let source = Arc::new(TokenSource::new(
        expired_store(&dir),
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    ));

    let mut handles = Vec::new();
    for _ in 0..10 {
        let source = Arc::clone(&source);
        handles.push(tokio::spawn(async move { source.access_token().await }));
    }

    for handle in handles {
        handle
            .await
            .unwrap()
            .expect("every caller should get a token");
    }

    assert_eq!(
        server.bodies().len(),
        1,
        "the server saw more than one refresh"
    );
    assert_eq!(source.refresh_count(), 1);
}

/// §8 — a refused grant marks the connection dead and is never retried. A
/// retry loop against an authorization server is how an account ends up rate
/// limited for nothing.
#[tokio::test]
async fn an_invalid_grant_is_marked_dead_and_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(400, r#"{"error":"refresh_token_expired"}"#, 0).await;
    let source = TokenSource::new(
        expired_store(&dir),
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );

    let first = source.access_token().await.expect_err("should fail");
    assert_eq!(
        first.kind,
        codex_cc_proxy_core::anthropic::ErrorKind::AuthenticationError
    );
    assert!(source.is_dead());

    // Every later attempt fails without touching the network.
    for _ in 0..5 {
        let error = source.access_token().await.expect_err("should stay failed");
        assert!(error.message.contains("login"));
    }
    assert_eq!(server.bodies().len(), 1, "a dead grant was retried");
}

/// A transient failure does not kill the grant. Marking it dead on a 503 would
/// force a re-login for what a retry would have fixed.
#[tokio::test]
async fn a_transient_failure_leaves_the_grant_alive() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(503, r#"{"error":"temporarily_unavailable"}"#, 0).await;
    let source = TokenSource::new(
        expired_store(&dir),
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );

    let error = source.access_token().await.expect_err("should fail");
    assert_eq!(
        error.kind,
        codex_cc_proxy_core::anthropic::ErrorKind::OverloadedError,
        "a transient failure should surface as retryable"
    );
    assert!(!source.is_dead());
}

/// A token still well inside its lifetime is used as-is.
#[tokio::test]
async fn a_valid_token_is_not_refreshed() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(200, &fresh_token_response(9_000), 0).await;
    let store = FileStore::new(dir.path().join("credentials.json"));
    store
        .save(&Credentials {
            expires_at: Some(10_000),
            ..sample()
        })
        .unwrap();

    let source = TokenSource::new(
        Arc::new(store),
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );

    assert_eq!(source.access_token().await.unwrap(), "access-secret");
    assert!(
        server.bodies().is_empty(),
        "no refresh should have happened"
    );
}

/// With nothing stored, the answer is to log in — not an opaque failure.
#[tokio::test]
async fn an_empty_store_asks_for_login() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(200, &fresh_token_response(9_000), 0).await;
    let source = TokenSource::new(
        Arc::new(FileStore::new(dir.path().join("none.json"))),
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );

    let error = source.access_token().await.expect_err("should fail");
    assert!(error.message.contains("login"), "{}", error.message);
}

// ---------------------------------------------------------------------------
// The authorization request, and the claims read back from the tokens.
// ---------------------------------------------------------------------------

use base64::Engine;
use codex_cc_proxy::auth::flow;
use codex_cc_proxy::auth::jwt;

/// Build an unsigned JWT with the given payload. Signature verification is not
/// performed and is not wanted — see the note on the jwt module.
fn token_with(payload: serde_json::Value) -> String {
    let encode = |value: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
    format!(
        "{}.{}.{}",
        encode(br#"{"alg":"none"}"#),
        encode(payload.to_string().as_bytes()),
        encode(b"signature")
    )
}

#[test]
fn the_authorization_url_carries_every_required_parameter() {
    let authorization = flow::begin(1455);

    assert!(
        authorization
            .url
            .starts_with("https://auth.openai.com/oauth/authorize?"),
        "{}",
        authorization.url
    );

    for expected in [
        "response_type=code",
        "code_challenge_method=S256",
        "id_token_add_organizations=true",
        "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
    ] {
        assert!(
            authorization.url.contains(expected),
            "missing {expected} in {}",
            authorization.url
        );
    }

    assert!(
        authorization
            .url
            .contains(&format!("state={}", authorization.state))
    );
    assert!(authorization.url.contains(&format!(
        "code_challenge={}",
        authorization.pkce.challenge()
    )));
    // The verifier stays local. Sending it here would defeat PKCE entirely.
    assert!(!authorization.url.contains(authorization.pkce.verifier()));
}

/// Spaces in the scope are percent-encoded, not turned into `+`. A `+` in a
/// query value is not universally read as a space.
#[test]
fn the_scope_is_percent_encoded() {
    let authorization = flow::begin(1455);
    assert!(
        authorization
            .url
            .contains("openid%20profile%20email%20offline_access")
    );
    assert!(!authorization.url.contains('+'));
}

/// Two logins never share a state or a verifier.
#[test]
fn each_authorization_is_fresh() {
    let first = flow::begin(1455);
    let second = flow::begin(1455);

    assert_ne!(first.state, second.state);
    assert_ne!(first.pkce.verifier(), second.pkce.verifier());
}

/// The token response carries no expiry field, so it is read from the access
/// token's own claim. Without this every request looks due for refresh.
#[test]
fn expiry_comes_from_the_access_token_claim() {
    let token = token_with(serde_json::json!({ "exp": 1_800_000_000u64 }));
    assert_eq!(jwt::expiry(&token), Some(1_800_000_000));
}

/// A token with no `exp`, or one that is not a JWT at all, yields nothing —
/// which `needs_refresh` treats as expired. Refreshing needlessly costs one
/// request; assuming a token is live when it is not fails the turn.
#[test]
fn an_unreadable_token_yields_no_expiry() {
    assert_eq!(jwt::expiry("not-a-jwt"), None);
    assert_eq!(jwt::expiry(&token_with(serde_json::json!({}))), None);
    assert_eq!(jwt::expiry(""), None);
}

/// The account id is a claim nested under the auth namespace of the id token.
#[test]
fn the_account_id_is_read_from_the_id_token() {
    let token = token_with(serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct_from_claim",
            "chatgpt_plan_type": "pro",
        },
    }));

    assert_eq!(
        jwt::account_id(Some(&token)).as_deref(),
        Some("acct_from_claim")
    );
}

/// The plan sits beside the account id, under the same namespace.
///
/// It is worth reading because it is the only local explanation for a whole
/// class of refusal: efforts and models are gated on the subscription, and a
/// free account asking for one gets an error that names the value, never the
/// plan. Reporting it turns "the backend said no" into a checkable fact.
#[test]
fn the_plan_is_read_from_the_id_token() {
    let token = token_with(serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct_from_claim",
            "chatgpt_plan_type": "plus",
        },
    }));

    assert_eq!(jwt::plan(Some(&token)).as_deref(), Some("plus"));
}

/// A missing plan is absent, not guessed at. Defaulting to "free" would be a
/// fabricated figure, and defaulting to "plus" would explain away a refusal
/// that deserves explaining.
#[test]
fn an_id_token_without_a_plan_claims_nothing() {
    assert_eq!(jwt::plan(None), None);
    assert_eq!(jwt::plan(Some("not-a-jwt")), None);
    assert_eq!(
        jwt::plan(Some(&token_with(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "a" },
        })))),
        None
    );
}

#[test]
fn an_id_token_without_the_claim_yields_no_account() {
    assert_eq!(jwt::account_id(None), None);
    assert_eq!(jwt::account_id(Some("not-a-jwt")), None);
    assert_eq!(
        jwt::account_id(Some(&token_with(serde_json::json!({ "sub": "u" })))),
        None
    );
}

/// The codes this authorization server uses to say a grant is gone. Each is
/// terminal, and each must be, or the proxy retries something that cannot
/// succeed.
#[rstest::rstest]
#[case(400, r#"{"error":"refresh_token_expired"}"#, true)]
#[case(400, r#"{"error":"refresh_token_reused"}"#, true)]
#[case(400, r#"{"error":"refresh_token_invalidated"}"#, true)]
#[case(400, r#"{"error":"invalid_grant"}"#, true)]
#[case(401, r#"{}"#, true)]
// A plain 400 with no code is NOT terminal. Marking a grant dead on a
// recoverable failure forces a re-login that a retry would have avoided.
#[case(400, r#"{"error":"server_error"}"#, false)]
#[case(400, r#"{}"#, false)]
#[case(503, r#"{}"#, false)]
#[tokio::test]
async fn refusals_are_classified_by_whether_the_grant_survives(
    #[case] status: u16,
    #[case] body: &str,
    #[case] expected_dead: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(status, body, 0).await;
    let source = TokenSource::new(
        expired_store(&dir),
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );

    let _ = source
        .access_token()
        .await
        .expect_err("the refresh should fail");

    assert_eq!(
        source.is_dead(),
        expected_dead,
        "status {status} with body {body} was classified wrongly"
    );
}

// ---------------------------------------------------------------------------
// Completing a login.
// ---------------------------------------------------------------------------

use codex_cc_proxy::auth::login;

/// The state is checked before the code is spent. A response carrying the wrong
/// state is not this flow's response, and exchanging its code would attach
/// somebody else's authorization to this proxy.
#[tokio::test]
async fn a_mismatched_state_is_refused_before_the_code_is_spent() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(200, "{}", 0).await;
    let store: Arc<dyn AccountStore> =
        Arc::new(FileStore::new(dir.path().join("credentials.json")));
    let authorization = flow::begin(1455);

    let error = login::complete(
        &reqwest::Client::new(),
        &server.url,
        "client-abc",
        &authorization,
        "a-different-state",
        "the-code",
        &store,
        None,
    )
    .await
    .expect_err("a mismatched state should be refused");

    assert!(error.message.contains("did not match"), "{}", error.message);
    assert!(
        server.bodies().is_empty(),
        "the code must not be sent when the state does not match"
    );
}

/// The exchange is form-encoded and carries the verifier. Sending the challenge
/// instead, or omitting the verifier, defeats PKCE entirely.
#[tokio::test]
async fn the_code_exchange_sends_the_verifier_form_encoded() {
    let dir = tempfile::tempdir().unwrap();
    let response = serde_json::json!({
        "access_token": token_with(serde_json::json!({ "exp": 4_000 })),
        "refresh_token": "r-1",
        "id_token": token_with(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_login" },
        })),
    })
    .to_string();

    let server = AuthServer::start(200, &response, 0).await;
    let store: Arc<dyn AccountStore> =
        Arc::new(FileStore::new(dir.path().join("credentials.json")));
    let authorization = flow::begin(1455);

    let credentials = login::complete(
        &reqwest::Client::new(),
        &server.url,
        "client-abc",
        &authorization,
        &authorization.state,
        "the-code",
        &store,
        None,
    )
    .await
    .expect("the exchange should succeed");

    let body = &server.bodies()[0];
    assert!(body.contains("grant_type=authorization_code"), "{body}");
    assert!(body.contains("code=the-code"), "{body}");
    assert!(
        body.contains(&format!("code_verifier={}", authorization.pkce.verifier())),
        "the verifier must be sent: {body}"
    );
    assert!(
        !body.contains(authorization.pkce.challenge()),
        "the challenge belongs in the authorization request, not here: {body}"
    );

    // Expiry and account both come from claims, because the response carries
    // neither as a field.
    assert_eq!(credentials.expires_at, Some(4_000));
    assert_eq!(credentials.account_id.as_deref(), Some("acct_login"));

    // And it is persisted, or the next run would ask for login again.
    assert_eq!(store.load().unwrap().unwrap().refresh_token, "r-1");
}

#[tokio::test]
async fn a_refused_exchange_stores_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(400, r#"{"error":"invalid_grant"}"#, 0).await;
    let store: Arc<dyn AccountStore> =
        Arc::new(FileStore::new(dir.path().join("credentials.json")));
    let authorization = flow::begin(1455);

    let error = login::complete(
        &reqwest::Client::new(),
        &server.url,
        "client-abc",
        &authorization,
        &authorization.state,
        "bad-code",
        &store,
        None,
    )
    .await
    .expect_err("a refused exchange should fail");

    assert_eq!(
        error.kind,
        codex_cc_proxy_core::anthropic::ErrorKind::AuthenticationError
    );
    assert!(store.load().unwrap().is_none(), "nothing should be stored");
}

#[test]
fn the_callback_query_yields_the_code_and_state() {
    let (code, state) = login::parse_callback("code=abc123&state=xyz").unwrap();
    assert_eq!(code, "abc123");
    assert_eq!(state, "xyz");
}

/// The server's own message is the only useful diagnostic when a user declines.
#[test]
fn a_declined_authorization_reports_the_reason() {
    let error =
        login::parse_callback("error=access_denied&error_description=The%20user%20declined")
            .expect_err("a declined authorization should fail");

    assert!(
        error.message.contains("The user declined"),
        "{}",
        error.message
    );
}

#[test]
fn a_callback_without_a_code_is_an_error() {
    assert!(login::parse_callback("state=only").is_err());
    assert!(login::parse_callback("").is_err());
}

/// The authorization request asks for exactly what the proxy uses.
///
/// Least privilege: a scope the proxy never exercises is one more thing a
/// stolen token could do. This proxy invokes no connector, so it does not ask
/// to.
#[test]
fn the_authorization_request_asks_for_no_scope_it_does_not_use() {
    let authorization = flow::begin(1455);

    for required in ["openid", "profile", "email", "offline_access"] {
        assert!(
            flow::SCOPE.split(' ').any(|scope| scope == required),
            "{required} is needed and missing"
        );
    }

    assert!(
        !flow::SCOPE.contains("connectors"),
        "connector scopes are refused for this client: {}",
        flow::SCOPE
    );
    // And nothing else has crept in.
    assert_eq!(flow::SCOPE.split(' ').count(), 4);
    assert!(
        authorization
            .url
            .contains("scope=openid%20profile%20email%20offline_access&")
    );
}

// ---------------------------------------------------------------------------
// §8 — more than one account in one store.
// ---------------------------------------------------------------------------

use codex_cc_proxy::auth::store::AccountStore;

/// A grant belonging to somebody else, distinguishable from `sample()` in
/// every field that matters.
fn other() -> Credentials {
    Credentials {
        access_token: "other-access".to_owned(),
        refresh_token: "other-refresh".to_owned(),
        id_token: Some("other-id".to_owned()),
        account_id: Some("acct_456".to_owned()),
        expires_at: Some(1_900_000_000),
    }
}

/// A credential file written before this store held more than one account is
/// read as the one account it describes. Anything else costs a re-login for a
/// grant that is sitting right there and still valid.
#[test]
fn a_file_from_the_single_account_build_loads_as_one_selected_account() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&sample()).unwrap().as_bytes(),
    )
    .unwrap();

    let store = FileStore::new(&path);

    let loaded = store.load().unwrap().expect("the stored grant should load");
    assert_eq!(loaded.access_token, "access-secret");
    assert_eq!(loaded.refresh_token, "refresh-secret");

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "acct_123");
    assert_eq!(accounts[0].account_id.as_deref(), Some("acct_123"));
    assert!(accounts[0].selected, "the only account serves turns");
}

/// The migration survives the next write. A refresh saves the rotated grant,
/// and it must land in the account the old file described rather than beside
/// it.
#[test]
fn a_migrated_account_keeps_its_place_through_the_next_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    std::fs::write(&path, serde_json::to_string(&sample()).unwrap().as_bytes()).unwrap();

    // What a refresh does: read the grant, then write it back rotated. A store
    // that dropped the old file on the way in would have nothing to read here.
    let store = FileStore::new(&path);
    let loaded = store
        .load()
        .unwrap()
        .expect("the migrated grant should load");
    store
        .save(&Credentials {
            access_token: "rotated".to_owned(),
            ..loaded
        })
        .unwrap();

    let accounts = store.accounts().unwrap();
    assert_eq!(
        accounts.len(),
        1,
        "the save added an account instead of updating one"
    );
    assert_eq!(accounts[0].name, "acct_123");
    let stored = store.load().unwrap().unwrap();
    assert_eq!(stored.access_token, "rotated");
    assert_eq!(
        stored.refresh_token, "refresh-secret",
        "the rest of the migrated grant should survive the write"
    );
}

/// Logging in twice leaves two usable grants rather than one. The second
/// login serves turns from then on, and the first is still there to go back
/// to.
#[test]
fn logging_in_twice_leaves_two_usable_grants() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    assert_eq!(store.add(&sample(), None).unwrap(), "acct_123");
    assert_eq!(store.add(&other(), None).unwrap(), "acct_456");

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 2);
    assert!(
        !accounts[0].selected,
        "the first login no longer serves turns"
    );
    assert!(accounts[1].selected, "the newest login serves turns");
    assert_eq!(store.load().unwrap().unwrap().access_token, "other-access");

    store.select("acct_123").unwrap();
    assert_eq!(
        store.load().unwrap().unwrap().access_token,
        "access-secret",
        "the first account is still usable"
    );
}

/// Authorizing the same account twice replaces its grant. Two entries for one
/// account would be two refresh-token families against one grant, which is the
/// arrangement §8 exists to prevent.
#[test]
fn authorizing_the_same_account_again_replaces_its_grant() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    store.add(&sample(), None).unwrap();
    store
        .add(
            &Credentials {
                refresh_token: "re-authorized".to_owned(),
                ..sample()
            },
            None,
        )
        .unwrap();

    assert_eq!(store.accounts().unwrap().len(), 1);
    assert_eq!(
        store.load().unwrap().unwrap().refresh_token,
        "re-authorized"
    );
}

/// A label names the account. The id is what the backend calls it; a label is
/// what the operator calls it, and one of the two is memorable.
#[test]
fn a_label_names_the_account() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    assert_eq!(store.add(&sample(), Some("work")).unwrap(), "work");

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts[0].name, "work");
    // The id it belongs to is still reported: the label is a local name, not a
    // replacement for what the backend knows.
    assert_eq!(accounts[0].account_id.as_deref(), Some("acct_123"));
}

/// A grant whose id token carried no account id is still storable. The name is
/// assigned rather than invented from the grant: nothing in it is an account
/// id, and treating a token as one would be exactly the fabrication §8
/// forbids.
#[test]
fn an_account_with_no_id_is_named_rather_than_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    let name = store
        .add(
            &Credentials {
                account_id: None,
                ..sample()
            },
            None,
        )
        .unwrap();

    assert_eq!(name, "account-1");
    assert_eq!(store.accounts().unwrap()[0].account_id, None);
}

/// Selecting something that is not there says what is.
#[test]
fn selecting_an_unknown_account_names_the_known_ones() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();

    let error = store.select("nobody").unwrap_err().to_string();

    assert!(error.contains("nobody"), "{error}");
    assert!(error.contains("acct_123"), "{error}");
}

/// Clearing one account leaves the rest usable, and something still serves
/// turns afterwards.
#[test]
fn clearing_one_account_leaves_the_rest_usable() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();
    store.add(&other(), None).unwrap();

    // `clear` is the selected account, which is the one the second login left
    // serving turns.
    store.clear().unwrap();

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "acct_123");
    assert!(accounts[0].selected, "something must still serve turns");
    assert_eq!(store.load().unwrap().unwrap().access_token, "access-secret");

    // And clearing the last one empties the store, as it always did.
    store.clear().unwrap();
    assert!(store.load().unwrap().is_none());
    assert!(store.accounts().unwrap().is_empty());
}

/// Removing an account that is not selected leaves the selection alone.
#[test]
fn removing_an_unselected_account_leaves_the_selection_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();
    store.add(&other(), None).unwrap();

    store.remove("acct_123").unwrap();

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "acct_456");
    assert_eq!(store.load().unwrap().unwrap().access_token, "other-access");
}

/// A selection naming an account that is not there falls back to the first
/// stored one. A file that names a missing account still holds usable grants,
/// and reporting "not authenticated" there would send an operator to re-login
/// for nothing.
#[test]
fn a_selection_naming_nothing_falls_back_to_the_first_account() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let store = FileStore::new(&path);
    store.add(&sample(), None).unwrap();

    let mut file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    file["selected"] = Value::from("departed");
    std::fs::write(&path, file.to_string()).unwrap();

    assert_eq!(store.load().unwrap().unwrap().access_token, "access-secret");
    assert!(store.accounts().unwrap()[0].selected);
}

/// The account list is rendered to whoever asks `status`. Nothing in it may be
/// a token: this is the one shape in the credential module that is meant to
/// leave the process.
#[test]
fn the_account_list_carries_no_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();

    let accounts = store.accounts().unwrap();
    let rendered = format!(
        "{}{:?}",
        serde_json::to_string(&accounts).unwrap(),
        accounts
    );

    for secret in ["access-secret", "refresh-secret", "id-secret"] {
        assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
    }
    assert!(rendered.contains("acct_123"), "{rendered}");
}

/// §8 — a refresh moves one account's grant and nothing else.
///
/// Refresh-token families rotate. Two accounts in one store hold two separate
/// families, so refreshing one must leave the other's byte for byte where it
/// was: an account whose stored token was overwritten by another account's
/// rotation is an account one refresh away from holding nothing, and the
/// failure would not appear until the operator switched to it.
#[tokio::test]
async fn a_refresh_on_one_account_leaves_the_other_grant_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(200, &fresh_token_response(9_000), 0).await;
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));

    // Both expired, so either would refresh if asked.
    store
        .add(
            &Credentials {
                expires_at: Some(1_000),
                ..sample()
            },
            None,
        )
        .unwrap();
    store
        .add(
            &Credentials {
                expires_at: Some(1_000),
                ..other()
            },
            None,
        )
        .unwrap();
    store.select("acct_123").unwrap();

    let untouched = {
        let all = store.accounts().unwrap();
        all.iter()
            .find(|account| account.name == "acct_456")
            .cloned()
            .expect("the second account should be stored")
    };

    let source = TokenSource::new(
        Arc::clone(&store) as Arc<dyn CredentialStore>,
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );
    source.access_token().await.unwrap();

    // The selected account rotated.
    assert_eq!(store.load().unwrap().unwrap().refresh_token, "new-refresh");

    // The other one did not.
    store.select("acct_456").unwrap();
    let stored = store.load().unwrap().unwrap();
    assert_eq!(stored.refresh_token, "other-refresh");
    assert_eq!(stored.access_token, "other-access");
    assert_eq!(stored.expires_at, Some(1_000));
    assert_eq!(
        store
            .accounts()
            .unwrap()
            .iter()
            .find(|account| account.name == "acct_456")
            .map(|account| account.expires_at),
        Some(untouched.expires_at)
    );

    // And it still serves a turn, spending its own refresh token rather than
    // the one the first account just rotated into place.
    let second = TokenSource::new(
        Arc::clone(&store) as Arc<dyn CredentialStore>,
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );
    second.access_token().await.unwrap();

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2, "one refresh each");
    let sent = |body: &str| -> String {
        serde_json::from_str::<Value>(body).unwrap()["refresh_token"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(sent(&bodies[0]), "refresh-secret");
    assert_eq!(sent(&bodies[1]), "other-refresh");
}

/// A login adds an account rather than overwriting whichever one was there.
///
/// The store is where that rule lives, and this is the wiring: an exchange
/// that called `save` would pass every store-level test and still retire a
/// working grant the first time an operator authorized a second account.
#[tokio::test]
async fn a_second_login_adds_an_account_rather_than_replacing_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
    let handle: Arc<dyn AccountStore> = Arc::clone(&store) as Arc<dyn AccountStore>;

    for (account, refresh) in [("acct_first", "r-first"), ("acct_second", "r-second")] {
        let response = serde_json::json!({
            "access_token": token_with(serde_json::json!({ "exp": 4_000 })),
            "refresh_token": refresh,
            "id_token": token_with(serde_json::json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": account },
            })),
        })
        .to_string();
        let server = AuthServer::start(200, &response, 0).await;
        let authorization = flow::begin(1455);

        login::complete(
            &reqwest::Client::new(),
            &server.url,
            "client-abc",
            &authorization,
            &authorization.state,
            "the-code",
            &handle,
            None,
        )
        .await
        .expect("the exchange should succeed");
    }

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 2, "the second login replaced the first");
    assert_eq!(accounts[0].name, "acct_first");
    assert_eq!(accounts[1].name, "acct_second");
    assert!(accounts[1].selected, "the newest login serves turns");
    assert_eq!(store.load().unwrap().unwrap().refresh_token, "r-second");

    // The label an operator gives names the account instead.
    let response = serde_json::json!({
        "access_token": token_with(serde_json::json!({ "exp": 4_000 })),
        "refresh_token": "r-third",
        "id_token": token_with(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_third" },
        })),
    })
    .to_string();
    let server = AuthServer::start(200, &response, 0).await;
    let authorization = flow::begin(1455);
    login::complete(
        &reqwest::Client::new(),
        &server.url,
        "client-abc",
        &authorization,
        &authorization.state,
        "the-code",
        &handle,
        Some("spare"),
    )
    .await
    .unwrap();

    assert_eq!(store.accounts().unwrap()[2].name, "spare");
}

/// An account is identified by its account id, not by the name it happens to
/// be stored under.
///
/// Authorizing an account already stored under a different name must replace
/// that account rather than add a second entry for it. Two entries for one
/// account are two holders of one refresh-token family, which is the
/// arrangement §8.1 exists to keep out of the store: the first rotation
/// retires the other entry's token, and the operator is left with an account
/// they can see and can never spend.
#[test]
fn re_authorizing_an_account_stored_under_another_name_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    store.add(&sample(), Some("work")).unwrap();
    let name = store
        .add(
            &Credentials {
                refresh_token: "re-authorized".to_owned(),
                ..sample()
            },
            None,
        )
        .unwrap();

    let accounts = store.accounts().unwrap();
    assert_eq!(
        accounts.len(),
        1,
        "one account, two entries sharing its refresh-token family: {accounts:?}"
    );
    assert_eq!(name, "work", "the name it is already stored under");
    assert_eq!(
        store.load().unwrap().unwrap().refresh_token,
        "re-authorized"
    );

    // And a new label renames the account rather than duplicating it.
    let name = store.add(&sample(), Some("day-job")).unwrap();
    assert_eq!(name, "day-job");
    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "day-job");
}

/// A refresh writes the grant of the account it read, even if the selection
/// moved while the request was in flight.
///
/// `save` resolving the target by selection is a read-modify-write across a
/// network round trip: switch accounts in the middle and one account's rotated
/// grant lands in another's entry, destroying a refresh token that only a
/// re-login can replace and leaving that account authenticating as somebody
/// else.
#[test]
fn a_grant_is_saved_to_the_account_it_belongs_to_not_the_selected_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();
    store.add(&other(), None).unwrap();

    // `acct_456` is selected; the grant being written belongs to `acct_123`.
    store
        .save(&Credentials {
            refresh_token: "rotated".to_owned(),
            ..sample()
        })
        .unwrap();

    store.select("acct_123").unwrap();
    assert_eq!(store.load().unwrap().unwrap().refresh_token, "rotated");

    store.select("acct_456").unwrap();
    let stored = store.load().unwrap().unwrap();
    assert_eq!(
        stored.refresh_token, "other-refresh",
        "another account's rotation landed in this one's entry"
    );
    assert_eq!(stored.access_token, "other-access");
}

/// The store is replaced, never truncated in place.
///
/// The file holds every account now. A write interrupted between truncation
/// and completion would leave the whole store unreadable — every account gone
/// for one account's rotated token — so the new content is written beside it
/// and moved over it.
#[cfg(unix)]
#[test]
fn a_write_leaves_no_window_where_the_store_is_half_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let store = FileStore::new(&path);
    store.add(&sample(), None).unwrap();
    store.add(&other(), None).unwrap();

    let before = std::fs::read_to_string(&path).unwrap();
    let inode = |path: &std::path::Path| -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    };
    let first = inode(&path);

    store
        .save(&Credentials {
            refresh_token: "rotated".to_owned(),
            ..other()
        })
        .unwrap();

    assert_ne!(
        inode(&path),
        first,
        "the file was written in place rather than replaced"
    );
    assert_ne!(std::fs::read_to_string(&path).unwrap(), before);
    // Nothing is left lying around beside it.
    let strays: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .filter(|name| name != "credentials.json")
        .collect();
    assert!(strays.is_empty(), "left behind: {strays:?}");

    // Still private, and still both accounts.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(store.accounts().unwrap().len(), 2);
}

/// §8 — a refusal is about one refresh token, not about the process holding it.
///
/// The message a refused grant produces tells the operator to log in again.
/// They do, and the new grant lands in the store the daemon reads on every
/// turn — but a refusal latched for the life of the process short-circuits
/// before it ever gets there, so the documented recovery does not recover
/// until the daemon is restarted. A login through the CLI never touches the
/// daemon at all, which is the path most people take.
#[tokio::test]
async fn a_new_grant_ends_a_refusal_without_restarting() {
    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(400, r#"{"error":"refresh_token_reused"}"#, 0).await;
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
    store
        .add(
            &Credentials {
                expires_at: Some(1_000),
                ..sample()
            },
            None,
        )
        .unwrap();

    let source = TokenSource::new(
        Arc::clone(&store) as Arc<dyn CredentialStore>,
        server.url.clone(),
        "client-abc",
        Arc::new(FixedClock(2_000)),
    );
    source
        .access_token()
        .await
        .expect_err("the stored grant is refused");
    assert!(source.is_dead());

    // What a login does: a fresh grant for another account, selected.
    store
        .add(
            &Credentials {
                access_token: "freshly-authorized".to_owned(),
                refresh_token: "fresh-refresh".to_owned(),
                expires_at: Some(9_000),
                ..other()
            },
            None,
        )
        .unwrap();

    assert!(
        !source.is_dead(),
        "the refused grant is no longer the one held"
    );
    assert_eq!(
        source.access_token().await.unwrap(),
        "freshly-authorized",
        "the recovery the refusal's own message describes"
    );

    // And the refused grant is still refused: selecting it back must not put
    // the refusal loop the flag exists to prevent back on the table.
    store.select("acct_123").unwrap();
    assert!(source.is_dead());
    source
        .access_token()
        .await
        .expect_err("a refused grant is not retried");
    assert_eq!(
        server.bodies().len(),
        1,
        "the refused token was sent upstream again"
    );
}

/// A label that already names a different account is refused, not honoured.
///
/// `login --as work` months after the first one, with the browser signed into
/// somebody else: the label resolves to an entry holding another account's
/// grant, and writing over it retires a working grant with nothing said —
/// exactly what the `add`/`save` split exists to prevent. Refusing costs the
/// authorization that was just spent, which one more login replaces; the other
/// way costs a grant that may not be replaceable at all.
#[test]
fn a_label_already_naming_another_account_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), Some("work")).unwrap();

    let error = store.add(&other(), Some("work")).unwrap_err().to_string();

    assert!(error.contains("work"), "{error}");
    assert!(
        error.contains("acct_123"),
        "the name's current owner: {error}"
    );

    // Nothing was disturbed.
    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].account_id.as_deref(), Some("acct_123"));
    assert_eq!(store.load().unwrap().unwrap().access_token, "access-secret");

    // The same label for the account that already holds it still works: that
    // is a re-authorization, not a collision.
    store.add(&sample(), Some("work")).unwrap();
    assert_eq!(store.accounts().unwrap().len(), 1);
}
