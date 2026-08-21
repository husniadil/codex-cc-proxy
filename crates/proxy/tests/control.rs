//! `docs/api.md` §3 — the control socket.
//!
//! Driven over a real socket, because "the CLI holds no state of its own" is
//! only true if every verb genuinely goes through this interface.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use proxenos::auth::authorize::Authorizer;
use proxenos::auth::store::AccountStore;
use proxenos::auth::store::CredentialStore;
use proxenos::auth::store::Credentials;
use proxenos::auth::store::FileStore;
use proxenos::auth::store::Provider;
use proxenos::catalog::Catalog;
use proxenos::catalog::CatalogSource;
use proxenos::config::ResolvedTier;
use proxenos::control;
use proxenos::control::handler::ControlState;
use proxenos::control::protocol::METHODS;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

/// An unsigned JWT carrying the given claims. Nothing here verifies one, and
/// nothing should — see the note on the `jwt` module.
fn id_token(claims: Value) -> String {
    use base64::Engine;
    let encode = |value: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
    format!(
        "{}.{}.{}",
        encode(br#"{"alg":"none"}"#),
        encode(claims.to_string().as_bytes()),
        encode(b"signature")
    )
}

fn tiers() -> Vec<ResolvedTier> {
    vec![
        ResolvedTier {
            defaulted: false,
            account: None,
            tier: "opus",
            model: "gpt-5.6-terra".to_owned(),
        },
        ResolvedTier {
            defaulted: false,
            account: None,
            tier: "sonnet",
            model: "gpt-5.6-terra".to_owned(),
        },
        ResolvedTier {
            defaulted: false,
            account: None,
            tier: "haiku",
            model: "gpt-5.4-mini".to_owned(),
        },
        ResolvedTier {
            defaulted: false,
            account: None,
            tier: "fable",
            model: "gpt-5.4-mini".to_owned(),
        },
    ]
}

struct Harness {
    path: std::path::PathBuf,
    /// The configuration file a persisted change is written to — inside the
    /// temp directory, never the operator's.
    config_file: std::path::PathBuf,
    store: Arc<FileStore>,
    /// The same policy the ingress routes turns from. Asserting on this is the
    /// difference between testing that a method echoes a value back and testing
    /// that it moved anything.
    policy: Arc<proxenos::policy::Policy>,
    /// The same store the ingress path writes a quota snapshot into.
    usage: Arc<proxenos::usage::UsageStore>,
    /// The same switches the ingress path would read. Asserting on these is
    /// the difference between testing that a flag round-trips and testing that
    /// the method does anything.
    switches: Arc<proxenos::recorder::Switches>,
    /// The configuration this harness's daemon started from. Held so a
    /// test can switch it off and assert that nothing is left behind.
    config: Arc<proxenos::config::Config>,
    /// The same signal the daemon's own run loop waits on, so a test can assert
    /// a stop actually moved something rather than only answering.
    shutdown: Arc<proxenos::daemon::Shutdown>,
    /// The same conversations the ingress serves, so a test can assert a
    /// switch reached them rather than only reached the store.
    sessions: Arc<proxenos::session::SessionStore>,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
        let switches = Arc::new(proxenos::recorder::Switches::default());
        // Bound to the credential store the way the daemon binds it, so a
        // figure recorded here is filed under whoever is selected — which is
        // what every per-account assertion below is about.
        let usage = Arc::new(proxenos::usage::UsageStore::for_accounts(
            Arc::clone(&store) as Arc<dyn AccountStore>,
        ));
        let policy = Arc::new(proxenos::policy::Policy::new(
            proxenos::policy::Snapshot::new(
                tiers(),
                None,
                proxenos::config::CrossAccountTiers::Refused,
            ),
        ));
        // The mapping matches this harness's catalog. A switch re-resolves the
        // mapping from configuration, so a default that names models the
        // catalog does not have would make every switch refuse itself.
        let config = Arc::new(proxenos::config::Config {
            tiers: proxenos::config::Tiers {
                opus: Some("gpt-5.6-terra".into()),
                sonnet: Some("gpt-5.6-terra".into()),
                haiku: Some("gpt-5.6-terra".into()),
                fable: Some("gpt-5.6-terra".into()),
            },
            ..proxenos::config::Config::default()
        });
        let shutdown = Arc::new(proxenos::daemon::Shutdown::default());
        let sessions = Arc::new(proxenos::session::SessionStore::new());

        let state = ControlState {
            port: 8787,
            policy: Arc::clone(&policy),
            catalog: Arc::new(CatalogSource::fixed(
                Catalog::parse(
                    r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000},
                                {"id":"gpt-5.4-mini","context_window":200000}]}"#,
                    95.0,
                )
                .unwrap(),
            )),
            credentials: Arc::clone(&store) as Arc<dyn AccountStore>,
            capture: Arc::clone(&switches),
            usage: Arc::clone(&usage),
            login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
            config: Arc::clone(&config),
            shutdown: Arc::clone(&shutdown),
            // No credentials to ask with, and no endpoint that would answer:
            // no test may reach the network.
            tokens: None,
            usage_endpoint: String::new(),
            // Inside the temp directory, always. A test that could reach an
            // operator's real configuration would be a test that edits the
            // machine it runs on.
            sessions: Arc::clone(&sessions),
            config_path: Some(dir.path().join("config.toml")),
        };

        let socket = path.clone();
        tokio::spawn(async move {
            let _ = control::serve(&socket, state).await;
        });

        // Wait for the socket to appear rather than sleeping a fixed interval.
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        Self {
            path,
            config_file: dir.path().join("config.toml"),
            store,
            policy,
            switches,
            usage,
            config,
            shutdown,
            sessions,
            _dir: dir,
        }
    }

    /// The same harness, answering on a socket whose daemon holds this grant.
    async fn with_tokens(self, tokens: Arc<proxenos::auth::tokens::TokenSource>) -> Self {
        let catalog = r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000}]}"#;
        self.respawn_with(catalog, "gpt-5.6-terra", Some(tokens))
            .await
    }

    /// The same harness, answering on a socket whose daemon writes to another
    /// configuration path — for the tests about what a failed write does.
    async fn with_config(self, config_file: std::path::PathBuf) -> Self {
        let harness = Self {
            config_file,
            ..self
        };
        let catalog = r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000},
                                  {"id":"gpt-5.4-mini","context_window":200000}]}"#;
        harness.respawn(catalog, "gpt-5.6-terra").await
    }

    /// The same harness, publishing the caller's client policy — for the tests
    /// about what switching it off leaves behind.
    async fn with_client(self, client: proxenos::config::ClientConfig) -> Self {
        let harness = Self {
            config: Arc::new(proxenos::config::Config {
                client,
                ..proxenos::config::Config::default()
            }),
            ..self
        };
        let catalog = r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000},
                                  {"id":"gpt-5.4-mini","context_window":200000}]}"#;
        harness.respawn(catalog, "gpt-5.4-mini").await
    }

    /// The same harness, whose daemon started from this configuration — for
    /// the tests about a mapping that belongs to one account.
    async fn with_configuration(self, config: proxenos::config::Config) -> Self {
        let harness = Self {
            config: Arc::new(config),
            ..self
        };
        let catalog = r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000},
                                  {"id":"gpt-5.4-mini","context_window":200000}]}"#;
        harness.respawn(catalog, "gpt-5.6-terra").await
    }

    /// The same harness, whose catalog was fetched for the named account.
    async fn with_catalog_for(self, account: &str) -> Self {
        let catalog = r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000}]}"#;
        let catalog = Catalog::parse(catalog, 95.0)
            .unwrap()
            .fetched_for(account.to_owned());
        let path = self._dir.path().join("control-3.sock");
        let policy = Arc::clone(&self.policy);
        let state = ControlState {
            port: 8787,
            policy: Arc::clone(&policy),
            catalog: Arc::new(CatalogSource::fixed(catalog)),
            credentials: Arc::clone(&self.store) as Arc<dyn AccountStore>,
            capture: Arc::clone(&self.switches),
            usage: Arc::clone(&self.usage),
            login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
            config: Arc::clone(&self.config),
            shutdown: Arc::clone(&self.shutdown),
            tokens: None,
            usage_endpoint: String::new(),
            sessions: Arc::clone(&self.sessions),
            config_path: Some(self.config_file.clone()),
        };
        let socket = path.clone();
        tokio::spawn(async move {
            let _ = control::serve(&socket, state).await;
        });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Self { path, ..self }
    }

    /// The same harness, whose catalog comes from a real endpoint and can be
    /// fetched again. The daemon starts holding what it fetched for the
    /// account selected now, exactly as `run` does.
    async fn with_catalog_source(self, endpoint: &str) -> Self {
        let tokens = Arc::new(proxenos::auth::tokens::TokenSource::new(
            Arc::clone(&self.store) as Arc<dyn CredentialStore>,
            String::new(),
            "client-abc",
            Arc::new(proxenos::auth::tokens::SystemClock),
        ));
        let catalog = Arc::new(CatalogSource::new(
            Catalog::fallback(),
            endpoint.to_owned(),
            String::new(),
            "0.0.0",
            95.0,
        ));
        let authorization = proxenos::auth::authorize::AccountAuthorizer::new(
            Arc::clone(&self.store) as Arc<dyn AccountStore>,
            Arc::clone(&tokens),
        )
        .authorize(None)
        .await
        .expect("a stored grant");
        catalog.refresh(&authorization).await;

        let path = self._dir.path().join("control-4.sock");
        let state = ControlState {
            port: 8787,
            policy: Arc::clone(&self.policy),
            catalog,
            credentials: Arc::clone(&self.store) as Arc<dyn AccountStore>,
            capture: Arc::clone(&self.switches),
            usage: Arc::clone(&self.usage),
            login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
            config: Arc::clone(&self.config),
            shutdown: Arc::clone(&self.shutdown),
            tokens: Some(tokens),
            usage_endpoint: String::new(),
            sessions: Arc::clone(&self.sessions),
            config_path: Some(self.config_file.clone()),
        };
        let socket = path.clone();
        tokio::spawn(async move {
            let _ = control::serve(&socket, state).await;
        });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Self { path, ..self }
    }

    /// A harness whose catalog and single mapped model are the caller's, for
    /// tests about what a particular window produces.
    async fn with_catalog(catalog: &str, model: &str) -> Self {
        let harness = Self::start().await;
        harness.respawn(catalog, model).await
    }

    async fn respawn(self, catalog: &str, model: &str) -> Self {
        self.respawn_with(catalog, model, None).await
    }

    async fn respawn_with(
        self,
        catalog: &str,
        model: &str,
        tokens: Option<Arc<proxenos::auth::tokens::TokenSource>>,
    ) -> Self {
        let tiers: Vec<ResolvedTier> = ["opus", "sonnet", "haiku", "fable"]
            .into_iter()
            .map(|tier| ResolvedTier {
                defaulted: false,
                account: None,
                tier,
                model: model.to_owned(),
            })
            .collect();

        let path = self._dir.path().join("control-2.sock");
        // Seeded from the configuration this daemon "started from", exactly
        // as startup seeds the real one.
        let policy = Arc::new(proxenos::policy::Policy::new(
            proxenos::policy::Snapshot::new(tiers, None, self.config.cross_account_policy()),
        ));
        let state = ControlState {
            port: 8787,
            policy: Arc::clone(&policy),
            catalog: Arc::new(CatalogSource::fixed(Catalog::parse(catalog, 95.0).unwrap())),
            credentials: Arc::clone(&self.store) as Arc<dyn AccountStore>,
            capture: Arc::clone(&self.switches),
            usage: Arc::clone(&self.usage),
            login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
            config: Arc::clone(&self.config),
            shutdown: Arc::clone(&self.shutdown),
            tokens,
            usage_endpoint: String::new(),
            sessions: Arc::clone(&self.sessions),
            config_path: Some(self.config_file.clone()),
        };

        let socket = path.clone();
        tokio::spawn(async move {
            let _ = control::serve(&socket, state).await;
        });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        Self {
            path,
            policy,
            ..self
        }
    }

    async fn call(&self, method: &str) -> Result<Value, proxenos::error::ProxyError> {
        control::call(&self.path, method, None).await
    }

    /// A call with parameters, whose error is reduced to its message — every
    /// assertion here is about what the refusal says, not about its code.
    async fn call_with(&self, method: &str, params: Value) -> Result<Value, String> {
        control::call(&self.path, method, Some(params))
            .await
            .map_err(|error| error.message)
    }
}

/// Every documented method answers over the socket. A method in the vocabulary
/// that the daemon does not know is a contract this project has already
/// published and cannot honour.
#[tokio::test]
async fn every_documented_method_is_answered() {
    let harness = Harness::start().await;

    for method in METHODS {
        // `login` really starts a flow, and the flow binds the one fixed
        // callback port. Calling it here would contend with the test that
        // covers it properly — a scheduling failure wearing a behaviour
        // failure's clothes, and one that only appears when the machine is
        // busy enough to overlap them. Its vocabulary is established there.
        if method == "login" {
            continue;
        }
        let result = harness.call(method).await;
        match result {
            Ok(_) => {}
            Err(error) => assert!(
                !error.message.contains("unknown method"),
                "`{method}` is documented but the daemon does not know it"
            ),
        }
    }
}

