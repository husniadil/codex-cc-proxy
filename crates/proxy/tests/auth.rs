//! `docs/proxy-behavior.md` §8 — credentials.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use proxenos::auth::pkce::Pkce;
use proxenos::auth::store::CredentialStore;
use proxenos::auth::store::Credentials;
use proxenos::auth::store::FileStore;
use proxenos::auth::store::Provider;
use proxenos::auth::store::WritePoint;
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
    // Clearing what is already gone is not an error: `accounts.forget` must be safe
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
use proxenos::auth::tokens::Clock;
use proxenos::auth::tokens::TokenSource;
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
        proxenos_core::anthropic::ErrorKind::AuthenticationError
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
        proxenos_core::anthropic::ErrorKind::OverloadedError,
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
use proxenos::auth::flow;
use proxenos::auth::jwt;

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

use proxenos::auth::login;

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
        proxenos_core::anthropic::ErrorKind::AuthenticationError
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

use proxenos::auth::store::AccountStore;

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

/// Logging in twice leaves two usable grants rather than one. The account
/// already serving turns keeps serving them, and the new one is there to
/// switch to.
#[test]
fn logging_in_twice_leaves_two_usable_grants() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    assert_eq!(store.add(&sample(), None).unwrap(), "acct_123");
    assert_eq!(store.add(&other(), None).unwrap(), "acct_456");

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 2);
    assert!(accounts[0].selected, "the first login still serves turns");
    assert!(
        !accounts[1].selected,
        "a login stores a credential; it does not choose what serves"
    );
    assert_eq!(store.load().unwrap().unwrap().access_token, "access-secret");

    store.select("acct_456").unwrap();
    assert_eq!(
        store.load().unwrap().unwrap().access_token,
        "other-access",
        "the second account is usable once it is chosen"
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
    store.select("acct_456").unwrap();

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
    assert!(
        accounts[0].selected,
        "the account already serving turns keeps serving them"
    );
    assert_eq!(store.load().unwrap().unwrap().refresh_token, "r-first");

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
    // Nothing is left lying around beside it. The lock is the one permanent
    // neighbour: every writer takes it, so it is there before the first write
    // and stays after the last. A half-finished replacement is what this is
    // looking for.
    let strays: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .filter(|name| name != "credentials.json" && name != "credentials.json.lock")
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
    store.select("acct_456").unwrap();

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

/// Naming an account in an empty store says the store is empty.
///
/// The refusal lists what is stored, and with nothing stored that list is a
/// blank the reader has to interpret. What they need to be told is to log in.
#[test]
fn selecting_from_an_empty_store_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    let error = store.select("work").unwrap_err().to_string();

    assert!(error.contains("login"), "{error}");
    assert!(
        !error.contains("stored: "),
        "an empty list is not an answer: {error}"
    );
}

/// Add an account by editing the file directly, taking no lock.
///
/// What an older binary or a hand edit does. Built by cloning an entry already
/// there, so the test states only what it means to change and cannot drift
/// from the stored shape.
fn add_account_behind_the_lock(path: &std::path::Path, name: &str, account_id: &str) {
    let mut file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut entry = file["accounts"][0].clone();
    entry["name"] = name.into();
    entry["account_id"] = account_id.into();
    file["accounts"].as_array_mut().unwrap().push(entry);
    std::fs::write(path, serde_json::to_string_pretty(&file).unwrap()).unwrap();
}

