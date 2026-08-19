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

/// §7.1 — an omitted tier takes its default rather than refusing to start.
///
/// This reverses an earlier rule that required all four explicitly. The reason
/// for that rule was that a defaulted mapping hides which model serves the
/// cheap background traffic; `status` prints the mapping in use either way,
/// which answers it without making the first run fail on a file nobody had
/// written yet.
#[test]
fn an_omitted_tier_takes_its_default() {
    let tiers = Tiers {
        opus: Some("gpt-5.5".to_owned()),
        sonnet: Some("gpt-5.5".to_owned()),
        haiku: None,
        fable: None,
    };

    let resolved = tiers.resolve().expect("the omitted tiers default");
    let model = |tier: &str| {
        resolved
            .iter()
            .find(|entry| entry.tier == tier)
            .map(|entry| entry.model.as_str())
            .unwrap()
    };

    assert_eq!(model("opus"), "gpt-5.5");
    assert_eq!(model("haiku"), "gpt-5.6-luna");
    assert_eq!(model("fable"), "gpt-5.6-sol");
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

/// A missing configuration is a first run, and a first run works.
///
/// Every key has a default, so there is nothing the operator must state before
/// the daemon can serve a request. Refusing to start would be demanding a file
/// whose entire content the daemon already knows.
#[test]
fn a_missing_configuration_falls_back_to_the_defaults() {
    let dir = tempfile::tempdir().unwrap();

    unsafe { std::env::set_var("CODEX_CC_PROXY_HOME", dir.path().join("nothing-here")) };
    let config = Config::load().expect("a missing configuration is a first run, not a failure");
    unsafe { std::env::remove_var("CODEX_CC_PROXY_HOME") };

    assert_eq!(config.port, 8787);
    assert!(config.tiers.resolve().is_ok());
    assert!(config.instructions.working_budget);
}

/// An unreadable configuration is still an error. Falling back there would
/// start a daemon that ignores what the operator actually wrote.
#[test]
fn an_unparseable_configuration_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "port = \"not a number\"").unwrap();

    unsafe { std::env::set_var("CODEX_CC_PROXY_HOME", dir.path()) };
    let error = Config::load().expect_err("a malformed configuration should fail");
    unsafe { std::env::remove_var("CODEX_CC_PROXY_HOME") };

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

// ---------------------------------------------------------------------------
// Opinionated defaults. A configuration that states nothing should still be a
// working configuration, and the shipped answers are the ones the README gives.
// ---------------------------------------------------------------------------

/// All four tiers have defaults, and they are the mapping the README states.
///
/// Requiring them explicitly made an unmapped haiku impossible — and also made
/// the first run fail on a file the operator had not written yet. A default
/// mapping is visible in `status` and overridable in one line, which answers
/// the original concern without the cost.
#[test]
fn every_tier_has_the_default_the_readme_states() {
    let resolved = Tiers::default()
        .resolve()
        .expect("the defaults should resolve on their own");

    let model = |tier: &str| {
        resolved
            .iter()
            .find(|entry| entry.tier == tier)
            .map(|entry| entry.model.as_str())
            .expect("every tier is mapped")
    };

    assert_eq!(model("opus"), "gpt-5.6-terra");
    assert_eq!(model("sonnet"), "gpt-5.6-luna");
    assert_eq!(model("haiku"), "gpt-5.6-luna");
    assert_eq!(model("fable"), "gpt-5.6-sol");
}

/// A configuration stating nothing at all is a working configuration.
#[test]
fn an_empty_configuration_resolves() {
    let config: Config = toml::from_str("").expect("an empty configuration should parse");
    assert!(config.tiers.resolve().is_ok());
    assert!(config.validate().is_ok());
}

/// One stated tier overrides its default and leaves the rest alone.
#[test]
fn a_stated_tier_overrides_only_itself() {
    let config: Config = toml::from_str(
        r#"
        [tiers]
        opus = "gpt-5.5"
        "#,
    )
    .unwrap();

    let resolved = config.tiers.resolve().unwrap();
    let model = |tier: &str| {
        resolved
            .iter()
            .find(|entry| entry.tier == tier)
            .map(|entry| entry.model.as_str())
            .unwrap()
    };

    assert_eq!(model("opus"), "gpt-5.5");
    assert_eq!(
        model("haiku"),
        "gpt-5.6-luna",
        "the rest keep their defaults"
    );
}