#[tokio::test]
async fn an_unknown_method_is_refused_by_name() {
    let harness = Harness::start().await;

    let error = harness
        .call("definitely.not.a.method")
        .await
        .expect_err("an unknown method should fail");

    assert!(
        error.message.contains("definitely.not.a.method"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn status_reports_the_base_url_and_tiers() {
    let harness = Harness::start().await;

    let status = harness.call("status").await.unwrap();

    assert_eq!(status["base_url"], json!("http://127.0.0.1:8787"));
    assert_eq!(status["auth"]["connected"], json!(false));
    assert_eq!(status["tiers"]["haiku"], json!("gpt-5.4-mini"));
    // Whether the mapping was validated against a real catalog or merely
    // against the fallback list. A caller that cannot tell would report an
    // unvalidated mapping as a validated one.
    assert_eq!(status["catalog_authoritative"], json!(true));
}

#[tokio::test]
async fn status_reflects_stored_credentials() {
    let harness = Harness::start().await;
    harness
        .store
        .save(&Credentials {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: None,
            account_id: Some("acct_9".to_owned()),
            expires_at: Some(9_999_999_999),
        })
        .unwrap();

    let status = harness.call("status").await.unwrap();

    assert_eq!(status["auth"]["connected"], json!(true));
    assert_eq!(status["auth"]["account_id"], json!("acct_9"));
}

/// The plan and the identity behind the grant are reported, because they are
/// the local half of an explanation the backend never gives.
///
/// A refusal names the value it rejected — an effort, a model — and not the
/// entitlement that was missing. Knowing the plan is what turns that into a
/// checkable fact instead of a guess.
#[tokio::test]
async fn status_reports_the_plan_and_identity_behind_the_grant() {
    let harness = Harness::start().await;
    harness
        .store
        .save(&Credentials {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: Some(id_token(json!({
                "email": "someone@example.com",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct_9",
                    "chatgpt_plan_type": "plus",
                },
            }))),
            account_id: Some("acct_9".to_owned()),
            expires_at: Some(9_999_999_999),
        })
        .unwrap();

    let status = harness.call("status").await.unwrap();

    assert_eq!(status["auth"]["plan"], json!("plus"));
    assert_eq!(status["auth"]["email"], json!("someone@example.com"));
    assert_eq!(status["auth"]["expires_at"], json!(9_999_999_999u64));
}

/// A grant whose token says nothing claims nothing. An absent plan is absent,
/// never defaulted — a guessed "free" would explain away a refusal that has
/// some other cause, and a guessed "plus" would deny one that is real.
#[tokio::test]
async fn status_claims_no_plan_it_was_not_told() {
    let harness = Harness::start().await;
    harness
        .store
        .save(&Credentials {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: None,
            account_id: Some("acct_9".to_owned()),
            expires_at: None,
        })
        .unwrap();

    let status = harness.call("status").await.unwrap();

    assert_eq!(status["auth"]["connected"], json!(true));
    assert!(status["auth"]["plan"].is_null());
    assert!(status["auth"]["email"].is_null());
}

/// A tier mapped onto a model the catalog withholds is named in `status`.
///
/// It passed validation — the catalog knows the id — so nothing else in the
/// system would ever mention that the model is not among the ones on offer.
#[tokio::test]
async fn status_names_a_tier_mapped_onto_a_withheld_model() {
    let harness = Harness::start().await;

    let status = harness.call("status").await.unwrap();

    // The harness maps nothing hidden, so there is nothing to report — and the
    // field is present and empty rather than absent, so a caller can tell
    // "nothing withheld" from "this daemon does not report it".
    assert_eq!(status["unlisted_tiers"], json!([]));
}

/// `accounts.forget` clears credentials, and is safe to run twice.
#[tokio::test]
async fn forgetting_clears_credentials() {
    let harness = Harness::start().await;
    harness
        .store
        .save(&Credentials {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: None,
            account_id: None,
            expires_at: None,
        })
        .unwrap();

    harness.call("accounts.forget").await.unwrap();
    assert!(harness.store.load().unwrap().is_none());

    harness.call("accounts.forget").await.unwrap();
}

/// §2.1 — all four tier variables, plus the context floor. `WebFetch` runs on
/// the haiku tier, so an unmapped haiku breaks it in a way that looks unrelated
/// to tier mapping.
#[tokio::test]
async fn env_emits_all_four_tiers_and_the_context_floor() {
    let harness = Harness::start().await;

    let result = harness.call("env").await.unwrap();
    let variables: Vec<(String, String)> = result["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry[0].as_str().unwrap().to_owned(),
                entry[1].as_str().unwrap().to_owned(),
            )
        })
        .collect();

    let names: Vec<&str> = variables.iter().map(|(name, _)| name.as_str()).collect();

    for required in [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "CLAUDE_CODE_DISABLE_1M_CONTEXT",
    ] {
        assert!(
            names.contains(&required),
            "{required} is missing from `env`"
        );
    }

    let lookup = |name: &str| {
        variables
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap()
    };

    assert_eq!(lookup("ANTHROPIC_BASE_URL"), "http://127.0.0.1:8787");
    assert_eq!(lookup("ANTHROPIC_DEFAULT_HAIKU_MODEL"), "gpt-5.4-mini");
    assert_eq!(lookup("CLAUDE_CODE_DISABLE_1M_CONTEXT"), "1");
}

/// The token is required for the client's sake and its value is ignored, so it
/// must not look like a real one.
#[tokio::test]
async fn the_emitted_auth_token_is_visibly_a_placeholder() {
    let harness = Harness::start().await;
    let result = harness.call("env").await.unwrap();
    let rendered = result.to_string();

    assert!(rendered.contains("\"ANTHROPIC_AUTH_TOKEN\",\"unused\""));
}

#[tokio::test]
async fn models_lists_windows_and_says_where_they_came_from() {
    let harness = Harness::start().await;

    let result = harness.call("models").await.unwrap();

    assert_eq!(result["authoritative"], json!(true));

    // Looked up rather than indexed: the order is the catalog's own and says
    // nothing about the contract. Indexing made this test fail when two model
    // ids simply sorted differently.
    let terra = result["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == json!("gpt-5.6-terra"))
        .expect("the mapped model should be listed");

    assert_eq!(terra["context_window"], json!(272_000));
}

/// A model with no known window reports null, not a number. Any figure here
/// would be invented.
#[tokio::test]
async fn an_unknown_window_is_reported_as_null() {
    let dir = tempfile::tempdir().unwrap();
    let state = ControlState {
        port: 1,
        policy: Arc::new(proxenos::policy::Policy::new(
            proxenos::policy::Snapshot::new(
                tiers(),
                None,
                proxenos::config::CrossAccountTiers::Refused,
            ),
        )),
        catalog: Arc::new(CatalogSource::fixed(Catalog::fallback())),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(proxenos::recorder::Switches::default()),
        usage: Arc::new(proxenos::usage::UsageStore::default()),
        login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
        config: Arc::new(proxenos::config::Config::default()),
        shutdown: Arc::new(proxenos::daemon::Shutdown::default()),
        tokens: None,
        usage_endpoint: String::new(),
        sessions: Arc::new(proxenos::session::SessionStore::new()),
        config_path: None,
    };

    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "models" }).to_string(),
    )
    .await;
    let result = response.result.unwrap();

    assert_eq!(result["authoritative"], json!(false));
    assert_eq!(result["models"][0]["context_window"], Value::Null);
}

/// Quota that has never been reported is unknown, not zero. A zeroed window
/// reads as "no quota used" rather than "not yet known".
#[tokio::test]
async fn unseen_quota_reports_unknown_rather_than_zero() {
    let harness = Harness::start().await;

    let usage = harness.call("usage").await.unwrap();

    assert_eq!(usage["known"], json!(false));
    assert!(usage.get("used_percent").is_none());
}

/// `usage` names the models this daemon serves.
///
/// That is what lets a globally-configured status line tell a session running
/// through this proxy from one running against its own provider. Reported
/// whether or not a quota has been seen, because the question is about which
/// session is asking rather than about the answer.
#[tokio::test]
async fn usage_names_the_models_this_daemon_serves() {
    let harness = Harness::start().await;

    let usage = harness.call("usage").await.unwrap();
    let served: Vec<&str> = usage["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model.as_str().unwrap())
        .collect();

    assert!(served.contains(&"gpt-5.6-terra"));
    assert!(served.contains(&"gpt-5.4-mini"));
    // Each id once, however many tiers map to it.
    assert_eq!(served.len(), 2);
}

/// Starting a capture over the socket changes what the daemon captures.
///
/// Asserted on the switches the ingress path actually reads, not only on what
/// `status` reports back. A method that reports success and changes nothing is
/// the failure this project refuses everywhere else, and only the first of
/// those two assertions can catch it.
#[tokio::test]
async fn recording_can_be_started_and_stopped() {
    let harness = Harness::start().await;

    assert_eq!(
        harness.call("status").await.unwrap()["recording"],
        json!(false)
    );

    control::call(
        &harness.path,
        "record.start",
        Some(json!({ "mode": "ingress" })),
    )
    .await
    .unwrap();
    assert!(harness.switches.ingress(), "ingress capture should be on");
    assert!(
        !harness.switches.upstream(),
        "and the mode that spends quota should not have been started for it"
    );
    assert_eq!(
        harness.call("status").await.unwrap()["recording"],
        json!(true)
    );

    harness.call("record.stop").await.unwrap();
    assert!(!harness.switches.ingress());
    assert_eq!(
        harness.call("status").await.unwrap()["recording"],
        json!(false)
    );
}

/// The costly mode has to be named. It bills every turn that follows, so it is
/// never what an unqualified `record.start` means.
#[tokio::test]
async fn upstream_capture_must_be_asked_for_by_name() {
    let harness = Harness::start().await;

    control::call(&harness.path, "record.start", None)
        .await
        .unwrap();
    assert!(harness.switches.ingress());
    assert!(!harness.switches.upstream());

    control::call(
        &harness.path,
        "record.start",
        Some(json!({ "mode": "upstream" })),
    )
    .await
    .unwrap();
    assert!(harness.switches.upstream());
}

/// A mode nobody implements is refused rather than silently treated as the
/// default, which would start the wrong capture and report success.
#[tokio::test]
async fn an_unknown_capture_mode_is_refused() {
    let harness = Harness::start().await;

    let error = control::call(
        &harness.path,
        "record.start",
        Some(json!({ "mode": "sideways" })),
    )
    .await
    .expect_err("an unknown mode should be refused");

    assert!(error.message.contains("sideways"), "{}", error.message);
    assert!(!harness.switches.ingress());
    assert!(!harness.switches.upstream());
}

/// A malformed line does not take the connection down with it.
#[tokio::test]
async fn a_malformed_request_is_reported_without_closing_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let state = ControlState {
        port: 1,
        policy: Arc::new(proxenos::policy::Policy::new(
            proxenos::policy::Snapshot::new(
                tiers(),
                None,
                proxenos::config::CrossAccountTiers::Refused,
            ),
        )),
        catalog: Arc::new(CatalogSource::fixed(Catalog::fallback())),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(proxenos::recorder::Switches::default()),
        usage: Arc::new(proxenos::usage::UsageStore::default()),
        login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
        config: Arc::new(proxenos::config::Config::default()),
        shutdown: Arc::new(proxenos::daemon::Shutdown::default()),
        tokens: None,
        usage_endpoint: String::new(),
        sessions: Arc::new(proxenos::session::SessionStore::new()),
        config_path: None,
    };

    let response = control::answer(&state, "{ not json").await;
    assert_eq!(response.error.map(|error| error.code), Some(-32700));

    // The next request on the same connection still works.
    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "status" }).to_string(),
    )
    .await;
    assert!(response.result.is_some());
}

/// The socket can clear credentials, so the filesystem is its access control.
#[cfg(unix)]
#[tokio::test]
async fn the_socket_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let harness = Harness::start().await;
    let mode = std::fs::metadata(&harness.path)
        .unwrap()
        .permissions()
        .mode();

    assert_eq!(
        mode & 0o077,
        0,
        "the control socket must not be reachable by others"
    );
}

// ---------------------------------------------------------------------------
// Rendering. Presentation only — the daemon decides what is true.
// ---------------------------------------------------------------------------

use proxenos::render;

#[tokio::test]
async fn env_renders_as_shell_exports() {
    let harness = Harness::start().await;
    let result = harness.call("env").await.unwrap();

    let rendered = render::env_shell(&result);

    assert!(rendered.contains("export ANTHROPIC_BASE_URL=http://127.0.0.1:8787"));
    assert!(rendered.contains("export ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-5.4-mini"));
    assert!(rendered.contains("export CLAUDE_CODE_DISABLE_1M_CONTEXT=1"));
}

#[tokio::test]
async fn env_renders_as_a_settings_fragment() {
    let harness = Harness::start().await;
    let result = harness.call("env").await.unwrap();

    let parsed: Value = serde_json::from_str(&render::settings_json(&result)).unwrap();

    assert_eq!(
        parsed["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"],
        json!("gpt-5.4-mini")
    );
    assert_eq!(parsed["env"]["CLAUDE_CODE_DISABLE_1M_CONTEXT"], json!("1"));
}

/// The payload carries both halves of what a client needs, under names of their
/// own. A caller reading only `variables` is untouched by this, which is what
/// makes it safe to add underneath one that already exists.
#[tokio::test]
async fn the_env_payload_carries_the_client_policy_beside_the_variables() {
    let harness = Harness::start().await;
    let result = harness.call("env").await.unwrap();

    assert!(
        result["variables"].is_array(),
        "the existing half must keep its shape: {result}"
    );
    assert_eq!(
        result["settings"],
        json!({
            "permissions": { "deny": ["Skill(claude-api)"] },
            "disableClaudeAiConnectors": true,
        })
    );
}

/// The connector opt-out is the one piece of client policy with an environment
/// half. `disableClaudeAiConnectors` silences the client's notice; this
/// variable is the client's documented switch for the claude.ai-hosted servers
/// themselves. One configuration key, both renderings — an export-only launch
/// (`proxenos env`) otherwise runs with the connectors it asked to disable.
#[tokio::test]
async fn disabling_connectors_also_reaches_the_environment() {
    let harness = Harness::start().await;
    let result = harness.call("env").await.unwrap();

    let variables = render::variables(&result);
    assert!(
        variables
            .iter()
            .any(|(name, value)| name == "ENABLE_CLAUDEAI_MCP_SERVERS" && value == "false"),
        "the connector half of the policy has an environment variable and it is missing: {result}"
    );
}

/// One document, complete on its own.
///
/// Measured: a settings file's `env` key routes without help. A client started
/// with no `ANTHROPIC_*` in its environment, reading only this document, still
/// reached the proxy. So this rendering is not half a configuration waiting for
/// an `eval` — it is the whole thing, and it carries the policy an export
/// cannot.
#[tokio::test]
async fn the_settings_rendering_is_a_complete_configuration() {
    let harness = Harness::start().await;
    let result = harness.call("env").await.unwrap();

    let parsed: Value = serde_json::from_str(&render::settings_json(&result)).unwrap();

    assert_eq!(
        parsed["env"]["ANTHROPIC_BASE_URL"],
        json!("http://127.0.0.1:8787")
    );
    assert_eq!(
        parsed["permissions"]["deny"],
        json!(["Skill(claude-api)"]),
        "the policy half belongs in the same document as the routing half"
    );
    assert_eq!(parsed["disableClaudeAiConnectors"], json!(true));
}

