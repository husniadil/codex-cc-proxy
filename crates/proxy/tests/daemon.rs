//! Binding, and the configuration that gates it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy::config::Config;
use codex_cc_proxy::config::Tiers;
use codex_cc_proxy::daemon::bind;
use pretty_assertions::assert_eq;

fn complete_tiers() -> Tiers {
    Tiers {
        opus: Some("gpt-5.6-terra".to_owned()),
        sonnet: Some("gpt-5.6-terra".to_owned()),
        haiku: Some("gpt-5.4-mini".to_owned()),
        fable: Some("gpt-5.4-mini".to_owned()),
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
        opus: Some("gpt-5.6-terra".to_owned()),
        sonnet: Some("gpt-5.6-terra".to_owned()),
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
        opus: Some("gpt-5.6-terra".to_owned()),
        sonnet: Some("   ".to_owned()),
        haiku: Some("gpt-5.4-mini".to_owned()),
        fable: Some("gpt-5.4-mini".to_owned()),
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
        opus: Some("gpt-5.6-terra[1m]".to_owned()),
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
        opus   = "gpt-5.6-terra"
        sonnet = "gpt-5.6-terra"
        haiku  = "gpt-5.4-mini"
        fable  = "gpt-5.4-mini"
        "#,
    )
    .expect("the documented shape should parse");

    assert_eq!(config.port, 8787);
    assert!(config.transport.websocket);
    assert!(config.transport.compression);
    assert!(config.tiers.resolve().is_ok());
}

/// The upstream is configurable, and its defaults are the shipping ones.
///
/// Three of these have a failure mode nothing else can reach. `client_version`
/// is what the backend filters the model list by, and one that is too low
/// returns an empty catalog rather than an error — indistinguishable from an
/// account with no models. The endpoints move when the backend moves, and a
/// pinned binary that cannot be repointed is a binary that has to be rebuilt.
#[test]
fn the_upstream_defaults_are_what_ships() {
    let config = Config::default();

    assert_eq!(config.upstream.client_version, "2.0.0");
    assert_eq!(
        config.upstream.endpoint,
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        config.upstream.websocket,
        "wss://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        config.upstream.catalog,
        "https://chatgpt.com/backend-api/codex/models"
    );
}

/// Each is overridable on its own, without restating the rest.
#[test]
fn an_upstream_override_leaves_the_others_alone() {
    let config: Config = toml::from_str(
        r#"
        [tiers]
        opus = "gpt-5.6-terra"
        sonnet = "gpt-5.6-terra"
        haiku = "gpt-5.6-luna"
        fable = "gpt-5.6-luna"

        [upstream]
        client_version = "3.1.0"
        "#,
    )
    .expect("a partial upstream table should parse");

    assert_eq!(config.upstream.client_version, "3.1.0");
    assert_eq!(
        config.upstream.catalog, "https://chatgpt.com/backend-api/codex/models",
        "an untouched endpoint keeps its default"
    );
}

/// The share of a window left usable is configurable, because it decides the
/// figure the client is told and therefore when compaction fires.
#[test]
fn the_effective_window_percent_is_configurable_and_defaults_to_what_ships() {
    assert_eq!(Config::default().upstream.effective_window_percent, 95.0);

    let config: Config = toml::from_str(
        r#"
        [tiers]
        opus = "gpt-5.6-terra"
        sonnet = "gpt-5.6-terra"
        haiku = "gpt-5.6-luna"
        fable = "gpt-5.6-luna"

        [upstream]
        effective_window_percent = 80.0
        "#,
    )
    .unwrap();

    assert_eq!(config.upstream.effective_window_percent, 80.0);
}

