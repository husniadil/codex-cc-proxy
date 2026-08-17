//! `docs/api.md` §3 — the control socket.
//!
//! Driven over a real socket, because "the CLI holds no state of its own" is
//! only true if every verb genuinely goes through this interface.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy::auth::store::CredentialStore;
use codex_cc_proxy::auth::store::Credentials;
use codex_cc_proxy::auth::store::FileStore;
use codex_cc_proxy::catalog::Catalog;
use codex_cc_proxy::config::ResolvedTier;
use codex_cc_proxy::control;
use codex_cc_proxy::control::handler::ControlState;
use codex_cc_proxy::control::protocol::METHODS;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

fn tiers() -> Vec<ResolvedTier> {
    vec![
        ResolvedTier {
            tier: "opus",
            model: "gpt-5.6-terra".to_owned(),
        },
        ResolvedTier {
            tier: "sonnet",
            model: "gpt-5.6-terra".to_owned(),
        },
        ResolvedTier {
            tier: "haiku",
            model: "gpt-5.4-mini".to_owned(),
        },
        ResolvedTier {
            tier: "fable",
            model: "gpt-5.4-mini".to_owned(),
        },
    ]
}

struct Harness {
    path: std::path::PathBuf,
    store: Arc<FileStore>,
    /// The same switches the ingress path would read. Asserting on these is
    /// the difference between testing that a flag round-trips and testing that
    /// the method does anything.
    switches: Arc<codex_cc_proxy::recorder::Switches>,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));
        let switches = Arc::new(codex_cc_proxy::recorder::Switches::default());
        let usage = Arc::new(codex_cc_proxy::usage::UsageStore::default());

        let state = ControlState {
            port: 8787,
            tiers: Arc::new(tiers()),
            catalog: Arc::new(
                Catalog::parse(
                    r#"{"data":[{"id":"gpt-5.6-terra","context_window":272000},
                                {"id":"gpt-5.4-mini","context_window":200000}]}"#,
                )
                .unwrap(),
            ),
            credentials: Arc::clone(&store) as Arc<dyn CredentialStore>,
            capture: Arc::clone(&switches),
            usage: Arc::clone(&usage),
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
            store,
            switches,
            _dir: dir,
        }
    }

    async fn call(&self, method: &str) -> Result<Value, codex_cc_proxy::error::ProxyError> {
        control::call(&self.path, method, None).await
    }
}

/// Every documented method answers over the socket. A method in the vocabulary
/// that the daemon does not know is a contract this project has already
/// published and cannot honour.
#[tokio::test]
async fn every_documented_method_is_answered() {
    let harness = Harness::start().await;

    for method in METHODS {
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

/// `disconnect` clears credentials, and is safe to run twice.
#[tokio::test]
async fn disconnect_clears_credentials() {
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

    harness.call("disconnect").await.unwrap();
    assert!(harness.store.load().unwrap().is_none());

    harness.call("disconnect").await.unwrap();
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
        tiers: Arc::new(tiers()),
        catalog: Arc::new(Catalog::fallback()),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
    };

    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "models" }).to_string(),
    );
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
        tiers: Arc::new(tiers()),
        catalog: Arc::new(Catalog::fallback()),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
    };

    let response = control::answer(&state, "{ not json");
    assert_eq!(response.error.map(|error| error.code), Some(-32700));

    // The next request on the same connection still works.
    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "status" }).to_string(),
    );
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

use codex_cc_proxy::render;

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

    let parsed: Value = serde_json::from_str(&render::env_json(&result)).unwrap();

    assert_eq!(
        parsed["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"],
        json!("gpt-5.4-mini")
    );
    assert_eq!(parsed["env"]["CLAUDE_CODE_DISABLE_1M_CONTEXT"], json!("1"));
}

/// A reader must be able to tell an unvalidated mapping from a validated one.
#[tokio::test]
async fn status_says_when_the_catalog_was_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let state = ControlState {
        port: 8787,
        tiers: Arc::new(tiers()),
        catalog: Arc::new(Catalog::fallback()),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
    };

    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "status" }).to_string(),
    );
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
        tiers: Arc::new(tiers()),
        catalog: Arc::new(Catalog::fallback()),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
    };

    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "models" }).to_string(),
    );
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
        tiers: Arc::new(tiers()),
        catalog: Arc::new(Catalog::fallback()),
        credentials: Arc::new(FileStore::new(dir.path().join("c.json"))),
        capture: Arc::new(codex_cc_proxy::recorder::Switches::default()),
        usage: Arc::new(codex_cc_proxy::usage::UsageStore::default()),
    };

    let response = control::answer(
        &state,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "env" }).to_string(),
    );
    let rendered = render::env_shell(&response.result.unwrap());

    assert!(
        !rendered.contains("CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
        "{rendered}"
    );
    // The one-sided floor is still set.
    assert!(rendered.contains("CLAUDE_CODE_DISABLE_1M_CONTEXT=1"));
}