/// Shell exports carry routing and say so.
///
/// A deny rule has no environment variable — checked against the whole settings
/// schema, there is none — so this rendering is incomplete by construction. The
/// comment is the only place a reader finds that out at the moment it matters,
/// and `eval` steps over it.
#[tokio::test]
async fn shell_exports_name_what_they_cannot_carry() {
    let harness = Harness::start().await;
    let result = harness.call("env").await.unwrap();

    let rendered = render::env_shell(&result);

    assert!(
        rendered.contains("export ANTHROPIC_BASE_URL=http://127.0.0.1:8787"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("Skill(claude-api)"),
        "a deny rule is not an environment variable: {rendered}"
    );
    assert!(
        rendered.lines().any(|line| line.starts_with('#')),
        "the gap has to be stated where it is discovered: {rendered}"
    );
    assert!(
        rendered.contains("settings"),
        "the comment has to name the rendering that does carry it: {rendered}"
    );
}

/// Switched off leaves nothing behind.
///
/// The absent key is the assertion. A document that always carries an empty
/// `permissions` block would look like a policy to whoever merges it, and
/// merging an empty deny list over a real one is how a rule disappears.
#[tokio::test]
async fn a_client_policy_switched_off_leaves_no_trace() {
    let harness = Harness::start()
        .await
        .with_client(proxenos::config::ClientConfig {
            deny_skills: Vec::new(),
            disable_connectors: false,
        })
        .await;
    let result = harness.call("env").await.unwrap();

    // Present and empty: see `the_policy_half_is_present_and_empty_rather_than_absent`
    // for why absence has to stay reserved for a daemon that predates this.
    assert_eq!(result["settings"], json!({}), "{result}");

    let parsed: Value = serde_json::from_str(&render::settings_json(&result)).unwrap();
    assert!(parsed["env"].is_object(), "{parsed}");
    assert!(parsed.get("permissions").is_none(), "{parsed}");
    assert!(
        parsed.get("disableClaudeAiConnectors").is_none(),
        "{parsed}"
    );

    let rendered = render::env_shell(&result);
    assert!(
        !rendered.lines().any(|line| line.starts_with('#')),
        "there is no gap left to warn about: {rendered}"
    );

    assert!(
        !render::variables(&result)
            .iter()
            .any(|(name, _)| name == "ENABLE_CLAUDEAI_MCP_SERVERS"),
        "a policy switched off must not leave its environment half behind: {result}"
    );
}

/// The client refuses a denied skill with "Skill execution blocked by
/// permission rules" and names nobody. This is where the person holding that
/// message finds out what blocked it and which key to change.
#[tokio::test]
async fn status_names_the_client_policy_and_the_key_that_sets_it() {
    let harness = Harness::start().await;
    let result = harness.call("status").await.unwrap();

    assert_eq!(result["client"]["deny_skills"], json!(["claude-api"]));
    assert_eq!(result["client"]["disable_connectors"], json!(true));

    let rendered = render::status(&result);
    assert!(
        rendered.contains("claude-api"),
        "the blocked skill has to be named: {rendered}"
    );
    assert!(
        rendered.contains("deny_skills"),
        "and so has the key that undoes it: {rendered}"
    );
}

/// Nothing denied, nothing said. A status line reporting an empty policy would
/// have the reader looking for a rule that is not there.
#[tokio::test]
async fn status_stays_quiet_when_nothing_is_denied() {
    let harness = Harness::start()
        .await
        .with_client(proxenos::config::ClientConfig {
            deny_skills: Vec::new(),
            disable_connectors: true,
        })
        .await;
    let result = harness.call("status").await.unwrap();

    let rendered = render::status(&result);
    assert!(
        !rendered.contains("deny_skills"),
        "there is no denial to attribute: {rendered}"
    );
}

/// The rendered status names the plan and the account, because that is the
/// surface a person reads when a turn was refused and they want to know why.
#[tokio::test]
async fn the_rendered_status_names_the_plan_and_the_account() {
    let harness = Harness::start().await;
    harness
        .store
        .save(&Credentials {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: Some(id_token(json!({
                "email": "someone@example.com",
                "https://api.openai.com/auth": { "chatgpt_plan_type": "free" },
            }))),
            account_id: Some("acct_9".to_owned()),
            expires_at: Some(9_999_999_999),
        })
        .unwrap();

    let rendered = render::status(&harness.call("status").await.unwrap());

    assert!(rendered.contains("free"), "{rendered}");
    assert!(rendered.contains("someone@example.com"), "{rendered}");
}

/// An unknown plan says so rather than printing a blank or a guess.
#[tokio::test]
async fn the_rendered_status_does_not_invent_a_plan() {
    let harness = Harness::start().await;
    harness
        .store
        .save(&Credentials {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: None,
            account_id: Some("acct_9".to_owned()),
            expires_at: None,
        })
        .unwrap();

    let rendered = render::status(&harness.call("status").await.unwrap());

    assert!(rendered.contains("acct_9"), "{rendered}");
    assert!(
        !rendered.contains("plan"),
        "a plan it was never told should not be printed at all: {rendered}"
    );
}

/// A tier pointing at a withheld model is said out loud, naming the model.
///
/// This is the case that starts cleanly and then behaves oddly: validation
/// passed, so nothing refused it, but the model is not among the ones offered.
#[test]
fn the_rendered_status_names_a_withheld_model() {
    let rendered = render::status(&json!({
        "base_url": "http://127.0.0.1:8787",
        "auth": { "connected": true, "account_id": "acct_9" },
        "tiers": { "opus": "internal-preview" },
        "catalog_authoritative": true,
        "unlisted_tiers": ["internal-preview"],
    }));

    assert!(rendered.contains("internal-preview"), "{rendered}");
    assert!(
        rendered.to_lowercase().contains("not offered")
            || rendered.to_lowercase().contains("withheld"),
        "{rendered}"
    );
}

/// Nothing withheld prints no warning at all.
#[test]
fn the_rendered_status_is_quiet_when_nothing_is_withheld() {
    let rendered = render::status(&json!({
        "base_url": "http://127.0.0.1:8787",
        "auth": { "connected": false },
        "tiers": { "opus": "gpt-5.6-terra" },
        "catalog_authoritative": true,
        "unlisted_tiers": [],
    }));

    assert!(!rendered.to_lowercase().contains("withheld"), "{rendered}");
    assert!(
        !rendered.to_lowercase().contains("not offered"),
        "{rendered}"
    );
}

/// A reader must be able to tell an unvalidated mapping from a validated one.
#[tokio::test]
async fn status_says_when_the_catalog_was_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let state = ControlState {
        port: 8787,
        policy: Arc::new(proxenos::policy::Policy::new(
            proxenos::policy::Snapshot::new(
                tiers(),
                None,
                proxenos::config::CrossAccountTiers::Refused,
            ),
        )),
        catalog: Arc::new(CatalogSource::fixed(Catalog::fallback())),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(proxenos::recorder::Switches::default()),
        usage: Arc::new(proxenos::usage::UsageStore::default()),
        login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
        config: Arc::new(proxenos::config::Config::default()),
        shutdown: Arc::new(proxenos::daemon::Shutdown::default()),
        tokens: None,
        usage_endpoint: String::new(),
        sessions: Arc::new(proxenos::session::SessionStore::new()),
        config_path: None,
    };

    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "status" }).to_string(),
    )
    .await;
    let rendered = render::status(&response.result.unwrap());

    assert!(rendered.contains("has not been validated"), "{rendered}");
}

/// An unknown window prints as unknown. Printing a figure nobody measured is
/// how an assumption becomes a fact.
#[tokio::test]
async fn models_prints_unknown_rather_than_a_number() {
    let dir = tempfile::tempdir().unwrap();
    let state = ControlState {
        port: 1,
        policy: Arc::new(proxenos::policy::Policy::new(
            proxenos::policy::Snapshot::new(
                tiers(),
                None,
                proxenos::config::CrossAccountTiers::Refused,
            ),
        )),
        catalog: Arc::new(CatalogSource::fixed(Catalog::fallback())),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(proxenos::recorder::Switches::default()),
        usage: Arc::new(proxenos::usage::UsageStore::default()),
        login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
        config: Arc::new(proxenos::config::Config::default()),
        shutdown: Arc::new(proxenos::daemon::Shutdown::default()),
        tokens: None,
        usage_endpoint: String::new(),
        sessions: Arc::new(proxenos::session::SessionStore::new()),
        config_path: None,
    };

    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "models" }).to_string(),
    )
    .await;
    let rendered = render::models(&response.result.unwrap());

    assert!(rendered.contains("window unknown"), "{rendered}");
    assert!(rendered.contains("fallback list"), "{rendered}");
}

/// Not connected reads as an instruction, not as a state. Someone running
/// `status` for the first time needs to know what to do next.
#[tokio::test]
async fn status_tells_an_unauthenticated_user_what_to_do() {
    let harness = Harness::start().await;
    let rendered = render::status(&harness.call("status").await.unwrap());

    assert!(rendered.contains("login"), "{rendered}");
}

/// §7.2 — `env` states the real window when the catalog knows it.
///
/// The client cannot recognize these model ids, so it assumes 200,000 and says
/// so in a warning. That assumption is safe but wrong: a session compacts with
/// a quarter of its context unused. Stating the measured figure replaces a
/// guess with a fact.
#[tokio::test]
async fn env_states_the_real_context_window() {
    let harness = Harness::start().await;
    let result = harness.call("env").await.unwrap();
    let rendered = render::env_shell(&result);

    // The tiers here map to two models, 272000 and 200000. One variable covers
    // all four tiers, so the smallest wins — it is the only one that cannot
    // overrun. And the effective window rather than the raw one: 200000 × 95%.
    assert!(
        rendered.contains("export CLAUDE_CODE_MAX_CONTEXT_TOKENS=190000"),
        "{rendered}"
    );

    // Stating the window without also setting where to compact is worse than
    // saying nothing: the client drops its own 200,000 assumption and, not
    // recognizing the model, then enforces no limit at all.
    assert!(
        rendered.contains("export CLAUDE_CODE_AUTO_COMPACT_WINDOW=190000"),
        "{rendered}"
    );
}

/// With no catalog there is no window to state, and none is invented. A guessed
/// figure here would make the client compact against a number nobody measured.
#[tokio::test]
async fn env_states_no_window_when_the_catalog_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let state = ControlState {
        port: 8787,
        policy: Arc::new(proxenos::policy::Policy::new(
            proxenos::policy::Snapshot::new(
                tiers(),
                None,
                proxenos::config::CrossAccountTiers::Refused,
            ),
        )),
        catalog: Arc::new(CatalogSource::fixed(Catalog::fallback())),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(proxenos::recorder::Switches::default()),
        usage: Arc::new(proxenos::usage::UsageStore::default()),
        login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
        config: Arc::new(proxenos::config::Config::default()),
        shutdown: Arc::new(proxenos::daemon::Shutdown::default()),
        tokens: None,
        usage_endpoint: String::new(),
        sessions: Arc::new(proxenos::session::SessionStore::new()),
        config_path: None,
    };

    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "env" }).to_string(),
    )
    .await;
    let rendered = render::env_shell(&response.result.unwrap());

    assert!(
        !rendered.contains("CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
        "{rendered}"
    );
    // The one-sided floor is still set.
    assert!(rendered.contains("CLAUDE_CODE_DISABLE_1M_CONTEXT=1"));
}

/// The plan the backend reported on the last turn wins over the one in the
/// grant.
///
/// Two sources say what plan this account is on. The id token says what it was
/// when the operator last authenticated; the backend says what it is now, on
/// every turn, in the snapshot it opens each stream with. Preferring the token
/// would report a stale plan indefinitely after an upgrade — and the plan is
/// read precisely to explain refusals that turn on entitlement.
#[tokio::test]
async fn a_plan_the_backend_reported_wins_over_the_one_in_the_grant() {
    let harness = Harness::start().await;
    harness
        .store
        .save(&Credentials {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: Some(id_token(json!({
                "https://api.openai.com/auth": { "chatgpt_plan_type": "free" },
            }))),
            account_id: Some("acct_9".to_owned()),
            expires_at: None,
        })
        .unwrap();

    let snapshot = proxenos::usage::Snapshot::parse(
        &json!({
            "type": "codex.rate_limits",
            "plan_type": "plus",
            "rate_limits": { "limit_reached": false },
        })
        .to_string(),
    )
    .expect("a rate-limit event");
    harness.usage.record(&snapshot);

    let status = harness.call("status").await.unwrap();

    assert_eq!(status["auth"]["plan"], json!("plus"));
}

/// With no turn yet made there is nothing more current, so the grant's claim
/// stands — labelled as what it is rather than dropped.
#[tokio::test]
async fn the_grants_plan_is_used_until_the_backend_has_said_otherwise() {
    let harness = Harness::start().await;
    harness
        .store
        .save(&Credentials {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: Some(id_token(json!({
                "https://api.openai.com/auth": { "chatgpt_plan_type": "free" },
            }))),
            account_id: Some("acct_9".to_owned()),
            expires_at: None,
        })
        .unwrap();

    let status = harness.call("status").await.unwrap();

    assert_eq!(status["auth"]["plan"], json!("free"));
    assert_eq!(status["auth"]["plan_source"], json!("grant"));
}

// ---------------------------------------------------------------------------
// The compaction window has a range the client will accept, and a value outside
// it is not an error the operator ever sees.
// ---------------------------------------------------------------------------

/// A window the client cannot parse is not emitted at all.
///
/// The client accepts `CLAUDE_CODE_AUTO_COMPACT_WINDOW` only between 100,000
/// and 1,000,000 — its own parser says "Expected 'auto' or 100k–1M tokens", and
/// the equivalent settings key is declared `.min(1e5).max(1e6).catch(void 0)`,
/// which **silently discards** anything outside that. Emitting 81,600 therefore
/// does not compact early; it does nothing, and nothing says so.
///
/// Omitting it is not a fix — the client falls back to a window larger than the
/// model has — so this is reported loudly rather than papered over. What it must
/// not do is emit a number that is quietly thrown away.
#[tokio::test]
async fn a_window_below_what_the_client_accepts_is_not_emitted() {
    let harness = Harness::with_catalog(
        r#"{"data":[{"id":"tiny","context_window":80000,
                     "effective_context_window_percent":100.0}]}"#,
        "tiny",
    )
    .await;

    let variables = harness.call("env").await.unwrap();
    let rendered = render::env_shell(&variables);

    assert!(
        !rendered.contains("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
        "a window the client discards should not be emitted: {rendered}"
    );
}

/// A window inside the range is emitted as before.
#[tokio::test]
async fn a_window_the_client_accepts_is_emitted() {
    let harness = Harness::with_catalog(
        r#"{"data":[{"id":"roomy","context_window":272000,
                     "effective_context_window_percent":95.0}]}"#,
        "roomy",
    )
    .await;

    let rendered = render::env_shell(&harness.call("env").await.unwrap());

    assert!(
        rendered.contains("CLAUDE_CODE_AUTO_COMPACT_WINDOW=258400"),
        "{rendered}"
    );
}

/// Above the range is discarded for the same reason, in the other direction.
#[tokio::test]
async fn a_window_above_what_the_client_accepts_is_not_emitted() {
    let harness = Harness::with_catalog(
        r#"{"data":[{"id":"huge","context_window":2000000,
                     "effective_context_window_percent":100.0}]}"#,
        "huge",
    )
    .await;

    let rendered = render::env_shell(&harness.call("env").await.unwrap());

    assert!(
        !rendered.contains("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
        "{rendered}"
    );
}

/// A tier mapping can be set on a running daemon, and it moves what routes turns.
///
/// Asserted on the policy the ingress reads, not only on what `tiers.get`
/// echoes back. A method that reported a new mapping while turns kept going to
/// the old model would be the exact failure this project refuses everywhere
/// else — and only the first of these two assertions can catch it.
#[tokio::test]
async fn setting_the_tier_mapping_moves_what_routes_turns() {
    let harness = Harness::start().await;

    let result = harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "sonnet": "gpt-5.4-mini" } }),
        )
        .await
        .unwrap();

    assert_eq!(result["tiers"]["sonnet"], json!("gpt-5.4-mini"));
    // Untouched tiers keep what they had: a partial set is a change to the
    // tiers named, never a replacement of the whole mapping.
    assert_eq!(result["tiers"]["opus"], json!("gpt-5.6-terra"));

    let routed = harness.policy.get();
    assert_eq!(
        routed
            .models()
            .iter()
            .find(|mapping| mapping.requested == "sonnet")
            .map(|mapping| mapping.upstream.as_str()),
        Some("gpt-5.4-mini")
    );
}