/// A write that finds the file changed since it read starts over.
///
/// Every write is a read, a change, and a replacement of the whole file, so two
/// writers overlapping used to mean one of them silently lost everything the
/// other had done — and with several accounts in one file, "everything" is an
/// account, not a stale token.
///
/// The writer simulated here takes no lock: it edits the file in place, which
/// is what an older binary or a hand edit does. The lock cannot cover those,
/// so the comparison still has to.
#[test]
fn a_write_that_lost_a_race_is_redone_rather_than_lost() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let store = FileStore::new(&path);
    store.add(&sample(), None).unwrap();

    // Another writer, landing between this one's read and its comparison.
    // Once, so the retry has something to converge on.
    let raced = std::sync::atomic::AtomicBool::new(false);
    let edited = path;
    store.on_write_for_test(move |point| {
        if point == WritePoint::BeforeComparison
            && !raced.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            add_account_behind_the_lock(&edited, "interloper", "acct_interloper");
        }
    });

    store.add(&other(), None).unwrap();

    let names: Vec<String> = store
        .accounts()
        .unwrap()
        .into_iter()
        .map(|account| account.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "acct_123".to_owned(),
            "interloper".to_owned(),
            "acct_456".to_owned()
        ],
        "the other writer's account was overwritten"
    );
}

/// A write that cannot take its lock says what to do about it.
///
/// The lock lives beside the credentials, so a directory that cannot hold one
/// stops every write. Locking is also not something every filesystem does — a
/// home on a network mount is the case that exists — and there the failure is
/// the filesystem's, not the operator's. Either way the answer is the same and
/// the message has to carry it, because "could not lock the credential file"
/// on its own reads as a bug in this program.
#[test]
fn a_write_that_cannot_lock_names_the_way_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    // Something already occupying the lock's name that is not a file.
    std::fs::create_dir(dir.path().join("credentials.json.lock")).unwrap();

    let error = FileStore::new(&path)
        .add(&sample(), None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("PROXENOS_HOME"),
        "nothing to act on: {error}"
    );
}

/// A writer waits for the one already writing, rather than landing inside it.
///
/// The comparison covers the gap between a write's read and its check. It
/// cannot cover the gap between the check and the replacement: those are two
/// operations, and a writer that lands between them is copied over by a
/// replacement that already decided nothing had changed. Only a lock the
/// filesystem enforces closes that, which is why this drives the second writer
/// into exactly that gap.
///
/// Both writers are `FileStore`, so both take the lock — two open descriptions
/// in one process conflict the same way two processes do.
#[test]
fn a_writer_waits_rather_than_landing_inside_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let store = FileStore::new(&path);
    store.add(&sample(), None).unwrap();

    let (start, wait_to_start) = std::sync::mpsc::channel();
    let (finished, wait_for_finish) = std::sync::mpsc::channel();
    let second = {
        std::thread::spawn(move || {
            wait_to_start.recv().unwrap();
            FileStore::new(&path)
                .add(
                    &Credentials {
                        account_id: Some("acct_second".to_owned()),
                        ..other()
                    },
                    Some("second"),
                )
                .unwrap();
            let _ = finished.send(());
        })
    };

    // Inside the window the comparison cannot cover. The wait is bounded
    // because the passing case is the one that never finishes: a second writer
    // held off by the lock cannot report done until this write has released
    // it. Timing out here is the evidence, and without the lock the second
    // writer reports done in milliseconds and this one copies over it.
    let started = std::sync::atomic::AtomicBool::new(false);
    let wait_for_finish = std::sync::Mutex::new(wait_for_finish);
    store.on_write_for_test(move |point| {
        if point == WritePoint::AfterComparison
            && !started.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            start.send(()).unwrap();
            let _ = wait_for_finish
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(2));
        }
    });

    store.add(&other(), None).unwrap();
    second.join().unwrap();

    let names: Vec<String> = store
        .accounts()
        .unwrap()
        .into_iter()
        .map(|account| account.name)
        .collect();
    assert!(
        names.contains(&"second".to_owned()),
        "the second writer landed inside the first one's replacement and was copied over: {names:?}"
    );
    assert!(
        names.contains(&"acct_456".to_owned()),
        "the first writer's own account is missing: {names:?}"
    );
}