/// A percentage outside the range is refused rather than clamped.
///
/// Zero advertises a window of nothing and every turn is refused; above a
/// hundred advertises more than exists and the guard stops guarding. Both are
/// silent, and a clamp would make an operator's mistake look like it worked.
#[test]
fn an_impossible_window_percentage_is_refused() {
    for percent in ["0.0", "-5.0", "100.1", "1000.0"] {
        let raw = format!(
            r#"
            [tiers]
            opus = "gpt-5.6-terra"
            sonnet = "gpt-5.6-terra"
            haiku = "gpt-5.6-luna"
            fable = "gpt-5.6-luna"

            [upstream]
            effective_window_percent = {percent}
            "#
        );
        let config: Config = toml::from_str(&raw).unwrap();
        assert!(
            config.validate().is_err(),
            "`{percent}` should be refused, not clamped"
        );
    }
}

/// A workable percentage passes.
#[test]
fn a_usable_window_percentage_validates() {
    assert!(Config::default().validate().is_ok());
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

/// §4 — the configuration file is read from disk. A daemon that documents a
/// configuration file and then runs on defaults ignores everything the operator
/// wrote, silently.
#[test]
fn the_configuration_is_read_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        codex_cc_proxy::config::EXAMPLE,
    )
    .unwrap();

    // SAFETY-adjacent: the variable is scoped to this process, and the test is
    // the only reader.
    unsafe { std::env::set_var("CODEX_CC_PROXY_HOME", dir.path()) };
    let config = Config::load().expect("the example should load");
    unsafe { std::env::remove_var("CODEX_CC_PROXY_HOME") };

    assert_eq!(config.port, 8787);
    assert!(config.tiers.resolve().is_ok());
}

/// A missing configuration is a first run, and the message says what to write
/// and where. An error that only says "missing" leaves the reader to guess.
#[test]
fn a_missing_configuration_says_what_to_write() {
    let dir = tempfile::tempdir().unwrap();

    unsafe { std::env::set_var("CODEX_CC_PROXY_HOME", dir.path().join("nothing-here")) };
    let error = Config::load().expect_err("a missing configuration should fail");
    unsafe { std::env::remove_var("CODEX_CC_PROXY_HOME") };

    assert!(error.message.contains("[tiers]"), "{}", error.message);
    assert!(error.message.contains("haiku"), "{}", error.message);
    assert!(error.message.contains("config.toml"), "{}", error.message);
}

/// The example in the error message is itself valid. An example that does not
/// parse is worse than none.
#[test]
fn the_example_configuration_is_valid() {
    let config: Config =
        toml::from_str(codex_cc_proxy::config::EXAMPLE).expect("the example should parse");
    assert!(config.tiers.resolve().is_ok());
}

/// A mistyped ceiling is refused rather than ignored. An operator who wrote it
/// meant to cap their spending, and quietly dropping it spends at full rate.
#[test]
fn an_unrecognized_effort_is_refused() {
    let config: Config = toml::from_str(
        r#"
        effort = "cheap"
        [tiers]
        opus = "m"
        sonnet = "m"
        haiku = "m"
        fable = "m"
        "#,
    )
    .unwrap();

    let error = config
        .effort_ceiling()
        .expect_err("an unknown effort should fail");
    assert!(error.message.contains("cheap"), "{}", error.message);
    assert!(
        error.message.contains("low"),
        "the error should list the valid values"
    );
}

#[test]
fn a_recognized_effort_parses() {
    let config: Config = toml::from_str(
        r#"
        effort = "low"
        [tiers]
        opus = "m"
        sonnet = "m"
        haiku = "m"
        fable = "m"
        "#,
    )
    .unwrap();

    assert_eq!(
        config.effort_ceiling().unwrap(),
        Some(codex_cc_proxy_core::responses::Effort::Low)
    );
}

/// No ceiling means the backend's own default, not zero effort.
#[test]
fn no_effort_key_means_no_ceiling() {
    let config: Config = toml::from_str(codex_cc_proxy::config::EXAMPLE).unwrap();
    assert_eq!(config.effort_ceiling().unwrap(), None);
}