/// A model the catalog does not have is refused, and the refusal names what it
/// does have.
///
/// This is the whole reason the daemon owns the mapping rather than a
/// front-end: it is the side holding the catalog. A set that skipped this check
/// would let a caller point a tier at a model the backend will not serve, and
/// the failure would arrive one turn later, as a 400 the client cannot fix.
#[tokio::test]
async fn setting_a_tier_to_a_model_the_catalog_lacks_is_refused() {
    let harness = Harness::start().await;

    let error = harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "sonnet": "gpt-9-imaginary" } }),
        )
        .await
        .unwrap_err();

    assert!(error.contains("gpt-9-imaginary"), "{error}");
    // And nothing moved.
    assert_eq!(
        harness
            .policy
            .get()
            .models()
            .iter()
            .find(|mapping| mapping.requested == "sonnet")
            .map(|mapping| mapping.upstream.as_str()),
        Some("gpt-5.6-terra")
    );
}

/// An unknown tier name is refused rather than quietly added.
#[tokio::test]
async fn setting_an_unknown_tier_name_is_refused() {
    let harness = Harness::start().await;

    let error = harness
        .call_with("tiers.set", json!({ "tiers": { "hyper": "gpt-5.4-mini" } }))
        .await
        .unwrap_err();

    assert!(error.contains("hyper"), "{error}");
}

/// The effort ceiling can be raised on a running daemon.
///
/// It caps every turn regardless of what the client asked for, so a ceiling set
/// once at startup silently downgrades every request a front-end makes after —
/// and nothing about that failure is visible: the turns succeed, they are just
/// shallower than they were asked to be.
#[tokio::test]
async fn the_effort_ceiling_can_be_raised_without_a_restart() {
    let harness = Harness::start().await;

    let result = harness
        .call_with("effort.set", json!({ "effort": "high" }))
        .await
        .unwrap();

    assert_eq!(result["effort"], json!("high"));
    assert_eq!(
        harness.policy.get().effort_ceiling(),
        Some(proxenos_core::responses::Effort::High)
    );
    // And `status` says so. A capped turn succeeds, so without this nothing
    // anywhere would ever mention that every request is being capped.
    assert_eq!(
        harness.call("status").await.unwrap()["effort_ceiling"],
        json!("high")
    );
}

/// And removed entirely, which is not the same as setting it to the highest
/// value the catalog happens to list.
#[tokio::test]
async fn the_effort_ceiling_can_be_removed() {
    let harness = Harness::start().await;

    harness
        .call_with("effort.set", json!({ "effort": "high" }))
        .await
        .unwrap();
    let result = harness
        .call_with("effort.set", json!({ "effort": null }))
        .await
        .unwrap();

    assert_eq!(result["effort"], Value::Null);
    assert_eq!(harness.policy.get().effort_ceiling(), None);
    // Null, not the highest value the catalog lists: with no ceiling the only
    // cap left is the model's own.
    assert_eq!(
        harness.call("status").await.unwrap()["effort_ceiling"],
        Value::Null
    );
}

/// The login flow, end to end short of the browser.
///
/// One test rather than three, because every assertion here needs the one fixed
/// callback port and the suite runs tests concurrently — three tests would
/// contend for it and fail on scheduling rather than on behaviour.
///
/// The discriminating assertions are the ones that could pass only if something
/// was really bound: a method that returned a URL and armed nothing would look
/// identical to its caller right up to the moment the browser redirected into
/// nothing.
#[tokio::test]
async fn login_arms_a_callback_joins_a_second_caller_and_releases_on_cancel() {
    let harness = Harness::start().await;

    let first = harness.call("login").await.unwrap();
    let url = first["authorization_url"].as_str().unwrap().to_owned();

    assert!(url.starts_with("https://"), "{url}");
    assert!(url.contains("code_challenge"), "{url}");
    assert_eq!(first["already_in_flight"], json!(false));

    // The redirect target is a fixed port, and something has to be listening on
    // it before the operator's browser arrives.
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", 1455))
            .await
            .is_ok(),
        "the callback port should be listening once login has started"
    );

    // A second caller joins the first. Beginning again would either fail to
    // bind or replace the state the first flow is waiting to match, leaving the
    // operator holding a URL whose callback is guaranteed to be rejected.
    let second = harness
        .call_with("login", json!({ "label": "spare" }))
        .await
        .unwrap();
    assert_eq!(second["authorization_url"], json!(url));
    assert_eq!(second["already_in_flight"], json!(true));
    // The joined flow keeps the name it was started with, and the answer says
    // so. A caller told only that it joined would go looking for an account
    // called `spare` that was never going to exist.
    assert_eq!(
        second["label"],
        Value::Null,
        "the flow it joined carries no label, and this call's is not adopted"
    );

    harness.call("login.cancel").await.unwrap();

    // Bindable again means genuinely released. A flow that merely forgot its
    // state would leave the listener holding the port.
    let rebound = tokio::net::TcpListener::bind(("127.0.0.1", 1455)).await;
    assert!(rebound.is_ok(), "the callback port should be free again");
    drop(rebound);

    let again = harness
        .call_with("login", json!({ "label": "spare" }))
        .await
        .unwrap();
    assert_eq!(again["already_in_flight"], json!(false));
    assert_eq!(
        again["label"],
        json!("spare"),
        "a flow this call started carries the name it asked for"
    );
    assert_ne!(
        again["authorization_url"],
        json!(url),
        "a fresh login is a fresh flow, not the cancelled one"
    );
    harness.call("login.cancel").await.unwrap();
}

/// `tiers.set` takes the same two forms the file does: a model id, or
/// `{ account, model }` pinning the tier to another account. The pinned form is
/// the write-time half of the consent gate — the roadmap's rule refuses it at
/// the daemon's start AND here, so a front-end cannot write what a restart will
/// then refuse to load.
#[tokio::test]
async fn a_cross_account_tier_set_without_consent_is_refused() {
    let harness = Harness::start().await;

    let error = harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "haiku": { "account": "spare", "model": "gpt-5.4-mini" } } }),
        )
        .await
        .unwrap_err();

    assert!(
        error.contains("cross_account_tiers"),
        "the refusal names the consent key: {error}"
    );
    // And nothing moved.
    assert_eq!(
        harness
            .policy
            .get()
            .tiers()
            .iter()
            .find(|tier| tier.tier == "haiku")
            .and_then(|tier| tier.account.clone()),
        None
    );
}

/// With consent, a pinned tier moves routing and is reported with its pin. The
/// pinned model is deliberately one this harness's catalog does not have: a
/// catalog is one account's menu, so it cannot speak for the account the pin
/// names, and validating against it would refuse the exact case pins exist for.
#[tokio::test]
async fn a_cross_account_tier_set_with_consent_pins_the_tier() {
    let harness = Harness::start()
        .await
        .with_configuration(proxenos::config::Config {
            cross_account_tiers: true,
            tiers: proxenos::config::Tiers {
                opus: Some("gpt-5.6-terra".into()),
                sonnet: Some("gpt-5.6-terra".into()),
                haiku: Some("gpt-5.6-terra".into()),
                fable: Some("gpt-5.6-terra".into()),
            },
            ..proxenos::config::Config::default()
        })
        .await;

    let result = harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "haiku": { "account": "spare", "model": "claude-haiku-4-5" } } }),
        )
        .await
        .unwrap();

    assert_eq!(
        result["tiers"]["haiku"],
        json!({ "account": "spare", "model": "claude-haiku-4-5" }),
        "a pinned tier is reported with its pin"
    );
    assert_eq!(
        result["tiers"]["opus"],
        json!("gpt-5.6-terra"),
        "a bare tier keeps its shape"
    );

    let routed = harness.policy.get();
    let haiku = routed
        .tiers()
        .iter()
        .find(|tier| tier.tier == "haiku")
        .unwrap()
        .clone();
    assert_eq!(haiku.model, "claude-haiku-4-5");
    assert_eq!(haiku.account.as_deref(), Some("spare"));

    // The status rendering names the pin — a pinned tier printed as its model
    // alone would hide which account it spends.
    let status = harness.call("status").await.unwrap();
    let rendered = render::status(&status);
    assert!(
        rendered.contains("claude-haiku-4-5 (as spare)"),
        "{rendered}"
    );
}

/// Consent is granted over the socket and takes effect without a restart —
/// the roadmap's rule: a persisted configuration key, written through the
/// control socket so both the CLI and a front-end can set it deliberately.
/// Always persisted, because a consent that evaporated at the next restart
/// would leave the file refusing a mapping the operator explicitly permitted.
#[tokio::test]
async fn consent_granted_over_the_socket_takes_effect_without_a_restart() {
    let harness = Harness::start().await;

    let pinned = json!({ "tiers": { "haiku": { "account": "spare", "model": "m" } } });
    let refused = harness.call_with("tiers.set", pinned.clone()).await;
    assert!(refused.is_err(), "consent has not been granted yet");

    let result = harness
        .call_with("cross_account_tiers.set", json!({ "enabled": true }))
        .await
        .unwrap();
    assert_eq!(result["cross_account_tiers"], json!(true));
    assert_eq!(result["persisted"], json!(true));

    let written = std::fs::read_to_string(&harness.config_file).unwrap();
    assert!(
        written.contains("\ncross_account_tiers = true"),
        "the consent must be durable: {written}"
    );

    harness
        .call_with("tiers.set", pinned)
        .await
        .expect("the granted consent applies to the next call, not the next restart");
}

/// Revoking consent while a pin is in force is refused naming the pin. The
/// alternative writes a file the daemon will refuse to start from — a refusal
/// the operator only meets at the next restart, with no way back but an edit.
#[tokio::test]
async fn consent_cannot_be_revoked_while_a_pin_is_in_force() {
    let harness = Harness::start().await;
    harness
        .call_with("cross_account_tiers.set", json!({ "enabled": true }))
        .await
        .unwrap();

    harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "haiku": { "account": "spare", "model": "m" } } }),
        )
        .await
        .unwrap();

    let error = harness
        .call_with("cross_account_tiers.set", json!({ "enabled": false }))
        .await
        .unwrap_err();
    assert!(
        error.contains("haiku"),
        "the refusal names the pin still in force: {error}"
    );
}

/// A change asks to be persisted; it is never persisted by default.
///
/// A front-end changing a mapping to try something is not the same as an
/// operator changing what this daemon is, and only the caller knows which it is
/// doing. Asserted on the file, because "persisted: false" in the reply is what
/// a method that silently wrote would also say.
#[tokio::test]
async fn a_change_is_not_written_to_the_configuration_unless_asked() {
    let harness = Harness::start().await;

    let result = harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "sonnet": "gpt-5.4-mini" } }),
        )
        .await
        .unwrap();

    assert_eq!(result["persisted"], json!(false));
    assert!(
        !harness.config_file.exists(),
        "nothing should have been written to the configuration"
    );
}

/// And when it is asked for, the file says so afterwards.
#[tokio::test]
async fn a_persisted_change_survives_in_the_file() {
    let harness = Harness::start().await;
    std::fs::write(
        &harness.config_file,
        "# why this is what it is\n[tiers]\nsonnet = \"gpt-5.6-terra\"\n",
    )
    .unwrap();

    let result = harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "sonnet": "gpt-5.4-mini" }, "persist": true }),
        )
        .await
        .unwrap();

    assert_eq!(result["persisted"], json!(true));

    let written = std::fs::read_to_string(&harness.config_file).unwrap();
    assert!(written.contains(r#"sonnet = "gpt-5.4-mini""#), "{written}");
    // The comment is the whole reason this is a text edit rather than a
    // re-serialization. Losing it would be invisible: the file would still
    // parse, still work, and never again explain itself.
    assert!(written.contains("# why this is what it is"), "{written}");
}

/// A refused value is never written. The check that refuses it runs before
/// anything reaches the file, so a daemon cannot be left with a configuration
/// it will not start from.
#[tokio::test]
async fn a_refused_effort_never_reaches_the_file() {
    let harness = Harness::start().await;
    std::fs::write(&harness.config_file, "port = 8787\n").unwrap();

    let error = harness
        .call_with("effort.set", json!({ "effort": "cheap", "persist": true }))
        .await
        .unwrap_err();

    assert!(error.contains("cheap"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&harness.config_file).unwrap(),
        "port = 8787\n"
    );
}

/// A change that could not be written leaves the daemon as it was.
///
/// The caller is told the write failed; a daemon that had already moved would
/// be running a policy nobody chose, reported as an error, and gone at the next
/// restart. Validate, then persist, then apply — so the only ordering where the
/// two can disagree is the one where nothing was asked to be persisted at all.
#[tokio::test]
async fn a_change_that_cannot_be_written_is_not_applied_either() {
    let harness = Harness::start().await;

    // A real configuration that reads fine and cannot be written: the read leg
    // has to succeed, or this would prove the wrong half of the ordering.
    let unwritable = harness.config_file.parent().unwrap().join("read-only.toml");
    std::fs::write(&unwritable, "port = 8787\n").unwrap();
    let mut permissions = std::fs::metadata(&unwritable).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&unwritable, permissions).unwrap();
    let harness = harness.with_config(unwritable).await;

    let before = harness.policy.get();

    let error = harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "sonnet": "gpt-5.4-mini" }, "persist": true }),
        )
        .await
        .unwrap_err();
    assert!(error.contains("could not write"), "{error}");
    assert_eq!(harness.policy.get().tiers(), before.tiers());

    let error = harness
        .call_with("effort.set", json!({ "effort": "high", "persist": true }))
        .await
        .unwrap_err();
    assert!(error.contains("could not write"), "{error}");
    assert_eq!(
        harness.policy.get().effort_ceiling(),
        before.effort_ceiling()
    );
}

/// One loopback reply, then done — enough to have an authorization server
/// refuse a grant without any test reaching the network.
async fn refusing_token_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;

            // Consume the whole request before answering. Replying first races
            // the reply against the client still writing its request, and
            // closing a socket with unread inbound data resets the connection
            // instead of finishing it — a race the client loses only when the
            // machine is busy, which made this stub the suite's one flaky
            // dependency.
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while let Ok(read) = stream.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(headers_end) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let content_length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .map(str::to_owned)
                    })
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                if request.len() >= headers_end + content_length {
                    break;
                }
            }

            let body = r#"{"error":"refresh_token_expired"}"#;
            let reply = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });

    url
}

/// A grant the backend has refused is reported as such.
///
/// `connected` stays true — the credential file is still there and still
/// readable — so without this nothing anywhere says the provider is finished.
/// A front-end would show it healthy while every turn failed with an
/// authentication error, which is the worst of both: no figure to act on and
/// no reason to look.
#[tokio::test]
async fn status_reports_a_grant_the_backend_refused() {
    let harness = Harness::start().await;
    harness
        .store
        .save(&Credentials {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            id_token: None,
            account_id: Some("acct_9".to_owned()),
            // In the past, so the next use has to refresh — which is what gets
            // refused.
            expires_at: Some(1_000),
        })
        .unwrap();

    let tokens = Arc::new(proxenos::auth::tokens::TokenSource::new(
        Arc::clone(&harness.store) as Arc<dyn CredentialStore>,
        refusing_token_endpoint().await,
        "client-abc",
        Arc::new(proxenos::auth::tokens::SystemClock),
    ));

    let harness = harness.with_tokens(Arc::clone(&tokens)).await;
    assert_eq!(
        harness.call("status").await.unwrap()["auth"]["dead"],
        json!(false)
    );

    let refusal = tokens
        .access_token()
        .await
        .expect_err("the grant should have been refused");
    assert!(
        tokens.is_dead(),
        "the refusal was not treated as terminal: {refusal:?}"
    );

    let status = harness.call("status").await.unwrap();
    assert_eq!(status["auth"]["connected"], json!(true));
    assert_eq!(status["auth"]["dead"], json!(true));
}