/// Renaming moves the name and nothing else.
///
/// A login without `--as` names the account by the id the backend knows it by,
/// which is a UUID nobody wants to type at `--use`. Changing it should not cost
/// an authorization: the grant is fine, only what this store calls it is
/// wrong.
#[test]
fn renaming_moves_the_name_and_keeps_the_grant() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();
    store.add(&other(), None).unwrap();
    store.select("acct_123").unwrap();

    store.rename("acct_123", "work").unwrap();

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts[0].name, "work");
    assert_eq!(accounts[0].account_id.as_deref(), Some("acct_123"));
    assert!(
        accounts[0].selected,
        "the account serving turns must still be serving: {accounts:?}"
    );
    assert_eq!(
        store.load().unwrap().unwrap().access_token,
        "access-secret",
        "the grant should be untouched"
    );
    // The other account is where it was.
    assert_eq!(accounts[1].name, "acct_456");
    assert_eq!(accounts.len(), 2);

    // And the new name is what selects it from now on.
    store.select("work").unwrap();
    assert!(store.select("acct_123").is_err());
}

/// Renaming to a name another account holds is refused, for the same reason a
/// colliding label is: the store would otherwise have two accounts answering
/// to one name, and whichever `--use` found first would be the one that got
/// the turns.
#[test]
fn renaming_onto_another_account_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), Some("work")).unwrap();
    store.add(&other(), Some("spare")).unwrap();

    let error = store.rename("spare", "work").unwrap_err().to_string();
    assert!(error.contains("work"), "{error}");

    let names: Vec<String> = store
        .accounts()
        .unwrap()
        .into_iter()
        .map(|account| account.name)
        .collect();
    assert_eq!(names, vec!["work".to_owned(), "spare".to_owned()]);

    // Renaming an account to what it is already called is not a collision.
    store.rename("spare", "spare").unwrap();
    assert_eq!(store.accounts().unwrap()[1].name, "spare");

    // And a name nobody holds says so.
    let error = store.rename("ghost", "whatever").unwrap_err().to_string();
    assert!(error.contains("ghost"), "{error}");
}

// ---------------------------------------------------------------------------
// §8 — a credential that is not a subscription grant.
// ---------------------------------------------------------------------------

use proxenos::auth::store::Credential;

/// A key is an account like any other, of a different kind.
///
/// It has no refresh, no expiry and no account id, and nothing invents one for
/// it: a plausible expiry would drive a refresh that cannot happen, and a
/// plausible account id would be sent upstream as a header the key endpoint
/// never asked for.
#[test]
fn a_key_is_stored_as_an_account_of_its_own_kind() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    store
        .add_key("billing", "key-secret-value", Provider::Codex)
        .unwrap();

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "billing");
    assert_eq!(accounts[0].kind, "key");
    assert!(accounts[0].selected);
    assert_eq!(accounts[0].account_id, None);
    assert_eq!(accounts[0].expires_at, None);
    assert_eq!(accounts[0].email, None);

    // The listing is the shape that leaves the process.
    let rendered = format!(
        "{}{:?}",
        serde_json::to_string(&accounts).unwrap(),
        accounts
    );
    assert!(
        !rendered.contains("key-secret-value"),
        "leaked the key: {rendered}"
    );

    match store.credential().unwrap() {
        Some(Credential::Key(key)) => assert_eq!(key.value(), "key-secret-value"),
        other => panic!("expected a key, got {other:?}"),
    }
    // And `Debug` on the credential itself does not carry it either.
    assert!(
        !format!("{:?}", store.credential().unwrap()).contains("key-secret-value"),
        "Debug leaked the key"
    );
}

/// A grant and a key coexist, and switching between them is switching
/// accounts. Nothing about the second kind disturbs the first.
#[test]
fn a_key_and_a_grant_are_two_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();
    store
        .add_key("billing", "key-secret-value", Provider::Codex)
        .unwrap();
    store.select("billing").unwrap();

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].kind, "grant");
    assert_eq!(accounts[1].kind, "key");
    assert!(accounts[1].selected, "the newest is the one serving turns");

    store.select("acct_123").unwrap();
    assert!(matches!(
        store.credential().unwrap(),
        Some(Credential::Grant(_))
    ));
    assert_eq!(store.load().unwrap().unwrap().access_token, "access-secret");

    store.select("billing").unwrap();
    assert!(matches!(
        store.credential().unwrap(),
        Some(Credential::Key(_))
    ));
}

