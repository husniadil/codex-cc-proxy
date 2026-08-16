//! Binding, and the configuration that gates it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy::config::Config;
use codex_cc_proxy::config::Tiers;
use codex_cc_proxy::daemon::bind;
use pretty_assertions::assert_eq;

fn complete_tiers() -> Tiers {
    Tiers {
        opus: Some("gpt-5-codex".to_owned()),
        sonnet: Some("gpt-5-codex".to_owned()),
        haiku: Some("gpt-5-codex-mini".to_owned()),
        fable: Some("gpt-5-codex-mini".to_owned()),
    }
}

/// A second daemon on another port is silently unused by a client already
/// configured for the first, so the conflict is named rather than worked
/// around.
#[tokio::test]
async fn a_taken_port_fails_with_the_conflict_named() {
    let first = bind(0).await.expect("an ephemeral port should bind");
    let port = first.local_addr().unwrap().port();

    let error = bind(port).await.expect_err("the second bind should fail");

    assert!(
        error.message.contains("already in use"),
        "the message should name the conflict: {}",
        error.message
    );
    assert!(
        error.message.contains(&port.to_string()),
        "the message should name the port: {}",
        error.message
    );
}

/// Loopback only. Every caller reaching the socket is already a local process
/// running as the user, which is what makes serving without authentication
/// safe.
#[tokio::test]
async fn the_daemon_binds_loopback() {
    let listener = bind(0).await.unwrap();
    assert!(listener.local_addr().unwrap().ip().is_loopback());
}

/// §7.1 — an incomplete mapping refuses startup, naming what is missing.
#[test]
fn an_incomplete_tier_mapping_refuses_to_start() {
    let tiers = Tiers {
        opus: Some("gpt-5-codex".to_owned()),
        sonnet: Some("gpt-5-codex".to_owned()),
        haiku: None,
        fable: None,
    };

    let error = tiers
        .resolve()
        .expect_err("an incomplete mapping should fail");
    assert!(error.message.contains("haiku"), "{}", error.message);
    assert!(error.message.contains("fable"), "{}", error.message);
}

/// A tier mapped to an empty string is not mapped. Treating it as present sends
/// an empty model id upstream and fails on the first request instead of at
/// startup.
#[test]
fn a_blank_tier_counts_as_missing() {
    let tiers = Tiers {
        opus: Some("gpt-5-codex".to_owned()),
        sonnet: Some("   ".to_owned()),
        haiku: Some("gpt-5-codex-mini".to_owned()),
        fable: Some("gpt-5-codex-mini".to_owned()),
    };

    let error = tiers.resolve().expect_err("a blank tier should fail");
    assert!(error.message.contains("sonnet"), "{}", error.message);
}

#[test]
fn a_complete_mapping_resolves_all_four() {
    let resolved = complete_tiers().resolve().expect("a complete mapping");
    let names: Vec<&str> = resolved.iter().map(|tier| tier.tier).collect();
    assert_eq!(names, vec!["opus", "sonnet", "haiku", "fable"]);
}

/// §7.2 — a `[1m]` marker makes the client assume a million-token window, which
/// is roughly four times the real one, and auto-compaction would never fire
/// before the window overran. Late compaction fails the session; early
/// compaction only wastes context.
#[test]
fn a_model_id_carrying_a_1m_marker_is_rejected() {
    let tiers = Tiers {
        opus: Some("gpt-5-codex[1m]".to_owned()),
        ..complete_tiers()
    };

    let error = tiers
        .resolve()
        .expect_err("a [1m] marker should be rejected");
    assert!(error.message.contains("[1m]"), "{}", error.message);
    assert!(error.message.contains("opus"), "{}", error.message);
}

/// The configuration file is TOML, and its defaults are the documented ones.
#[test]
fn configuration_parses_with_documented_defaults() {
    let config: Config = toml::from_str(
        r#"
        [tiers]
        opus   = "gpt-5-codex"
        sonnet = "gpt-5-codex"
        haiku  = "gpt-5-codex-mini"
        fable  = "gpt-5-codex-mini"
        "#,
    )
    .expect("the documented shape should parse");

    assert_eq!(config.port, 8787);
    assert!(config.transport.websocket);
    assert!(config.transport.compression);
    assert!(config.tiers.resolve().is_ok());
}

/// Credentials are never stored in the configuration file. A key that looks
/// like one is not a field, so it cannot be read even if someone writes it.
#[test]
fn configuration_has_no_credential_fields() {
    let rendered = toml::to_string(&Config::default()).unwrap();
    for forbidden in ["token", "secret", "password", "refresh"] {
        assert!(
            !rendered.contains(forbidden),
            "the config shape should carry no {forbidden} field"
        );
    }
}