// ---------------------------------------------------------------------------
// Version skew. One binary is both the daemon and the CLI, and upgrading the
// file on disk does not restart the daemon — so a newer CLI talking to an older
// daemon is the ordinary state after an upgrade, not an exotic one.
// ---------------------------------------------------------------------------

/// Present and empty rather than absent.
///
/// Absence has to mean exactly one thing. With the key omitted when the policy
/// is empty, a daemon that predates client policy and a daemon told to publish
/// none look identical from here, and the CLI cannot tell the operator which it
/// is. Same rule `unlisted_tiers` already follows.
#[tokio::test]
async fn the_policy_half_is_present_and_empty_rather_than_absent() {
    let harness = Harness::start()
        .await
        .with_client(proxenos::config::ClientConfig {
            deny_skills: Vec::new(),
            disable_connectors: false,
        })
        .await;
    let result = harness.call("env").await.unwrap();

    assert_eq!(
        result["settings"],
        json!({}),
        "no policy is still an answer, and has to be reported as one: {result}"
    );
}

/// The capability is read from the payload, not from a version comparison.
///
/// Comparing version strings forces a policy about which differences matter and
/// gets it wrong for anyone running a patched build or anyone who forgets to
/// raise the number. The question actually being asked is whether this daemon
/// can answer for the policy, and the payload answers it directly.
#[test]
fn a_daemon_that_predates_the_policy_is_told_apart_from_one_that_has_none() {
    let predates = json!({ "variables": [] });
    let has_none = json!({ "variables": [], "settings": {} });

    let error = control::require_client_policy(&predates)
        .expect_err("a daemon that cannot answer for the policy must not be assumed to have none");
    assert!(
        error.message.contains("older build"),
        "the refusal has to name the situation: {}",
        error.message
    );
    assert!(
        error.message.to_lowercase().contains("restart the daemon"),
        "and what to do: {}",
        error.message
    );

    control::require_client_policy(&has_none)
        .expect("a daemon that published an empty policy answered the question");
}

/// `status` names both versions when they differ, and this is where an operator
/// looks first when something behaves as though a change never landed.
#[test]
fn status_names_a_version_skew_between_the_daemon_and_this_binary() {
    let stale = json!({
        "base_url": "http://127.0.0.1:8787",
        "version": "0.0.1-from-before",
        "auth": { "connected": false },
    });

    let rendered = render::status(&stale);
    assert!(
        rendered.contains("0.0.1-from-before"),
        "the daemon's version has to appear: {rendered}"
    );
    assert!(
        rendered.contains(env!("CARGO_PKG_VERSION")),
        "and this binary's, so the two can be compared at a glance: {rendered}"
    );
    assert!(
        rendered.to_lowercase().contains("restart the daemon"),
        "and what to do about it: {rendered}"
    );
}

/// Agreement is the common case and says nothing. A line that appears on every
/// run is one nobody reads on the run that matters.
#[tokio::test]
async fn status_is_quiet_when_the_daemon_is_this_binary() {
    let harness = Harness::start().await;
    let result = harness.call("status").await.unwrap();

    assert_eq!(result["version"], json!(env!("CARGO_PKG_VERSION")));
    let rendered = render::status(&result);
    assert!(
        !rendered.to_lowercase().contains("restart the daemon"),
        "nothing to warn about: {rendered}"
    );
}

/// Shell exports keep working against an older daemon, because everything they
/// carry is routing and an older daemon has all of it. They say what is
/// missing, which is the whole reason this path is allowed to continue.
#[test]
fn shell_exports_keep_working_against_an_older_daemon_and_say_so() {
    let predates = json!({
        "variables": [["ANTHROPIC_BASE_URL", "http://127.0.0.1:8787"]],
    });

    let rendered = render::env_shell(&predates);
    assert!(
        rendered.contains("export ANTHROPIC_BASE_URL=http://127.0.0.1:8787"),
        "routing still works: {rendered}"
    );
    assert!(
        rendered.to_lowercase().contains("restart the daemon"),
        "and the reason the policy is missing is named: {rendered}"
    );
}

/// The answer arrives before the process goes.
///
/// A caller that saw the connection close with no reply could not tell a clean
/// stop from a crash, and the whole point of asking over the socket rather than
/// with a signal is that the asker learns what happened. So the request marks
/// the intent, and the run loop is only released once the response has been
/// written. This asserts both halves in the order they have to happen: a reply
/// came back, and only then did the signal the daemon waits on fire.
#[tokio::test]
async fn a_stop_answers_first_and_releases_the_run_loop_after() {
    let harness = Harness::start().await;

    let result = harness.call("shutdown").await.unwrap();
    assert_eq!(result["stopping"], json!(true));
    assert_eq!(
        result["version"],
        json!(env!("CARGO_PKG_VERSION")),
        "the answer says which build is going away: {result}"
    );

    tokio::time::timeout(std::time::Duration::from_secs(2), harness.shutdown.wait())
        .await
        .expect("the run loop should be released once the answer has been written");
}

/// Until it is asked for, nothing is armed. A run loop released by anything
/// other than an explicit stop would be a daemon that exits on its own.
#[tokio::test]
async fn nothing_arms_a_stop_that_was_not_asked_for() {
    let harness = Harness::start().await;
    harness.call("status").await.unwrap();

    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        harness.shutdown.wait(),
    )
    .await;
    assert!(waited.is_err(), "no stop was requested, so nothing fires");
}

/// Two builds can carry the same version string and one still be older than a
/// feature. Then the string says nothing and the missing field is the only
/// evidence there is, so `status` reads that instead.
#[test]
fn status_names_an_older_build_even_when_the_version_string_matches() {
    let same_version_older_build = json!({
        "base_url": "http://127.0.0.1:8787",
        "version": env!("CARGO_PKG_VERSION"),
        "auth": { "connected": false },
    });

    let rendered = render::status(&same_version_older_build);
    assert!(
        rendered.contains("older build"),
        "the version matches, so only the missing field can say this: {rendered}"
    );
}

/// A daemon old enough not to report a version at all — which is what every
/// build before this one is. Naming a number it never sent would be inventing
/// one.
#[test]
fn status_does_not_invent_a_version_the_daemon_never_sent() {
    let ancient = json!({
        "base_url": "http://127.0.0.1:8787",
        "auth": { "connected": false },
    });

    let rendered = render::status(&ancient);
    assert!(rendered.contains("restart it"), "{rendered}");
    assert!(
        !rendered.contains("nothing,"),
        "no invented figure, and no sentence built around one: {rendered}"
    );
}

/// A stop is observed by what answers afterwards, and silence is a statement
/// about timing rather than about the daemon: a supervisor quick enough leaves
/// no gap to see, and one that throttles a respawn leaves a gap longer than any
/// sensible wait. The identity is what actually changes when the process does.
#[tokio::test]
async fn status_carries_an_identity_for_the_process_answering() {
    let harness = Harness::start().await;
    let first = harness.call("status").await.unwrap();

    let instance = first["instance"]
        .as_str()
        .expect("an answering daemon has to be identifiable");
    assert!(!instance.is_empty());

    // Stable within one process: an id that changed per call would report a
    // restart on every poll.
    let second = harness.call("status").await.unwrap();
    assert_eq!(second["instance"], first["instance"]);
}

// ---------------------------------------------------------------------------
// §3 — more than one account.
// ---------------------------------------------------------------------------

fn grant(account: &str, token: &str) -> Credentials {
    Credentials {
        access_token: token.to_owned(),
        refresh_token: format!("refresh-{account}"),
        id_token: Some(id_token(json!({
            "email": format!("{account}@example.test"),
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account,
                "chatgpt_plan_type": "plus",
            },
        }))),
        account_id: Some(account.to_owned()),
        expires_at: Some(1_800_000_000),
    }
}

/// `accounts` lists what is stored and says which one serves turns. A
/// front-end that could not tell would offer a switch with no current value.
#[tokio::test]
async fn accounts_lists_what_is_stored_and_which_one_serves_turns() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    let listed = harness.call("accounts").await.unwrap();
    let accounts = listed["accounts"].as_array().expect("a list of accounts");

    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0]["name"], json!("acct_one"));
    assert_eq!(accounts[0]["selected"], json!(false));
    assert_eq!(accounts[1]["name"], json!("acct_two"));
    assert_eq!(accounts[1]["selected"], json!(true));
    // Something a person can tell two accounts apart by.
    assert_eq!(accounts[1]["email"], json!("acct_two@example.test"));
    assert_eq!(listed["selected"], json!("acct_two"));

    // And no token reaches a caller. This answer leaves the process.
    let rendered = listed.to_string();
    for secret in ["a-one", "a-two", "refresh-acct_one", "refresh-acct_two"] {
        assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
    }
}

/// `accounts.select` moves what serves turns, not only what `status` reports.
///
/// The store is what every request authenticates through, so the assertion is
/// on the grant that comes out of it rather than on the answer this method
/// gives about itself.
#[tokio::test]
async fn selecting_an_account_moves_the_grant_that_serves_turns() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();
    assert_eq!(harness.store.load().unwrap().unwrap().access_token, "a-two");

    let answer = harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .unwrap();

    assert_eq!(answer["selected"], json!("acct_one"));
    assert_eq!(
        harness.store.load().unwrap().unwrap().access_token,
        "a-one",
        "the selection did not reach the store every request reads"
    );

    let status = harness.call("status").await.unwrap();
    assert_eq!(status["auth"]["account"], json!("acct_one"));
    assert_eq!(status["auth"]["account_id"], json!("acct_one"));
}

/// A quota belongs to an account. Carrying the previous account's snapshot
/// across a switch would report headroom the new account may not have, which
/// is the direction that costs something.
#[tokio::test]
async fn selecting_an_account_drops_the_previous_accounts_quota() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();
    harness.usage.record(&proxenos::usage::Snapshot {
        plan: Some("plus".to_owned()),
        ..Default::default()
    });
    assert!(harness.call("usage").await.unwrap()["known"] == json!(true));

    harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .unwrap();

    let usage = harness.call("usage").await.unwrap();
    assert_eq!(
        usage["known"],
        json!(false),
        "the previous account's quota survived the switch: {usage}"
    );
}

/// Selecting something that is not stored says what is.
#[tokio::test]
async fn selecting_an_unknown_account_names_the_stored_ones() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();

    let error = harness
        .call_with("accounts.select", json!({ "account": "ghost" }))
        .await
        .expect_err("an unknown account should be refused");

    assert!(error.contains("ghost"), "{error}");
    assert!(error.contains("acct_one"), "{error}");

    // And naming nothing at all is refused rather than silently doing nothing.
    let error = harness
        .call_with("accounts.select", json!({}))
        .await
        .expect_err("a call naming no account should be refused");
    assert!(error.contains("account"), "{error}");
}

/// `status` names the account serving turns and what else is stored, so the
/// answer that reports a connection also reports what it is connected as.
#[tokio::test]
async fn status_names_the_serving_account_and_the_others() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    let status = harness.call("status").await.unwrap();

    assert_eq!(status["auth"]["connected"], json!(true));
    assert_eq!(status["auth"]["account"], json!("acct_two"));
    let accounts = status["auth"]["accounts"]
        .as_array()
        .expect("status should list the stored accounts");
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0]["name"], json!("acct_one"));
    assert_eq!(accounts[1]["selected"], json!(true));
}

/// `accounts.forget` names the account it cleared and leaves the rest usable.
#[tokio::test]
async fn forgetting_names_the_account_it_cleared_and_leaves_the_rest() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    // With nothing named, the account serving turns is the one that goes.
    let answer = harness.call("accounts.forget").await.unwrap();
    assert_eq!(answer["forgotten"], json!("acct_two"));
    // Who serves turns now, so a caller does not have to ask again.
    assert_eq!(answer["serving"], json!("acct_one"));
    assert_eq!(
        harness.store.load().unwrap().unwrap().access_token,
        "a-one",
        "the remaining account must still serve turns"
    );

    // Naming one clears that one.
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();
    let answer = harness
        .call_with("accounts.forget", json!({ "account": "acct_one" }))
        .await
        .unwrap();
    assert_eq!(answer["forgotten"], json!("acct_one"));
    let listed = harness.call("accounts").await.unwrap();
    assert_eq!(listed["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(listed["accounts"][0]["name"], json!("acct_two"));

    // Clearing the last one empties the store, and doing it again is safe.
    let answer = harness.call("accounts.forget").await.unwrap();
    assert_eq!(answer["serving"], Value::Null, "nothing is left to serve");
    assert!(harness.store.load().unwrap().is_none());
    harness.call("accounts.forget").await.unwrap();
}

/// A refusal is about a grant. Switching accounts replaces the grant, so the
/// refusal has to go with it — otherwise the daemon reports the new account as
/// dead and refuses to spend it without ever having tried.
#[tokio::test]
async fn switching_accounts_clears_a_refusal_that_belonged_to_the_old_grant() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(
            &Credentials {
                expires_at: Some(1),
                ..grant("acct_two", "a-two")
            },
            None,
        )
        .unwrap();

    // A token source whose refresh endpoint refuses, so the grant is marked
    // dead exactly as a real refusal would mark it. No network: the endpoint
    // is a loopback stub that answers every request with a dead-grant refusal.
    let server = RefusingTokens::start().await;
    let tokens = Arc::new(proxenos::auth::tokens::TokenSource::new(
        Arc::clone(&harness.store) as Arc<dyn CredentialStore>,
        server.url.clone(),
        "client-abc",
        Arc::new(proxenos::auth::tokens::SystemClock),
    ));
    tokens
        .access_token()
        .await
        .expect_err("the grant is refused");
    assert!(tokens.is_dead());

    let harness = harness.with_tokens(Arc::clone(&tokens)).await;
    assert_eq!(
        harness.call("status").await.unwrap()["auth"]["dead"],
        json!(true)
    );

    harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .unwrap();

    assert!(
        !tokens.is_dead(),
        "the refusal belonged to the grant that was just switched away from"
    );
    assert_eq!(
        harness.call("status").await.unwrap()["auth"]["dead"],
        json!(false)
    );
}

/// A loopback stub that refuses every refresh the way a retired grant is
/// refused. Nothing here reaches the network.
struct RefusingTokens {
    url: String,
}

impl RefusingTokens {
    async fn start() -> Self {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/token",
            post(|_body: String| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    r#"{"error":"refresh_token_reused"}"#,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: format!("http://{addr}/token"),
        }
    }
}

/// What an operator sees. The account serving turns is marked, because a list
/// of names with no current value is the one thing this verb must not print.
#[tokio::test]
async fn the_rendered_account_list_marks_the_one_serving_turns() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    let rendered = render::accounts(&harness.call("accounts").await.unwrap());

    let serving: Vec<&str> = rendered
        .lines()
        .filter(|line| line.starts_with('*'))
        .collect();
    assert_eq!(
        serving.len(),
        1,
        "exactly one account serves turns: {rendered}"
    );
    assert!(serving[0].contains("acct_two"), "{rendered}");
    assert!(rendered.contains("acct_one@example.test"), "{rendered}");

    // An empty store says what to do about it rather than printing nothing.
    harness.call("accounts.forget").await.unwrap();
    harness.call("accounts.forget").await.unwrap();
    let rendered = render::accounts(&harness.call("accounts").await.unwrap());
    assert!(rendered.contains("login"), "{rendered}");
}