/// A credential file written before keys existed is every bit a file of
/// grants. The kind is absent there, and absent means grant.
#[test]
fn a_file_from_before_keys_reads_as_grants() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "selected": "work",
            "accounts": [{
                "name": "work",
                "access_token": "access-secret",
                "refresh_token": "refresh-secret",
                "account_id": "acct_123",
                "expires_at": 1_800_000_000_u64,
            }],
        })
        .to_string(),
    )
    .unwrap();

    let store = FileStore::new(&path);

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].kind, "grant");
    assert_eq!(
        store.load().unwrap().unwrap().refresh_token,
        "refresh-secret"
    );

    // And the single-grant shape from before accounts existed, too.
    let older = dir.path().join("older.json");
    std::fs::write(&older, serde_json::to_string(&sample()).unwrap()).unwrap();
    let store = FileStore::new(&older);
    assert_eq!(store.accounts().unwrap()[0].kind, "grant");
}

/// A key cannot be renamed onto a grant's name, forgotten differently, or
/// otherwise treated as a second class of thing: every account verb works on
/// it.
#[test]
fn the_account_verbs_work_on_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();
    store
        .add_key("billing", "key-secret-value", Provider::Codex)
        .unwrap();
    store.select("billing").unwrap();

    store.rename("billing", "spend").unwrap();
    assert_eq!(store.accounts().unwrap()[1].name, "spend");
    assert!(store.accounts().unwrap()[1].selected);

    store.remove("spend").unwrap();
    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "acct_123");
    assert!(accounts[0].selected);
}

/// §8 — what each kind puts on the wire.
///
/// The header set is the whole difference between the two endpoints. A grant
/// identifies a subscription client and the account it is spending; a key
/// identifies nothing but itself, and either of the other two headers on a key
/// request is a header the endpoint taking it never asked for.
#[tokio::test]
async fn each_kind_authorizes_with_the_headers_its_endpoint_expects() {
    use proxenos::auth::authorize::Authorizer;
    use proxenos::auth::authorize::Kind;

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
    let authorizer = proxenos::auth::authorize::AccountAuthorizer::new(
        Arc::clone(&store) as Arc<dyn AccountStore>,
        Arc::new(TokenSource::new(
            Arc::clone(&store) as Arc<dyn CredentialStore>,
            String::new(),
            "client-abc",
            Arc::new(FixedClock(2_000)),
        )),
    );

    // Nothing stored: the answer says what to do about it rather than sending
    // an empty authorization.
    let error = authorizer
        .authorize(None)
        .await
        .expect_err("an empty store cannot authorize");
    assert!(error.to_string().contains("login"), "{error}");

    store
        .add(
            &Credentials {
                expires_at: Some(9_000),
                ..sample()
            },
            None,
        )
        .unwrap();

    let grant = authorizer.authorize(None).await.unwrap();
    assert_eq!(grant.kind, Kind::Subscription);
    let header = |set: &proxenos::auth::authorize::Authorization, name: &str| {
        set.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    assert_eq!(
        header(&grant, "authorization").as_deref(),
        Some("Bearer access-secret")
    );
    assert_eq!(
        header(&grant, "chatgpt-account-id").as_deref(),
        Some("acct_123")
    );
    assert!(header(&grant, "originator").is_some());

    store
        .add_key("billing", "key-secret-value", Provider::Codex)
        .unwrap();
    store.select("billing").unwrap();

    let key = authorizer.authorize(None).await.unwrap();
    assert_eq!(key.kind, Kind::Key);
    assert_eq!(
        header(&key, "authorization").as_deref(),
        Some("Bearer key-secret-value")
    );
    assert_eq!(
        header(&key, "chatgpt-account-id"),
        None,
        "a key has no account to name"
    );
    assert_eq!(
        header(&key, "originator"),
        None,
        "the originator identifies a subscription client"
    );
    assert_eq!(key.headers.len(), 1, "one header, and nothing else");
}

/// Storing a key under a name a grant already holds is refused.
///
/// `add` refuses the same collision, and for the same reason: the grant would
/// be gone with nothing said, and only a re-login brings it back. A key is
/// handed over rather than granted, which makes it easier to type by accident,
/// not safer.
#[test]
fn a_key_cannot_be_stored_over_a_grant() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), Some("work")).unwrap();

    let error = store
        .add_key("work", "key-secret-value", Provider::Codex)
        .unwrap_err()
        .to_string();
    assert!(error.contains("work"), "{error}");

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].kind, "grant");
    assert_eq!(store.load().unwrap().unwrap().access_token, "access-secret");

    // Replacing a key with another key is not a collision: that is how a
    // rotated secret is stored.
    store.add_key("billing", "first", Provider::Codex).unwrap();
    store.add_key("billing", "second", Provider::Codex).unwrap();
    assert_eq!(store.accounts().unwrap().len(), 2);
}