/// A tier written as blank is still refused. Defaulting a *missing* value is
/// helpful; silently replacing something the operator wrote is not — a blank is
/// a mistake, and an id sent empty fails on the first request instead of here.
#[test]
fn a_blank_tier_is_still_refused_rather_than_defaulted() {
    let config: Config = toml::from_str(
        r#"
        [tiers]
        sonnet = "   "
        "#,
    )
    .unwrap();

    let error = config
        .tiers
        .resolve()
        .expect_err("a blank tier should still fail");
    assert!(error.message.contains("sonnet"), "{}", error.message);
}

/// The working budget ships on, because without it the model reads broadly and
/// spends the window fast. It is switchable, and off leaves nothing behind.
#[test]
fn the_working_budget_ships_on_and_can_be_switched_off() {
    let on = Config::default();
    let budget = on
        .instructions
        .budget()
        .expect("the budget should be on by default");
    assert!(budget.contains("smallest"), "{budget}");

    let off: Config = toml::from_str(
        r#"
        [instructions]
        working_budget = false
        "#,
    )
    .unwrap();
    assert_eq!(off.instructions.budget(), None);
}

/// The bundled `claude-api` skill ships denied.
///
/// Measured here: one invocation lands 73,000 to 93,000 bytes — roughly 18,000
/// to 23,000 tokens — in the conversation as a user text block, where it then
/// sits for the rest of the session and is charged on every turn. A range
/// because both ends were measured; the figure moves with what else the session
/// has loaded, so it is not a constant. A refused call costs a 43-byte
/// error instead. The deny does not remove the skill from the listing the
/// client sends, so the model may still reach for it; what it stops is the
/// load.
///
/// Opinionated by default for the same reason the working budget is, and
/// switchable in the same place, because an operator building against that API
/// wants the reference the rest of us are paying to avoid.
#[test]
fn the_client_policy_ships_on_and_can_be_switched_off() {
    let on = Config::default();
    assert_eq!(on.client.deny_skills, vec!["claude-api".to_owned()]);
    assert!(on.client.disable_connectors);
    assert_eq!(
        serde_json::Value::Object(on.client.settings()),
        serde_json::json!({
            "permissions": { "deny": ["Skill(claude-api)"] },
            "disableClaudeAiConnectors": true,
        })
    );

    let off: Config = toml::from_str(
        r#"
        [client]
        deny_skills = []
        disable_connectors = false
        "#,
    )
    .unwrap();
    assert!(
        off.client.settings().is_empty(),
        "switched off should leave no keys behind"
    );
}

/// Either half can be switched off alone, and the half left on is the only one
/// that appears. A policy document carrying a key nobody asked for is a policy
/// document that is partly guessed.
#[test]
fn each_half_of_the_client_policy_stands_alone() {
    let skills_only: Config = toml::from_str(
        r#"
        [client]
        disable_connectors = false
        "#,
    )
    .unwrap();
    assert_eq!(
        serde_json::Value::Object(skills_only.client.settings()),
        serde_json::json!({ "permissions": { "deny": ["Skill(claude-api)"] } })
    );

    let connectors_only: Config = toml::from_str(
        r#"
        [client]
        deny_skills = []
        "#,
    )
    .unwrap();
    assert_eq!(
        serde_json::Value::Object(connectors_only.client.settings()),
        serde_json::json!({ "disableClaudeAiConnectors": true })
    );
}

/// A skill named in the configuration reaches the document as the client's own
/// rule shape. The operator writes the skill id; the `Skill(...)` wrapper is
/// this proxy's job, because getting it wrong fails silently — an unrecognized
/// rule denies nothing and says nothing.
#[test]
fn a_configured_skill_becomes_a_rule_the_client_understands() {
    let config: Config = toml::from_str(
        r#"
        [client]
        deny_skills = ["claude-api", "some-other-skill"]
        "#,
    )
    .unwrap();

    assert_eq!(
        config.client.settings()["permissions"]["deny"],
        serde_json::json!(["Skill(claude-api)", "Skill(some-other-skill)"])
    );
}