/// Forgetting an account that was not serving turns leaves the serving one's
/// quota and its refusal alone.
///
/// Both belong to the grant being spent. Dropping the snapshot costs the
/// operator a figure they had; forgetting a refusal is worse — `status` would
/// report a healthy grant while every dispatch kept failing, which is the one
/// thing that field exists to prevent.
#[tokio::test]
async fn removing_an_idle_account_leaves_the_serving_grant_alone() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_spare", "a-spare"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_serving", "a-serving"), None)
        .unwrap();
    harness.usage.record(&proxenos::usage::Snapshot {
        plan: Some("plus".to_owned()),
        ..Default::default()
    });

    harness
        .call_with("accounts.forget", json!({ "account": "acct_spare" }))
        .await
        .unwrap();

    assert_eq!(
        harness.call("usage").await.unwrap()["known"],
        json!(true),
        "the serving account's quota was discarded with another account"
    );
    assert_eq!(
        harness.store.load().unwrap().unwrap().access_token,
        "a-serving"
    );
}

/// §3 — `auth.accounts` is present and empty rather than absent, including on
/// the answer that says nothing is connected. That is the state a front-end
/// most wants the list on, and a caller written to the documented contract
/// would read `undefined` where the document promised `[]`.
#[tokio::test]
async fn status_lists_accounts_even_when_nothing_is_connected() {
    let harness = Harness::start().await;

    let status = harness.call("status").await.unwrap();

    assert_eq!(status["auth"]["connected"], json!(false));
    assert_eq!(status["auth"]["accounts"], json!([]));
}

/// A switch reaches conversations already in flight.
///
/// A conduit sets its account on the connection at dial and reuses it for the
/// life of the conversation, so a session bound to the previous account would
/// keep being billed to it — and keep being refused by it — until the socket
/// dropped. Live sessions are dropped so the next turn dials again. That costs
/// a full upload for each one, which is the direction §4.3 already resolves
/// every ambiguity toward.
#[tokio::test]
async fn selecting_an_account_ends_conversations_bound_to_the_previous_one() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    let input = vec![proxenos_core::responses::InputItem::Message {
        role: proxenos_core::responses::ItemRole::User,
        content: Vec::new(),
    }];
    let _session = harness.sessions.resolve(&input);
    assert_eq!(harness.sessions.len(), 1);

    harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .unwrap();

    assert!(
        harness.sessions.is_empty(),
        "a conversation bound to the previous account survived the switch"
    );
}

/// A catalog describes one account's plan. After a switch it describes the
/// account that is no longer serving, and says so.
///
/// It is fetched once, at startup, with the account selected then. Nothing
/// refetches it, so `models` and the tier validation behind `tiers.set` go on
/// answering for the previous plan — a free account keeps being told it cannot
/// have what a Plus account offers, and the other way round. Until a refetch
/// exists, the answer states that the list was not fetched for the account
/// being served rather than presenting it as this account's menu.
#[tokio::test]
async fn the_catalog_says_when_it_was_fetched_for_another_account() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    // Fetched for the account serving turns: nothing to flag.
    let harness = harness.with_catalog_for("acct_two").await;
    assert_eq!(
        harness.call("status").await.unwrap()["catalog_stale"],
        json!(false)
    );
    assert_eq!(harness.call("models").await.unwrap()["stale"], json!(false));

    harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .unwrap();

    let status = harness.call("status").await.unwrap();
    assert_eq!(
        status["catalog_stale"],
        json!(true),
        "the catalog still claims to describe the account now serving: {status}"
    );
    assert_eq!(harness.call("models").await.unwrap()["stale"], json!(true));
    assert!(
        render::status(&status).contains("acct_two"),
        "an operator should be told which account the list belongs to: {}",
        render::status(&status)
    );
}

/// A configuration mapping every tier of each named account onto the one model
/// that account's catalog offers.
///
/// The stub answers a different model per account, which is the situation this
/// exists for: two accounts with disjoint menus cannot share one mapping.
fn mapping_per_account(accounts: &[&str]) -> proxenos::config::Config {
    let mut config = proxenos::config::Config::default();
    for account in accounts {
        let model = format!("model-for-{account}");
        config.accounts.insert(
            (*account).to_owned(),
            proxenos::config::AccountConfig {
                tiers: proxenos::config::Tiers {
                    opus: Some(model.clone().into()),
                    sonnet: Some(model.clone().into()),
                    haiku: Some(model.clone().into()),
                    fable: Some(model.into()),
                },
                effort: None,
            },
        );
    }
    config
}

/// A switch refetches the catalog for the account now serving.
///
/// The list is one account's menu, so after a switch it has to be asked for
/// again. Nothing here reaches the network: the stub answers on loopback and
/// keys its answer on the account header, which is also what proves the
/// refetch was made *as* the new account rather than merely made.
#[tokio::test]
async fn selecting_an_account_refetches_the_catalog_as_that_account() {
    let catalogs = CatalogServer::start().await;
    let harness = Harness::start()
        .await
        .with_configuration(mapping_per_account(&["acct_one", "acct_two"]))
        .await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    let harness = harness.with_catalog_source(&catalogs.url).await;

    // The daemon started holding what it fetched for the account selected
    // then, which the stub answers with a model only that account has.
    let models = harness.call("models").await.unwrap();
    assert_eq!(models["models"][0]["id"], json!("model-for-acct_two"));

    let answer = harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .unwrap();
    assert_eq!(answer["catalog_refreshed"], json!(true));

    let models = harness.call("models").await.unwrap();
    assert_eq!(
        models["models"][0]["id"],
        json!("model-for-acct_one"),
        "the list still describes the account that stopped serving turns"
    );
    assert_eq!(
        models["stale"],
        json!(false),
        "a list fetched for this account is not stale"
    );
    assert_eq!(
        harness.call("status").await.unwrap()["catalog_stale"],
        json!(false)
    );

    assert_eq!(
        catalogs.accounts(),
        vec!["acct_two".to_owned(), "acct_one".to_owned()],
        "the refetch has to be made as the account now serving"
    );
}

/// A refetch that fails keeps the list already in force.
///
/// Fetch failure is not evidence that a model went away (§7.1). Replacing a
/// real list with the fallback on a network blink would withdraw models the
/// account has, and every tier mapped to one would start reading as withheld.
#[tokio::test]
async fn a_failed_refetch_keeps_the_catalog_already_in_force() {
    let catalogs = CatalogServer::start().await;
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();
    let harness = harness.with_catalog_source(&catalogs.url).await;
    assert_eq!(
        harness.call("models").await.unwrap()["models"][0]["id"],
        json!("model-for-acct_two")
    );

    catalogs.refuse();
    let answer = harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .unwrap();

    assert_eq!(
        answer["selected"],
        json!("acct_one"),
        "the switch still happened"
    );
    assert_eq!(answer["catalog_refreshed"], json!(false));
    let models = harness.call("models").await.unwrap();
    assert_eq!(
        models["models"][0]["id"],
        json!("model-for-acct_two"),
        "a failed fetch replaced the list with something else"
    );
    // And it says the list is not this account's, which is the honest report
    // when it could not be replaced.
    assert_eq!(models["stale"], json!(true));
}

/// What the stub carries: what it was asked for, and whether it is refusing.
type CatalogState = (
    Arc<std::sync::Mutex<Vec<String>>>,
    Arc<std::sync::atomic::AtomicBool>,
);

/// A catalog stub on loopback, answering per account.
struct CatalogServer {
    url: String,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    refusing: Arc<std::sync::atomic::AtomicBool>,
}

impl CatalogServer {
    async fn start() -> Self {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let refusing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state = (Arc::clone(&seen), Arc::clone(&refusing));

        let app = axum::Router::new().route(
            "/models",
            axum::routing::get(
                |axum::extract::State(state): axum::extract::State<CatalogState>,
                 headers: axum::http::HeaderMap| async move {
                    let (seen, refusing) = state;
                    let account = headers
                        .get("chatgpt-account-id")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("unknown")
                        .to_owned();
                    if refusing.load(std::sync::atomic::Ordering::SeqCst) {
                        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, String::new());
                    }
                    if let Ok(mut seen) = seen.lock() {
                        seen.push(account.clone());
                    }
                    (
                        axum::http::StatusCode::OK,
                        json!({ "data": [
                            { "id": format!("model-for-{account}"), "context_window": 272_000 }
                        ] })
                        .to_string(),
                    )
                },
            ),
        );
        let app = app.with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            url: format!("http://{addr}/models"),
            seen,
            refusing,
        }
    }

    fn refuse(&self) {
        self.refusing
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn accounts(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }
}

/// Forgetting the account that was serving turns hands over to another one,
/// which is a switch by another name: the catalog is asked for again as
/// whoever serves now.
#[tokio::test]
async fn forgetting_the_serving_account_refetches_the_catalog() {
    let catalogs = CatalogServer::start().await;
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();
    let harness = harness.with_catalog_source(&catalogs.url).await;

    let answer = harness.call("accounts.forget").await.unwrap();

    assert_eq!(answer["forgotten"], json!("acct_two"));
    assert_eq!(answer["catalog_refreshed"], json!(true));
    assert_eq!(
        harness.call("models").await.unwrap()["models"][0]["id"],
        json!("model-for-acct_one")
    );
    assert_eq!(
        catalogs.accounts(),
        vec!["acct_two".to_owned(), "acct_one".to_owned()]
    );
}

/// An account whose grant carried no email is still identified by the id the
/// backend knows it by. A null field is not a value, and treating it as one
/// hides something the answer is carrying.
#[tokio::test]
async fn the_account_list_falls_back_to_the_id_when_there_is_no_email() {
    let harness = Harness::start().await;
    harness
        .store
        .add(
            &Credentials {
                access_token: "a".to_owned(),
                refresh_token: "r".to_owned(),
                id_token: None,
                account_id: Some("acct_nameless".to_owned()),
                expires_at: None,
            },
            None,
        )
        .unwrap();

    let rendered = render::accounts(&harness.call("accounts").await.unwrap());

    assert!(rendered.contains("acct_nameless"), "{rendered}");
    assert!(!rendered.contains("id unknown"), "{rendered}");
}

/// `accounts.rename` moves the name in the store every request reads, and the
/// account keeps serving turns under it.
#[tokio::test]
async fn renaming_an_account_moves_the_name_it_is_selected_by() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();

    let answer = harness
        .call_with(
            "accounts.rename",
            json!({ "account": "acct_one", "name": "work" }),
        )
        .await
        .unwrap();

    assert_eq!(answer["renamed"], json!("acct_one"));
    assert_eq!(answer["name"], json!("work"));

    let listed = harness.call("accounts").await.unwrap();
    assert_eq!(listed["accounts"][0]["name"], json!("work"));
    assert_eq!(listed["selected"], json!("work"));
    assert_eq!(
        listed["accounts"][0]["account_id"],
        json!("acct_one"),
        "the id the backend knows it by does not move"
    );
    assert_eq!(
        harness.store.load().unwrap().unwrap().access_token,
        "a-one",
        "the grant should be untouched"
    );

    // The new name is what selects it, and a call naming neither half is
    // refused rather than doing something arbitrary.
    harness
        .call_with("accounts.select", json!({ "account": "work" }))
        .await
        .unwrap();
    let error = harness
        .call_with("accounts.rename", json!({ "account": "work" }))
        .await
        .expect_err("a rename with no new name should be refused");
    assert!(error.contains("name"), "{error}");
}

/// §3 — an account says what it authenticates with. The two kinds are spent
/// against different endpoints, so a listing that did not distinguish them
/// would leave an operator guessing which of their accounts is which.
#[tokio::test]
async fn accounts_and_status_say_what_kind_each_account_is() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add_key("billing", "key-secret", Provider::Codex)
        .unwrap();

    let listed = harness.call("accounts").await.unwrap();
    assert_eq!(listed["accounts"][0]["kind"], json!("grant"));
    assert_eq!(listed["accounts"][1]["kind"], json!("key"));
    assert_eq!(listed["selected"], json!("billing"));
    assert!(
        !listed.to_string().contains("key-secret"),
        "the key reached a caller: {listed}"
    );

    let status = harness.call("status").await.unwrap();
    assert_eq!(status["auth"]["accounts"][1]["kind"], json!("key"));

    // And an operator sees it: a key has no address to show, so the column
    // that tells two accounts apart says what it is instead of nothing.
    let rendered = render::accounts(&listed);
    assert!(rendered.contains("key"), "{rendered}");
    assert!(rendered.contains("acct_one@example.test"), "{rendered}");
}

/// A daemon serving turns as a key is connected.
///
/// `status` read the grant, and a key is not one, so an account that could
/// serve every turn reported not connected with `login` as the advice — the
/// one thing that would not help.
#[tokio::test]
async fn status_reports_a_key_account_as_connected() {
    let harness = Harness::start().await;
    harness
        .store
        .add_key("billing", "key-secret", Provider::Codex)
        .unwrap();

    let status = harness.call("status").await.unwrap();

    assert_eq!(status["auth"]["connected"], json!(true));
    assert_eq!(status["auth"]["account"], json!("billing"));
    assert_eq!(status["auth"]["kind"], json!("key"));
    // None of these exist behind a key, and none is invented.
    assert_eq!(status["auth"]["account_id"], Value::Null);
    assert_eq!(status["auth"]["email"], Value::Null);
    assert_eq!(status["auth"]["expires_at"], Value::Null);
    assert_eq!(status["auth"]["plan"], Value::Null);
    assert_eq!(status["auth"]["dead"], json!(false));

    let rendered = render::status(&status);
    assert!(rendered.contains("billing"), "{rendered}");
    assert!(rendered.contains("key"), "{rendered}");
    assert!(!rendered.contains("not connected"), "{rendered}");
}

/// A missing plan names no source. `grant` is where the fallback reads it
/// from, and an account holding a key has none — attributing a null to it says
/// something was asked that never was.
#[tokio::test]
async fn a_key_account_reports_no_plan_and_no_source_for_one() {
    let harness = Harness::start().await;
    harness
        .store
        .add_key("billing", "key-secret", Provider::Codex)
        .unwrap();

    let status = harness.call("status").await.unwrap();
    assert_eq!(status["auth"]["plan"], Value::Null);
    assert_eq!(status["auth"]["plan_source"], Value::Null);

    // A grant still says where its plan came from.
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    let status = harness.call("status").await.unwrap();
    assert_eq!(status["auth"]["plan"], json!("plus"));
    assert_eq!(status["auth"]["plan_source"], json!("grant"));
}

/// A grant this daemon knows no address or id for is not a key, and does not
/// read as one.
#[tokio::test]
async fn a_thin_grant_is_not_rendered_as_a_key() {
    let harness = Harness::start().await;
    harness
        .store
        .add(
            &Credentials {
                access_token: "a".to_owned(),
                refresh_token: "r".to_owned(),
                id_token: None,
                account_id: None,
                expires_at: None,
            },
            Some("mystery"),
        )
        .unwrap();

    let rendered = render::accounts(&harness.call("accounts").await.unwrap());
    assert!(rendered.contains("mystery"), "{rendered}");
    assert!(!rendered.contains("key"), "{rendered}");
}