/// A rotation with nowhere to go is refused rather than turned into a new
/// account.
///
/// A grant carrying no account id is matched by selection alone, and the
/// selection can move to a key while a refresh is in flight. Appending the
/// rotated grant there would silently move the operator off the account they
/// had just selected.
#[test]
fn a_rotation_that_belongs_to_no_stored_account_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store
        .add_key("billing", "key-secret-value", Provider::Codex)
        .unwrap();

    let error = store
        .save(&Credentials {
            account_id: None,
            ..sample()
        })
        .unwrap_err()
        .to_string();
    assert!(error.contains("account"), "{error}");

    let accounts = store.accounts().unwrap();
    assert_eq!(
        accounts.len(),
        1,
        "a rotation created an account: {accounts:?}"
    );
    assert_eq!(accounts[0].name, "billing");
    assert!(accounts[0].selected, "the selection moved");

    // An empty store still takes one: a caller holding nothing but
    // `CredentialStore` has to be able to store what it just obtained.
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.save(&sample()).unwrap();
    assert_eq!(store.accounts().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// §8.1 — the store answers for an account by name.
// ---------------------------------------------------------------------------

/// A pinned tier names an account, and the store has to answer for that one
/// rather than for the selection.
///
/// `credential()` answers for whichever account is serving turns, which is the
/// wrong question here: a pinned tier says which account its turns belong to,
/// and reading the selection would serve them as somebody else.
#[test]
fn the_store_answers_for_an_account_other_than_the_selected_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();
    store.add(&other(), None).unwrap();
    store.select("acct_123").unwrap();

    let pinned = store.credential_for("acct_456").unwrap();
    assert_eq!(
        pinned.grant().map(|grant| grant.refresh_token.as_str()),
        Some("other-refresh")
    );

    // And the selection is still what `credential()` answers for.
    assert_eq!(
        store
            .credential()
            .unwrap()
            .and_then(|held| held.grant().map(|grant| grant.refresh_token.clone())),
        Some("refresh-secret".to_owned())
    );
}

/// A pin naming an account that is not stored refuses, and the refusal names
/// it.
///
/// Never a fallback to the serving account: that spends the wrong
/// subscription's quota invisibly, which is the failure the consent gate
/// exists to prevent (`roadmap.md` v0.6.0). The name is in the message because
/// a mapping and a store are edited separately and either one could be the
/// half that is wrong.
#[test]
fn a_pin_naming_an_unstored_account_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));
    store.add(&sample(), None).unwrap();

    let error = store.credential_for("spare").unwrap_err();
    let rendered = format!("{error}");
    assert!(rendered.contains("spare"), "{rendered}");
    assert!(rendered.contains("acct_123"), "{rendered}");
}

// ---------------------------------------------------------------------------
// §7.1, §8.1 — authorizing as an account other than the one serving turns.
// ---------------------------------------------------------------------------

fn pinned_authorizer(
    store: &Arc<FileStore>,
    endpoint: &str,
    now: u64,
) -> proxenos::auth::authorize::AccountAuthorizer {
    proxenos::auth::authorize::AccountAuthorizer::new(
        Arc::clone(store) as Arc<dyn AccountStore>,
        Arc::new(TokenSource::new(
            Arc::clone(store) as Arc<dyn CredentialStore>,
            endpoint.to_owned(),
            "client-abc",
            Arc::new(FixedClock(now)),
        )),
    )
}

