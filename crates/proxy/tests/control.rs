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
use std::sync::atomic::AtomicBool;

fn tiers() -> Vec<ResolvedTier> {
    vec![
        ResolvedTier {
            tier: "opus",
            model: "gpt-5-codex".to_owned(),
        },
        ResolvedTier {
            tier: "sonnet",
            model: "gpt-5-codex".to_owned(),
        },
        ResolvedTier {
            tier: "haiku",
            model: "gpt-5-codex-mini".to_owned(),
        },
        ResolvedTier {
            tier: "fable",
            model: "gpt-5-codex-mini".to_owned(),
        },
    ]
}

struct Harness {
    path: std::path::PathBuf,
    store: Arc<FileStore>,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let store = Arc::new(FileStore::new(dir.path().join("credentials.json")));

        let state = ControlState {
            port: 8787,
            tiers: Arc::new(tiers()),
            catalog: Arc::new(
                Catalog::parse(
                    r#"{"data":[{"id":"gpt-5-codex","context_window":272000},
                                {"id":"gpt-5-codex-mini","context_window":200000}]}"#,
                )
                .unwrap(),
            ),
            credentials: Arc::clone(&store) as Arc<dyn CredentialStore>,
            recording: Arc::new(AtomicBool::new(false)),
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
    assert_eq!(status["tiers"]["haiku"], json!("gpt-5-codex-mini"));
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
    assert_eq!(lookup("ANTHROPIC_DEFAULT_HAIKU_MODEL"), "gpt-5-codex-mini");
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
    assert_eq!(result["models"][0]["id"], json!("gpt-5-codex"));
    assert_eq!(result["models"][0]["context_window"], json!(272_000));
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
        recording: Arc::new(AtomicBool::new(false)),
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
    assert_eq!(
        harness.call("status").await.unwrap()["recording"],
        json!(true)
    );

    harness.call("record.stop").await.unwrap();
    assert_eq!(
        harness.call("status").await.unwrap()["recording"],
        json!(false)
    );
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
        recording: Arc::new(AtomicBool::new(false)),
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
    assert!(rendered.contains("export ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-5-codex-mini"));
    assert!(rendered.contains("export CLAUDE_CODE_DISABLE_1M_CONTEXT=1"));
}

#[tokio::test]
async fn env_renders_as_a_settings_fragment() {
    let harness = Harness::start().await;
    let result = harness.call("env").await.unwrap();

    let parsed: Value = serde_json::from_str(&render::env_json(&result)).unwrap();

    assert_eq!(
        parsed["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"],
        json!("gpt-5-codex-mini")
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
        recording: Arc::new(AtomicBool::new(false)),
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
        recording: Arc::new(AtomicBool::new(false)),
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