/// A configuration whose named account maps a tier to `model`.
fn mapping_for(account: &str, model: &str) -> proxenos::config::Config {
    let mut config = proxenos::config::Config::default();
    // The shared table names a model this harness's catalog has, so what the
    // account does not override still validates — the assertion is about the
    // one tier it does.
    config.tiers = proxenos::config::Tiers {
        opus: Some("gpt-5.6-terra".into()),
        sonnet: Some("gpt-5.6-terra".into()),
        haiku: Some("gpt-5.6-terra".into()),
        fable: Some("gpt-5.6-terra".into()),
    };
    config.accounts.insert(
        account.to_owned(),
        proxenos::config::AccountConfig {
            tiers: proxenos::config::Tiers {
                opus: Some(model.to_owned().into()),
                ..proxenos::config::Tiers::default()
            },
            effort: Some("low".to_owned()),
        },
    );
    config
}

/// A switch puts the account's own mapping in force.
///
/// A catalog is one account's menu, so the mapping that was routing turns a
/// moment ago describes the account just moved off. Leaving it in place is how
/// a switch ends with every turn dispatched to a model this account is not
/// offered.
#[tokio::test]
async fn selecting_an_account_puts_its_own_mapping_in_force() {
    let harness = Harness::start()
        .await
        .with_configuration(mapping_for("acct_one", "gpt-5.4-mini"))
        .await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .unwrap();

    let opus = harness
        .policy
        .get()
        .tiers()
        .iter()
        .find(|tier| tier.tier == "opus")
        .unwrap()
        .model
        .clone();
    assert_eq!(
        opus, "gpt-5.4-mini",
        "the selected account's mapping did not reach what routes turns"
    );
    assert_eq!(
        harness.policy.get().effort_ceiling(),
        Some(proxenos_core::responses::Effort::Low),
        "the selected account's ceiling did not reach what routes turns"
    );
}

/// A switch whose mapping the target account cannot serve is refused, and
/// leaves the daemon exactly where it was.
///
/// The alternative is a daemon serving an account whose every turn is
/// dispatched to a model the backend will not answer for — a failure that
/// arrives one turn later, upstream, saying nothing about tier mapping.
#[tokio::test]
async fn a_switch_the_target_account_cannot_serve_is_refused() {
    let harness = Harness::start()
        .await
        .with_configuration(mapping_for("acct_one", "a-model-this-account-has-not"))
        .await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    let error = harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .unwrap_err();

    assert!(
        error.contains("a-model-this-account-has-not"),
        "the refusal should name the model: {error}"
    );
    assert_eq!(
        harness.store.load().unwrap().unwrap().access_token,
        "a-two",
        "a refused switch must leave the store where it was"
    );
}

/// Renaming an account takes its configuration with it.
///
/// The section is keyed by the name, so a rename that left it behind would
/// detach a mapping from the account it was written for — and a section naming
/// nobody is not an error, so nothing would say so. Only the header moves: what
/// the operator wrote about why a tier is what it is stays where it was.
#[tokio::test]
async fn renaming_an_account_moves_its_configuration_section() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("spare"))
        .unwrap();
    std::fs::write(
        &harness.config_file,
        "[accounts.spare]\n\
         effort = \"low\"\n\n\
         # opus is terra here because this account is on the plan that has it\n\
         [accounts.spare.tiers]\n\
         opus = \"gpt-5.6-terra\"\n",
    )
    .unwrap();

    let answer = harness
        .call_with(
            "accounts.rename",
            json!({ "account": "spare", "name": "work" }),
        )
        .await
        .unwrap();

    assert_eq!(answer["moved_configuration"], json!(true));
    let written = std::fs::read_to_string(&harness.config_file).unwrap();
    assert!(
        written.contains("[accounts.work]") && written.contains("[accounts.work.tiers]"),
        "both tables should have moved: {written}"
    );
    assert!(
        !written.contains("[accounts.spare"),
        "nothing should still be filed under the old name: {written}"
    );
    assert!(
        written.contains("# opus is terra here because this account is on the plan that has it"),
        "the comment explaining the mapping should survive: {written}"
    );
    assert!(
        written.contains("effort = \"low\""),
        "the body of each table should survive: {written}"
    );
}

/// An account with no section is renamed without the file being touched.
///
/// Most accounts have none. Writing one out of the shipped example for a
/// rename would put a file on disk the operator never asked for.
#[tokio::test]
async fn renaming_an_account_with_no_section_writes_nothing() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("spare"))
        .unwrap();

    let answer = harness
        .call_with(
            "accounts.rename",
            json!({ "account": "spare", "name": "work" }),
        )
        .await
        .unwrap();

    assert_eq!(answer["moved_configuration"], json!(false));
    assert!(
        !harness.config_file.exists(),
        "a rename with nothing to move should not create a configuration file"
    );
}

/// A persisted tier change is written where the value is read from.
///
/// The serving account's section shadows the shared table for the tiers it
/// names, so writing the shared one would leave the change in force on this
/// daemon and gone at the next start — written, and left looking applied.
#[tokio::test]
async fn a_persisted_tier_is_written_under_the_account_that_shadows_it() {
    let harness = Harness::start()
        .await
        .with_configuration(mapping_for("spare", "gpt-5.6-terra"))
        .await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("spare"))
        .unwrap();
    std::fs::write(
        &harness.config_file,
        "[tiers]\n\
         opus   = \"gpt-5.6-terra\"\n\
         sonnet = \"gpt-5.6-terra\"\n\n\
         [accounts.spare.tiers]\n\
         opus = \"gpt-5.6-terra\"\n",
    )
    .unwrap();

    harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "opus": "gpt-5.4-mini", "sonnet": "gpt-5.4-mini" }, "persist": true }),
        )
        .await
        .unwrap();

    let written: toml::Value =
        toml::from_str(&std::fs::read_to_string(&harness.config_file).unwrap()).unwrap();
    assert_eq!(
        written["accounts"]["spare"]["tiers"]["opus"].as_str(),
        Some("gpt-5.4-mini"),
        "opus is shadowed by the account section, so that is where it belongs"
    );
    assert_eq!(
        written["tiers"]["sonnet"].as_str(),
        Some("gpt-5.4-mini"),
        "sonnet is not shadowed, so the shared table is where it is read from"
    );
    assert_eq!(
        written["tiers"]["opus"].as_str(),
        Some("gpt-5.6-terra"),
        "the shared opus is not what this account reads, and was not touched"
    );
}

/// A change aimed at an account that is not serving turns is written and not
/// applied, and says which.
#[tokio::test]
async fn a_tier_set_for_another_account_is_written_but_not_in_force() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("spare"))
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), Some("work"))
        .unwrap();

    let before = harness.policy.get().tiers().to_vec();
    harness
        .call_with(
            "tiers.set",
            json!({ "account": "spare", "tiers": { "opus": "gpt-5.4-mini" }, "persist": true }),
        )
        .await
        .unwrap();

    let written: toml::Value =
        toml::from_str(&std::fs::read_to_string(&harness.config_file).unwrap()).unwrap();
    assert_eq!(
        written["accounts"]["spare"]["tiers"]["opus"].as_str(),
        Some("gpt-5.4-mini")
    );
    assert_eq!(
        harness.policy.get().tiers(),
        before,
        "the account serving turns is `work`, so nothing routing should have moved"
    );
}

/// The same call without `persist` would change nothing anywhere, and says so
/// rather than answering as though it did something.
#[tokio::test]
async fn a_tier_set_for_another_account_without_persist_is_refused() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("spare"))
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), Some("work"))
        .unwrap();

    let error = harness
        .call_with(
            "tiers.set",
            json!({ "account": "spare", "tiers": { "opus": "gpt-5.4-mini" } }),
        )
        .await
        .unwrap_err();

    assert!(error.contains("persist"), "{error}");
}

/// A mapping written for another account is not validated against the serving
/// account's catalog.
///
/// The list in force is one account's menu, and a mapping written for a
/// different account makes no claim about it. Refusing here would refuse the
/// case per-account mappings exist for: a model one plan has and the other
/// does not.
#[tokio::test]
async fn a_tier_set_for_another_account_is_not_judged_by_this_ones_catalog() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("spare"))
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), Some("work"))
        .unwrap();

    harness
        .call_with(
            "tiers.set",
            json!({
                "account": "spare",
                "tiers": { "opus": "a-model-only-spare-has" },
                "persist": true,
            }),
        )
        .await
        .unwrap();

    let written: toml::Value =
        toml::from_str(&std::fs::read_to_string(&harness.config_file).unwrap()).unwrap();
    assert_eq!(
        written["accounts"]["spare"]["tiers"]["opus"].as_str(),
        Some("a-model-only-spare-has")
    );
}

/// A mapping this daemon persisted is in force when that account is selected.
///
/// Write it, then switch to it, is the feature's own workflow. Resolving from a
/// startup snapshot meant the section had been written, the switch reported
/// success, and the shared table went into force instead — silently, until a
/// restart.
#[tokio::test]
async fn a_mapping_persisted_here_is_what_a_switch_puts_in_force() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("spare"))
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), Some("work"))
        .unwrap();

    std::fs::write(
        &harness.config_file,
        "[tiers]\n\
         opus   = \"gpt-5.6-terra\"\n\
         sonnet = \"gpt-5.6-terra\"\n\
         haiku  = \"gpt-5.6-terra\"\n\
         fable  = \"gpt-5.6-terra\"\n",
    )
    .unwrap();

    harness
        .call_with(
            "tiers.set",
            json!({
                "account": "spare",
                "tiers": { "opus": "gpt-5.4-mini" },
                "persist": true,
            }),
        )
        .await
        .unwrap();
    harness
        .call_with("accounts.select", json!({ "account": "spare" }))
        .await
        .unwrap();

    let opus = harness
        .policy
        .get()
        .tiers()
        .iter()
        .find(|tier| tier.tier == "opus")
        .unwrap()
        .model
        .clone();
    assert_eq!(
        opus, "gpt-5.4-mini",
        "the section this daemon wrote was not what the switch resolved"
    );
}

/// A change persisted after the daemon created a section goes into that
/// section, not into the shared table it now shadows.
#[tokio::test]
async fn a_second_persisted_change_lands_where_the_first_one_put_the_section() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("work"))
        .unwrap();

    std::fs::write(
        &harness.config_file,
        "[tiers]\n\
         opus   = \"gpt-5.6-terra\"\n\
         sonnet = \"gpt-5.6-terra\"\n\
         haiku  = \"gpt-5.6-terra\"\n\
         fable  = \"gpt-5.6-terra\"\n",
    )
    .unwrap();

    harness
        .call_with(
            "tiers.set",
            json!({
                "account": "work",
                "tiers": { "opus": "gpt-5.4-mini" },
                "persist": true,
            }),
        )
        .await
        .unwrap();
    harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "opus": "gpt-5.6-terra" }, "persist": true }),
        )
        .await
        .unwrap();

    let written: toml::Value =
        toml::from_str(&std::fs::read_to_string(&harness.config_file).unwrap()).unwrap();
    assert_eq!(
        written["accounts"]["work"]["tiers"]["opus"].as_str(),
        Some("gpt-5.6-terra"),
        "the second change was written to a table the first one now shadows"
    );
}

/// A rename the store refuses leaves the configuration file alone.
///
/// Writing the file first meant a refused rename still moved the section: the
/// account already holding the new name inherited another account's mapping,
/// and the other lost its own, with the error saying nothing about the file.
#[tokio::test]
async fn a_refused_rename_does_not_move_the_section() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("alpha"))
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), Some("bravo"))
        .unwrap();
    let before = "[accounts.alpha.tiers]\nopus = \"gpt-5.6-terra\"\n";
    std::fs::write(&harness.config_file, before).unwrap();

    harness
        .call_with(
            "accounts.rename",
            json!({ "account": "alpha", "name": "bravo" }),
        )
        .await
        .unwrap_err();

    assert_eq!(
        std::fs::read_to_string(&harness.config_file).unwrap(),
        before,
        "a refused rename rewrote the configuration file"
    );
}

/// A rename onto a name whose section is still in the file is refused.
///
/// Forgetting an account leaves its section behind, so a name can be free in
/// the store and taken in the file. Moving onto it would define one table
/// twice, which TOML refuses — and the daemon would fail to start on a file the
/// operator never edited.
#[tokio::test]
async fn a_rename_onto_an_occupied_section_is_refused() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("alpha"))
        .unwrap();
    std::fs::write(
        &harness.config_file,
        "[accounts.alpha.tiers]\nopus = \"gpt-5.6-terra\"\n\n\
         [accounts.bravo.tiers]\nopus = \"gpt-5.4-mini\"\n",
    )
    .unwrap();

    let error = harness
        .call_with(
            "accounts.rename",
            json!({ "account": "alpha", "name": "bravo" }),
        )
        .await
        .unwrap_err();

    assert!(error.contains("bravo"), "{error}");
    let names: Vec<String> = harness
        .store
        .accounts()
        .unwrap()
        .into_iter()
        .map(|account| account.name)
        .collect();
    assert_eq!(
        names,
        vec!["alpha".to_owned()],
        "a refused rename must leave the store where it was"
    );
}

/// Removing an account's ceiling reports the shared one, which is what applies.
///
/// `null` under an account section removes that account's override rather than
/// removing every ceiling. Reporting "no ceiling" would be a figure that lasted
/// until the next start and then quietly came back.
#[tokio::test]
async fn removing_an_accounts_ceiling_leaves_the_shared_one_in_force() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("work"))
        .unwrap();
    std::fs::write(
        &harness.config_file,
        "effort = \"medium\"\n\n[accounts.work]\neffort = \"high\"\n",
    )
    .unwrap();

    let answer = harness
        .call_with("effort.set", json!({ "effort": null, "persist": true }))
        .await
        .unwrap();

    assert_eq!(
        answer["effort"],
        json!("medium"),
        "the shared ceiling is what applies once the override is gone"
    );
    assert_eq!(
        harness.policy.get().effort_ceiling(),
        Some(proxenos_core::responses::Effort::Medium),
        "what routes turns disagreed with what a restart would produce"
    );
}

/// A change written for an account that is not serving turns does not report
/// itself as in effect.
#[tokio::test]
async fn a_change_for_another_account_does_not_claim_to_be_in_effect() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), Some("spare"))
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), Some("work"))
        .unwrap();

    let answer = harness
        .call_with(
            "tiers.set",
            json!({
                "account": "spare",
                "tiers": { "opus": "gpt-5.4-mini" },
                "persist": true,
            }),
        )
        .await
        .unwrap();

    assert_eq!(answer["account"], json!("spare"));
    assert!(
        !answer["detail"].as_str().unwrap().contains("in effect now"),
        "nothing here changed, and the answer said it had: {}",
        answer["detail"]
    );
}

/// §9.1 — a tier whose turns are relayed is not measured against the first
/// provider's catalog.
///
/// The id belongs to the second provider and is absent from this list by
/// construction, so validating it here refuses a mapping that is correct, with
/// a message naming a menu it was never offered on.
#[tokio::test]
async fn setting_a_relay_bound_tier_is_not_refused_by_the_first_providers_catalog() {
    let harness = Harness::start().await;
    harness
        .store
        .add_key("relay", "relay-key-value", Provider::Anthropic)
        .unwrap();
    harness.store.select("relay").unwrap();

    harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "sonnet": "claude-sonnet-5" } }),
        )
        .await
        .expect("a relayed id is not the catalog's to refuse");

    assert_eq!(
        harness
            .policy
            .get()
            .tiers()
            .iter()
            .find(|tier| tier.tier == "sonnet")
            .map(|tier| tier.model.as_str()),
        Some("claude-sonnet-5")
    );
}