/// §7.1 — a pinned tier authorizes as the account it names.
///
/// The serving account is right there and its token would produce a working
/// turn, which is exactly why this cannot be left to fall back: the turn would
/// succeed against the wrong subscription's quota and nothing would say so.
#[tokio::test]
async fn a_pinned_account_authorizes_with_its_own_token() {
    use proxenos::auth::authorize::Authorizer;

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
    store
        .add(
            &Credentials {
                expires_at: Some(9_000),
                ..sample()
            },
            None,
        )
        .unwrap();
    store
        .add(
            &Credentials {
                expires_at: Some(9_000),
                ..other()
            },
            None,
        )
        .unwrap();
    store.select("acct_123").unwrap();

    let authorizer = pinned_authorizer(&store, "", 2_000);
    let header = |set: &proxenos::auth::authorize::Authorization, name: &str| {
        set.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };

    let serving = authorizer.authorize(None).await.unwrap();
    assert_eq!(
        header(&serving, "authorization").as_deref(),
        Some("Bearer access-secret")
    );

    let pinned = authorizer.authorize(Some("acct_456")).await.unwrap();
    assert_eq!(
        header(&pinned, "authorization").as_deref(),
        Some("Bearer other-access")
    );
    assert_eq!(
        header(&pinned, "chatgpt-account-id").as_deref(),
        Some("acct_456"),
        "the pinned account names itself upstream, not the one serving turns"
    );
}

/// A pin naming nothing stored refuses the turn, and never serves it as
/// somebody else.
#[tokio::test]
async fn a_pin_the_store_cannot_answer_for_refuses_the_turn() {
    use proxenos::auth::authorize::Authorizer;

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
    store
        .add(
            &Credentials {
                expires_at: Some(9_000),
                ..sample()
            },
            None,
        )
        .unwrap();

    let authorizer = pinned_authorizer(&store, "", 2_000);
    let error = authorizer
        .authorize(Some("spare"))
        .await
        .expect_err("a pin naming nothing stored cannot authorize");
    assert!(error.to_string().contains("spare"), "{error}");
}

/// §8.1 — refreshing a pinned grant lands in that account's entry.
///
/// The grant here carries **no account id**, which is the case `save` resolves
/// by falling back to the selection. Reached through the shared token source,
/// that rotation would be written over the serving account: one account
/// authenticating as another, and a refresh token only a re-login replaces
/// destroyed in the same write.
#[tokio::test]
async fn refreshing_a_pinned_account_writes_to_that_account() {
    use proxenos::auth::authorize::Authorizer;

    let dir = tempfile::tempdir().unwrap();
    let server = AuthServer::start(200, &fresh_token_response(9_000), 0).await;
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));

    // The serving account, fresh: nothing here should move.
    store
        .add(
            &Credentials {
                expires_at: Some(9_000),
                ..sample()
            },
            None,
        )
        .unwrap();
    // The pinned one, expired and anonymous.
    store
        .add(
            &Credentials {
                access_token: "spare-access".to_owned(),
                refresh_token: "spare-refresh".to_owned(),
                id_token: None,
                account_id: None,
                expires_at: Some(1_000),
            },
            Some("spare"),
        )
        .unwrap();
    store.select("acct_123").unwrap();

    let authorizer = pinned_authorizer(&store, &server.url, 2_000);
    authorizer.authorize(Some("spare")).await.unwrap();

    let rotated = store.credential_for("spare").unwrap();
    assert_eq!(
        rotated.grant().map(|grant| grant.refresh_token.as_str()),
        Some("new-refresh"),
        "the pinned account's own entry took the rotation"
    );

    let serving = store.credential_for("acct_123").unwrap();
    assert_eq!(
        serving.grant().map(|grant| grant.refresh_token.as_str()),
        Some("refresh-secret"),
        "the serving account's grant was not overwritten"
    );

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("spare-refresh"), "{}", bodies[0]);
}

/// Two accounts serving one client keep separate refresh state.
///
/// A refused grant is a fact about one refresh token, and a single shared
/// source would let one account's refusal answer for the other's — either
/// refusing a working account or retrying a token the backend has already
/// retired.
#[tokio::test]
async fn two_accounts_serving_at_once_keep_separate_refresh_state() {
    use proxenos::auth::authorize::Authorizer;

    let dir = tempfile::tempdir().unwrap();
    // Every refresh is refused as a dead grant.
    let server = AuthServer::start(400, r#"{"error":"invalid_grant"}"#, 0).await;
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));

    // The serving account is fresh and never needs a refresh.
    store
        .add(
            &Credentials {
                expires_at: Some(9_000),
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
            Some("spare"),
        )
        .unwrap();
    store.select("acct_123").unwrap();

    let authorizer = pinned_authorizer(&store, &server.url, 2_000);

    let refused = authorizer
        .authorize(Some("spare"))
        .await
        .expect_err("the pinned grant was refused");
    assert!(refused.to_string().contains("login"), "{refused}");

    // The serving account is unaffected by the other's refusal.
    authorizer
        .authorize(None)
        .await
        .expect("the serving account still authorizes");

    // And the refused grant is not retried.
    authorizer.authorize(Some("spare")).await.unwrap_err();
    assert_eq!(server.bodies().len(), 1, "a refused grant is not retried");
}

/// A key states which provider it is spent against, and the store keeps it.
///
/// `roadmap.md` v0.6.0 — routing reads the provider off the account, so a key
/// stored without one is a key that can only ever reach the first provider's
/// endpoint. The provider is a parameter rather than a default because the two
/// endpoints refuse each other's credentials, and a key that silently claimed
/// the wrong one would surface as an authentication failure naming the
/// credential rather than the destination.
#[test]
fn a_key_states_the_provider_it_is_spent_against() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    store
        .add_key("work", "key-secret-value", Provider::Codex)
        .unwrap();
    store
        .add_key("relay", "key-secret-value", Provider::Anthropic)
        .unwrap();

    let accounts = store.accounts().unwrap();
    assert_eq!(accounts[0].provider, "codex");
    assert_eq!(accounts[1].provider, "anthropic");

    // And it survives a reload: the field is written, not held in memory.
    let reopened = FileStore::new(dir.path().join("credentials.json"));
    let accounts = reopened.accounts().unwrap();
    assert_eq!(accounts[1].provider, "anthropic");
}

/// A login while another account is already serving stores the credential and
/// leaves the selection where it was.
///
/// Storing a credential and choosing what serves turns are two decisions, and
/// a login is only the first. Making it both means an operator who adds a
/// second account has silently moved every turn onto it.
#[test]
fn a_login_while_another_account_serves_leaves_the_selection_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    store.add(&sample(), None).unwrap();
    store.add(&other(), None).unwrap();
    store
        .add_key("keyed", "sk-ant-oat01-value", Provider::Anthropic)
        .unwrap();

    let accounts = store.accounts().unwrap();
    let serving: Vec<&str> = accounts
        .iter()
        .filter(|account| account.selected)
        .map(|account| account.name.as_str())
        .collect();
    assert_eq!(serving, vec!["acct_123"], "{accounts:?}");
    assert_eq!(accounts.len(), 3, "every login still stored: {accounts:?}");
}

/// The first login has nothing to displace, so it selects.
#[test]
fn a_first_login_selects_the_new_account() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    store.add(&sample(), None).unwrap();

    let accounts = store.accounts().unwrap();
    assert!(accounts[0].selected, "{accounts:?}");
}

/// And a first login by key selects too — one rule, not a per-flag one.
#[test]
fn a_first_login_by_key_selects_the_new_account() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("credentials.json"));

    store
        .add_key("keyed", "sk-ant-oat01-value", Provider::Anthropic)
        .unwrap();

    let accounts = store.accounts().unwrap();
    assert!(accounts[0].selected, "{accounts:?}");
}