/// The same tier is not reported as a model the backend withholds, either.
///
/// The catalog here hides an entry under the relayed id — contrived, so that
/// the exclusion is observable at all — because an id the first provider has
/// never heard of would drop out of that report by accident rather than by
/// rule, and an accident is not a thing a later change has to keep.
#[tokio::test]
async fn a_relay_bound_tier_is_not_reported_as_a_withheld_model() {
    let harness = Harness::with_catalog(
        r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000},
                    {"id":"claude-sonnet-5","context_window":200000,"visibility":"hide"}]}"#,
        "gpt-5.6-terra",
    )
    .await;
    harness
        .store
        .add_key("relay", "relay-key-value", Provider::Anthropic)
        .unwrap();
    harness.store.select("relay").unwrap();
    harness
        .call_with(
            "tiers.set",
            json!({ "tiers": { "sonnet": "claude-sonnet-5" } }),
        )
        .await
        .unwrap();

    let status = harness.call("status").await.unwrap();
    assert_eq!(
        status["unlisted_tiers"],
        json!([]),
        "the first provider's catalog does not speak for a relayed id"
    );
}

/// A daemon whose `env` answers for exactly this mapping and these accounts.
///
/// Built by hand rather than through the harness because the cases below turn
/// on which provider each tier's account is on, and that is the one thing the
/// harness fixes at start.
async fn env_for(
    dir: &tempfile::TempDir,
    tiers: Vec<ResolvedTier>,
    accounts: impl Fn(&FileStore),
) -> Value {
    let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
    accounts(&store);
    let state = ControlState {
        port: 8787,
        policy: Arc::new(proxenos::policy::Policy::new(
            proxenos::policy::Snapshot::new(
                tiers,
                None,
                proxenos::config::CrossAccountTiers::Permitted,
            ),
        )),
        catalog: Arc::new(CatalogSource::fixed(
            Catalog::parse(
                r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000},
                            {"id":"gpt-5.4-mini","context_window":200000}]}"#,
                95.0,
            )
            .unwrap(),
        )),
        credentials: store as Arc<dyn AccountStore>,
        capture: Arc::new(proxenos::recorder::Switches::default()),
        usage: Arc::new(proxenos::usage::UsageStore::default()),
        login: Arc::new(proxenos::auth::daemon_login::LoginFlow::default()),
        config: Arc::new(proxenos::config::Config::default()),
        shutdown: Arc::new(proxenos::daemon::Shutdown::default()),
        tokens: None,
        usage_endpoint: String::new(),
        sessions: Arc::new(proxenos::session::SessionStore::new()),
        config_path: None,
    };

    control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "env" }).to_string(),
    )
    .await
    .result
    .unwrap()
}

fn relayed(name: &'static str) -> impl Fn(&FileStore) {
    move |store: &FileStore| {
        store
            .add_key(name, "relay-key-value", Provider::Anthropic)
            .unwrap();
        store.select(name).unwrap();
    }
}

/// §7.2 — a mapping served entirely by the relay states no window at all.
///
/// The client recognizes these ids natively and already knows their windows, so
/// an override here would replace a real figure with one the first provider's
/// catalog cannot supply. And `CLAUDE_CODE_DISABLE_1M_CONTEXT` is omitted:
/// measured, it strips `context-1m-2025-08-07` from the beta list on the wire,
/// so setting it would deny an entitlement the account may actually hold.
#[tokio::test]
async fn an_all_relay_mapping_states_no_window_and_no_long_context_flag() {
    let dir = tempfile::tempdir().unwrap();
    let result = env_for(
        &dir,
        ["opus", "sonnet", "haiku", "fable"]
            .into_iter()
            .map(|tier| ResolvedTier {
                defaulted: false,
                account: None,
                tier,
                model: "claude-sonnet-5".to_owned(),
            })
            .collect(),
        relayed("relay"),
    )
    .await;

    // All three renderings of one variable set: the shell exports, the settings
    // document, and the list `exec` puts on the child.
    let shell = render::env_shell(&result);
    let settings: Value = serde_json::from_str(&render::settings_json(&result)).unwrap();
    let injected: std::collections::BTreeMap<String, String> =
        render::variables(&result).into_iter().collect();

    for absent in [
        "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
        "CLAUDE_CODE_DISABLE_1M_CONTEXT",
    ] {
        assert!(!shell.contains(absent), "{absent} in {shell}");
        assert_eq!(settings["env"][absent], Value::Null, "{absent} in settings");
        assert!(!injected.contains_key(absent), "{absent} in exec's injects");
    }

    // The final ids are still handed over, which is the whole point of the
    // launch surface: the client bakes them in and sends them for the session.
    assert_eq!(
        injected
            .get("ANTHROPIC_DEFAULT_SONNET_MODEL")
            .map(String::as_str),
        Some("claude-sonnet-5")
    );
    assert!(shell.contains("export ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-sonnet-5"));
    assert_eq!(
        settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
        json!("claude-sonnet-5")
    );
}

/// §7.2 — a mapping split across both providers states no window either, and
/// keeps the flag.
///
/// The override is global to the client, and only one side of a split mapping
/// can be right. The window is omitted because a figure taken from the first
/// provider's catalog would govern the relayed tier too, where nothing checks
/// it — the translating path has the proxy's own window guard behind it and the
/// relay path has nothing. The flag is kept for the opposite reason: without it
/// the client appends `[1m]` to the ids it does not recognize and assumes four
/// times the context they have.
#[tokio::test]
async fn a_mixed_mapping_states_no_window_and_keeps_the_long_context_flag() {
    let dir = tempfile::tempdir().unwrap();
    let result = env_for(
        &dir,
        vec![
            ResolvedTier {
                defaulted: false,
                account: None,
                tier: "opus",
                model: "gpt-5.6-terra".to_owned(),
            },
            ResolvedTier {
                defaulted: false,
                account: None,
                tier: "sonnet",
                model: "gpt-5.6-terra".to_owned(),
            },
            ResolvedTier {
                defaulted: false,
                account: None,
                tier: "fable",
                model: "gpt-5.4-mini".to_owned(),
            },
            ResolvedTier {
                defaulted: false,
                account: Some("relay".to_owned()),
                tier: "haiku",
                model: "claude-haiku-5".to_owned(),
            },
        ],
        |store: &FileStore| {
            store
                .add(
                    &Credentials {
                        access_token: "serving".to_owned(),
                        refresh_token: "refresh".to_owned(),
                        id_token: None,
                        account_id: Some("acct_serving".to_owned()),
                        expires_at: Some(u64::MAX / 2),
                    },
                    None,
                )
                .unwrap();
            store
                .add_key("relay", "relay-key-value", Provider::Anthropic)
                .unwrap();
            store.select("acct_serving").unwrap();
        },
    )
    .await;

    let shell = render::env_shell(&result);
    let settings: Value = serde_json::from_str(&render::settings_json(&result)).unwrap();
    let injected: std::collections::BTreeMap<String, String> =
        render::variables(&result).into_iter().collect();

    for absent in [
        "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    ] {
        assert!(!shell.contains(absent), "{absent} in {shell}");
        assert_eq!(settings["env"][absent], Value::Null, "{absent} in settings");
        assert!(!injected.contains_key(absent), "{absent} in exec's injects");
    }

    assert!(
        shell.contains("export CLAUDE_CODE_DISABLE_1M_CONTEXT=1"),
        "{shell}"
    );
    assert_eq!(
        settings["env"]["CLAUDE_CODE_DISABLE_1M_CONTEXT"],
        json!("1")
    );
    assert_eq!(
        injected
            .get("CLAUDE_CODE_DISABLE_1M_CONTEXT")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        injected
            .get("ANTHROPIC_DEFAULT_HAIKU_MODEL")
            .map(String::as_str),
        Some("claude-haiku-5")
    );
}

/// A switch is not refused over a pinned tier either.
///
/// The third door onto the same rule (§7.1). The catalog fetched for the
/// account being switched to is that account's menu, and a pinned tier names
/// another one — so measuring it here refuses a switch that is correct, and
/// leaves the daemon on an account the operator asked to leave.
#[tokio::test]
async fn a_switch_is_not_refused_over_a_pinned_tiers_model() {
    let mut config = proxenos::config::Config {
        cross_account_tiers: true,
        ..proxenos::config::Config::default()
    };
    config.tiers = proxenos::config::Tiers {
        opus: Some("gpt-5.6-terra".into()),
        sonnet: Some("gpt-5.6-terra".into()),
        haiku: Some("gpt-5.6-terra".into()),
        fable: Some("gpt-5.6-terra".into()),
    };
    config.accounts.insert(
        "acct_one".to_owned(),
        proxenos::config::AccountConfig {
            tiers: proxenos::config::Tiers {
                haiku: Some(proxenos::config::TierValue::Pinned(
                    proxenos::config::PinnedTier {
                        account: "acct_two".to_owned(),
                        model: "a-model-this-catalog-has-not".to_owned(),
                    },
                )),
                ..proxenos::config::Tiers::default()
            },
            effort: None,
        },
    );

    let harness = Harness::start().await.with_configuration(config).await;
    harness
        .store
        .add(&grant("acct_one", "a-one"), None)
        .unwrap();
    harness
        .store
        .add(&grant("acct_two", "a-two"), None)
        .unwrap();

    harness
        .call_with("accounts.select", json!({ "account": "acct_one" }))
        .await
        .expect("a pinned model is not the serving account's catalog to refuse");

    assert_eq!(
        harness
            .policy
            .get()
            .tiers()
            .iter()
            .find(|tier| tier.tier == "haiku")
            .and_then(|tier| tier.account.as_deref()),
        Some("acct_two"),
        "the pinned mapping should be the one in force"
    );
}

/// A snapshot with a percentage nothing else in these tests reports.
fn quota(used_percent: f64) -> proxenos::usage::Snapshot {
    proxenos::usage::Snapshot {
        plan: Some("plus".to_owned()),
        limit_reached: false,
        windows: vec![proxenos::usage::Window {
            used_percent,
            window_minutes: Some(300),
            resets_at: Some(1_789_487_264),
        }],
    }
}

/// **Build 2.** The per-account figures join the answer rather than replacing
/// it: the serving account stays exactly where it has always been, and each
/// account is named with its own figure and how fresh that figure is.
#[tokio::test]
async fn usage_names_each_account_beside_the_serving_figure() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_spare", "a-spare"), Some("spare"))
        .unwrap();
    harness
        .store
        .add(&grant("acct_main", "a-main"), Some("main"))
        .unwrap();

    // A turn served as the pinned account, and one as the serving account.
    harness
        .usage
        .record_for(Some("spare"), &quota(77.0), proxenos::usage::Source::Turn);
    harness
        .usage
        .record_for(None, &quota(11.0), proxenos::usage::Source::Turn);

    let usage = harness.call("usage").await.unwrap();

    // Unchanged where a reader already looks.
    assert_eq!(usage["known"], json!(true));
    assert_eq!(usage["plan"], json!("plus"));
    assert_eq!(usage["windows"][0]["used_percent"], json!(11.0));
    assert!(usage["models"].is_array());

    let accounts = usage["accounts"].as_array().expect("named figures");
    let by_name = |name: &str| {
        accounts
            .iter()
            .find(|entry| entry["account"] == json!(name))
            .unwrap_or_else(|| panic!("`{name}` should be reported: {usage}"))
            .clone()
    };

    let main = by_name("main");
    assert_eq!(main["known"], json!(true));
    assert_eq!(main["serving"], json!(true));
    assert_eq!(main["windows"][0]["used_percent"], json!(11.0));
    assert_eq!(main["source"], json!("turn"));
    assert!(
        main["measured_at"].as_u64().unwrap_or_default() > 0,
        "a figure states the moment it was taken: {main}"
    );

    let spare = by_name("spare");
    assert_eq!(spare["serving"], json!(false));
    assert_eq!(
        spare["windows"][0]["used_percent"],
        json!(77.0),
        "the pinned account's own figure is reported as its own: {spare}"
    );
}

/// **Build 3.** An account with no figure says it has none, and says why.
/// Never a zero, and never another account's.
#[tokio::test]
async fn an_account_with_no_figure_reports_unavailable() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_main", "a-main"), Some("main"))
        .unwrap();
    // A key holds no subscription entitlement, and the second provider's
    // quota endpoint is an open question — neither reports a plausible figure.
    harness
        .store
        .add_key("billing", "key-secret", Provider::Codex)
        .unwrap();
    harness
        .store
        .add_key("relay", "relay-secret", Provider::Anthropic)
        .unwrap();
    harness.store.select("main").unwrap();
    harness
        .usage
        .record_for(None, &quota(11.0), proxenos::usage::Source::Turn);

    let usage = harness.call("usage").await.unwrap();
    let accounts = usage["accounts"].as_array().expect("named figures");
    let by_name = |name: &str| {
        accounts
            .iter()
            .find(|entry| entry["account"] == json!(name))
            .unwrap_or_else(|| panic!("`{name}` should be reported: {usage}"))
            .clone()
    };

    for name in ["billing", "relay"] {
        let entry = by_name(name);
        assert_eq!(entry["known"], json!(false), "{entry}");
        assert!(
            entry.get("windows").is_none(),
            "a window nobody reported must not be rendered at all: {entry}"
        );
        assert!(
            entry["detail"].as_str().is_some_and(|why| !why.is_empty()),
            "an unavailable figure says why: {entry}"
        );
    }
    assert_eq!(by_name("main")["known"], json!(true));
}

/// A figure asked for over the socket says it was asked for. Both are
/// legitimate and differently stale, so neither is reported as the other.
#[tokio::test]
async fn a_figure_asked_for_says_it_was_asked_for() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_main", "a-main"), Some("main"))
        .unwrap();
    harness
        .usage
        .record_for(None, &quota(11.0), proxenos::usage::Source::Fetch);

    let usage = harness.call("usage").await.unwrap();
    assert_eq!(usage["accounts"][0]["source"], json!("fetch"));
}

/// The CLI shows every account's figure, and never prints one account's
/// headroom under another's name.
#[tokio::test]
async fn the_rendered_usage_names_every_account() {
    let harness = Harness::start().await;
    harness
        .store
        .add(&grant("acct_spare", "a-spare"), Some("spare"))
        .unwrap();
    harness
        .store
        .add(&grant("acct_main", "a-main"), Some("main"))
        .unwrap();
    harness
        .usage
        .record_for(Some("spare"), &quota(77.0), proxenos::usage::Source::Turn);

    let rendered = render::usage(&harness.call("usage").await.unwrap());

    // The serving account has made no turn, and says so rather than borrowing
    // the pinned account's figure.
    assert!(rendered.contains("none has been made"), "{rendered}");
    assert!(rendered.contains("spare"), "{rendered}");
    assert!(rendered.contains("77% used"), "{rendered}");
    assert!(rendered.contains("rode a turn"), "{rendered}");
}
