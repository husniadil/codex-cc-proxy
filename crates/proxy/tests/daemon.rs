//! Binding, and the configuration that gates it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use pretty_assertions::assert_eq;
use proxenos::config::Config;
use proxenos::config::Tiers;
use proxenos::daemon::bind;

fn complete_tiers() -> Tiers {
    Tiers {
        opus: Some("gpt-5.6-terra".into()),
        sonnet: Some("gpt-5.6-terra".into()),
        haiku: Some("gpt-5.4-mini".into()),
        fable: Some("gpt-5.4-mini".into()),
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
        opus: Some("gpt-5.5".into()),
        sonnet: Some("gpt-5.5".into()),
        haiku: None,
        fable: None,
    };

    let resolved = tiers
        .resolve(proxenos::config::CrossAccountTiers::Refused)
        .expect("the omitted tiers default");
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
        opus: Some("gpt-5.6-terra".into()),
        sonnet: Some("   ".into()),
        haiku: Some("gpt-5.4-mini".into()),
        fable: Some("gpt-5.4-mini".into()),
    };

    let error = tiers
        .resolve(proxenos::config::CrossAccountTiers::Refused)
        .expect_err("a blank tier should fail");
    assert!(error.message.contains("sonnet"), "{}", error.message);
}

#[test]
fn a_complete_mapping_resolves_all_four() {
    let resolved = complete_tiers()
        .resolve(proxenos::config::CrossAccountTiers::Refused)
        .expect("a complete mapping");
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
        opus: Some("gpt-5.6-terra[1m]".into()),
        ..complete_tiers()
    };

    let error = tiers
        .resolve(proxenos::config::CrossAccountTiers::Refused)
        .expect_err("a [1m] marker should be rejected");
    assert!(error.message.contains("[1m]"), "{}", error.message);
    assert!(error.message.contains("opus"), "{}", error.message);
}

/// A tier may name another account: `haiku = { account = "...", model = "..." }`.
/// The bare-string form keeps its meaning — the serving account — so every
/// existing configuration reads unchanged.
#[test]
fn a_tier_names_another_account_in_table_form() {
    let config: Config = toml::from_str(
        r#"
        [tiers]
        opus   = "gpt-5.6-terra"
        sonnet = "gpt-5.6-terra"
        haiku  = { account = "spare", model = "gpt-5.4-mini" }
        fable  = "gpt-5.4-mini"
        "#,
    )
    .expect("both forms parse");

    let resolved = config
        .tiers
        .resolve(proxenos::config::CrossAccountTiers::Permitted)
        .expect("a permitted cross-account mapping resolves");

    let haiku = resolved.iter().find(|tier| tier.tier == "haiku").unwrap();
    assert_eq!(haiku.model, "gpt-5.4-mini");
    assert_eq!(haiku.account.as_deref(), Some("spare"));

    let opus = resolved.iter().find(|tier| tier.tier == "opus").unwrap();
    assert_eq!(
        opus.account, None,
        "a bare string stays on the serving account"
    );
}

/// Routing one client's traffic across accounts spends another account's quota,
/// which is a decision the operator must own. Absent the consent key, the
/// mapping refuses — naming the key — rather than falling back to the serving
/// account, which would spend the wrong quota invisibly.
#[test]
fn a_cross_account_tier_without_consent_is_refused() {
    let config: Config = toml::from_str(
        r#"
        [tiers]
        opus   = "gpt-5.6-terra"
        sonnet = "gpt-5.6-terra"
        haiku  = { account = "spare", model = "gpt-5.4-mini" }
        fable  = "gpt-5.4-mini"
        "#,
    )
    .unwrap();

    let error = config
        .tiers
        .resolve(proxenos::config::CrossAccountTiers::Refused)
        .expect_err("consent is required");
    assert!(
        error.message.contains("cross_account_tiers"),
        "the refusal names the key that grants consent: {}",
        error.message
    );
    assert!(
        error.message.contains("haiku") && error.message.contains("spare"),
        "and the entry that needs it: {}",
        error.message
    );
}

/// The consent key parses, and the policy it produces is the one the daemon
/// hands to every resolve.
#[test]
fn the_consent_key_permits_what_its_absence_refuses() {
    let with = r#"
        cross_account_tiers = true
        [tiers]
        haiku = { account = "spare", model = "gpt-5.4-mini" }
    "#;
    let without = r#"
        [tiers]
        haiku = { account = "spare", model = "gpt-5.4-mini" }
    "#;

    let config: Config = toml::from_str(with).unwrap();
    assert!(config.tiers.resolve(config.cross_account_policy()).is_ok());

    let config: Config = toml::from_str(without).unwrap();
    assert!(config.tiers.resolve(config.cross_account_policy()).is_err());
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
    assert!(
        config
            .tiers
            .resolve(proxenos::config::CrossAccountTiers::Refused)
            .is_ok()
    );
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
    std::fs::write(dir.path().join("config.toml"), proxenos::config::EXAMPLE).unwrap();

    // SAFETY-adjacent: the variable is scoped to this process, and the test is
    // the only reader.
    unsafe { std::env::set_var("PROXENOS_HOME", dir.path()) };
    let config = Config::load().expect("the example should load");
    unsafe { std::env::remove_var("PROXENOS_HOME") };

    assert_eq!(config.port, 8787);
    assert!(
        config
            .tiers
            .resolve(proxenos::config::CrossAccountTiers::Refused)
            .is_ok()
    );
}

/// A missing configuration is a first run, and a first run works.
///
/// Every key has a default, so there is nothing the operator must state before
/// the daemon can serve a request. Refusing to start would be demanding a file
/// whose entire content the daemon already knows.
#[test]
fn a_missing_configuration_falls_back_to_the_defaults() {
    let dir = tempfile::tempdir().unwrap();

    unsafe { std::env::set_var("PROXENOS_HOME", dir.path().join("nothing-here")) };
    let config = Config::load().expect("a missing configuration is a first run, not a failure");
    unsafe { std::env::remove_var("PROXENOS_HOME") };

    assert_eq!(config.port, 8787);
    assert!(
        config
            .tiers
            .resolve(proxenos::config::CrossAccountTiers::Refused)
            .is_ok()
    );
    assert!(config.instructions.working_budget);
}

/// An unreadable configuration is still an error. Falling back there would
/// start a daemon that ignores what the operator actually wrote.
#[test]
fn an_unparseable_configuration_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "port = \"not a number\"").unwrap();

    unsafe { std::env::set_var("PROXENOS_HOME", dir.path()) };
    let error = Config::load().expect_err("a malformed configuration should fail");
    unsafe { std::env::remove_var("PROXENOS_HOME") };

    assert!(error.message.contains("config.toml"), "{}", error.message);
}

/// The example in the error message is itself valid. An example that does not
/// parse is worse than none.
#[test]
fn the_example_configuration_is_valid() {
    let config: Config =
        toml::from_str(proxenos::config::EXAMPLE).expect("the example should parse");
    assert!(
        config
            .tiers
            .resolve(proxenos::config::CrossAccountTiers::Refused)
            .is_ok()
    );
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
        Some(proxenos_core::responses::Effort::Low)
    );
}

/// No ceiling means the backend's own default, not zero effort.
#[test]
fn no_effort_key_means_no_ceiling() {
    let config: Config = toml::from_str(proxenos::config::EXAMPLE).unwrap();
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
        .resolve(proxenos::config::CrossAccountTiers::Refused)
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
    assert!(
        config
            .tiers
            .resolve(proxenos::config::CrossAccountTiers::Refused)
            .is_ok()
    );
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

    let resolved = config
        .tiers
        .resolve(proxenos::config::CrossAccountTiers::Refused)
        .unwrap();
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
        .resolve(proxenos::config::CrossAccountTiers::Refused)
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
    assert_eq!(
        on.client.effective_deny_skills(true),
        vec!["claude-api".to_owned()]
    );
    assert!(on.client.disable_connectors);
    assert!(on.client.disable_remote_control);
    assert_eq!(
        serde_json::Value::Object(on.client.settings(true)),
        serde_json::json!({
            "permissions": { "deny": ["Skill(claude-api)"] },
            "disableClaudeAiConnectors": true,
            "remoteControlAtStartup": false,
        })
    );

    let off: Config = toml::from_str(
        r#"
        [client]
        deny_skills = []
        disable_connectors = false
        disable_remote_control = false
        "#,
    )
    .unwrap();
    assert!(
        off.client.settings(true).is_empty(),
        "switched off should leave no keys behind"
    );
}

/// The deny default is resolved per launch: it protects a translated session
/// from a reference that documents the second provider's API, and a session
/// whose every turn is relayed is served by that very provider.
///
/// A written list is the operator's own rule and applies on either path — the
/// default is the only thing that moves.
#[test]
fn the_deny_default_applies_only_to_a_translating_launch() {
    let unset = Config::default();
    assert_eq!(
        unset.client.effective_deny_skills(false),
        Vec::<String>::new()
    );
    assert!(
        unset.client.settings(false).get("permissions").is_none(),
        "an all-relay launch has no default deny to carry"
    );

    let written: Config = toml::from_str(
        r#"
        [client]
        deny_skills = ["claude-api"]
        "#,
    )
    .unwrap();
    assert_eq!(
        written.client.settings(false)["permissions"]["deny"],
        serde_json::json!(["Skill(claude-api)"]),
        "a written list is the operator's rule on either path"
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
        disable_remote_control = false
        "#,
    )
    .unwrap();
    assert_eq!(
        serde_json::Value::Object(skills_only.client.settings(true)),
        serde_json::json!({ "permissions": { "deny": ["Skill(claude-api)"] } })
    );

    let connectors_only: Config = toml::from_str(
        r#"
        [client]
        deny_skills = []
        disable_remote_control = false
        "#,
    )
    .unwrap();
    assert_eq!(
        serde_json::Value::Object(connectors_only.client.settings(true)),
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
        config.client.settings(true)["permissions"]["deny"],
        serde_json::json!(["Skill(claude-api)", "Skill(some-other-skill)"])
    );
}

/// A per-account section overrides only the tiers it names.
///
/// Two accounts on different plans are offered different models, so one mapping
/// cannot be right for both — and with a key account beside a subscription the
/// two menus need not overlap at all. What an account does not state falls
/// through to the shared table, because the common case is one or two tiers
/// differing rather than a whole second mapping.
#[test]
fn an_account_section_overrides_only_the_tiers_it_names() {
    let config: Config = toml::from_str(
        r#"
        [tiers]
        opus   = "shared-opus"
        sonnet = "shared-sonnet"
        haiku  = "shared-haiku"
        fable  = "shared-fable"

        [accounts.api.tiers]
        opus = "key-opus"
        "#,
    )
    .unwrap();

    let model = |account: Option<&str>, tier: &str| {
        config
            .tiers_for(account)
            .resolve(proxenos::config::CrossAccountTiers::Refused)
            .unwrap()
            .into_iter()
            .find(|resolved| resolved.tier == tier)
            .unwrap()
            .model
    };

    assert_eq!(model(Some("api"), "opus"), "key-opus");
    assert_eq!(model(Some("api"), "sonnet"), "shared-sonnet");
    assert_eq!(model(Some("work"), "opus"), "shared-opus");
    assert_eq!(model(None, "opus"), "shared-opus");
}

/// The effort ceiling follows the same rule.
///
/// Which efforts a model offers is a property of the catalog, and the catalog
/// is one account's menu — so a ceiling that is right for one account can name
/// an effort another account's model does not accept.
#[test]
fn an_account_section_overrides_the_effort_ceiling() {
    let config: Config = toml::from_str(
        r#"
        effort = "high"

        [accounts.api]
        effort = "low"
        "#,
    )
    .unwrap();

    use proxenos_core::responses::Effort;
    assert_eq!(
        config.effort_ceiling_for(Some("api")).unwrap(),
        Some(Effort::Low)
    );
    assert_eq!(
        config.effort_ceiling_for(Some("work")).unwrap(),
        Some(Effort::High)
    );
    assert_eq!(config.effort_ceiling_for(None).unwrap(), Some(Effort::High));
}

/// A misspelled key inside an account section is refused, for the reason every
/// other unknown key is: one that does nothing is worse than one that is
/// refused, because only one of them says so.
#[test]
fn an_unknown_key_in_an_account_section_is_refused() {
    let error = toml::from_str::<Config>(
        r#"
        [accounts.api]
        efort = "low"
        "#,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("efort"), "{error}");
}

/// The shipped example states exactly the defaults it documents.
///
/// Two things make this load-bearing rather than tidy. The example calls itself
/// "the defaults, shown so they can be changed", so a value that has drifted is
/// a lie in the one document an operator reads first. And a persisted change
/// made before any file exists is written *into* the example, so every key it
/// states that no longer matches the compiled default would be pinned to the
/// old value — silently changing tiers the operator never asked about, at the
/// next start rather than now.
///
/// Compared as effective values, because the example states its tiers and a
/// default `Tiers` leaves them unstated: the two shapes differ and the mappings
/// must not.
#[test]
fn the_example_states_the_defaults_it_documents() {
    let example: Config =
        toml::from_str(proxenos::config::EXAMPLE).expect("the example should parse");
    let defaults = Config::default();

    // Tier and model only. `defaulted` differs by construction and says so
    // truthfully: the example states its mapping, a bare default does not.
    let mapping = |config: &Config| {
        config
            .tiers
            .resolve(proxenos::config::CrossAccountTiers::Refused)
            .unwrap()
            .into_iter()
            .map(|tier| (tier.tier, tier.model))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        mapping(&example),
        mapping(&defaults),
        "the example's tier mapping is not the compiled default"
    );
    assert_eq!(example.port, defaults.port);
    assert_eq!(example.effort_ceiling().unwrap(), None);
    assert_eq!(example.transport.websocket, defaults.transport.websocket);
    assert_eq!(
        example.transport.compression,
        defaults.transport.compression
    );
    assert_eq!(
        example.instructions.identity,
        defaults.instructions.identity
    );
    assert_eq!(
        example.instructions.working_budget,
        defaults.instructions.working_budget
    );
    assert_eq!(example.instructions.append, defaults.instructions.append);
    assert_eq!(example.client.deny_skills, defaults.client.deny_skills);
    assert_eq!(
        example.client.disable_connectors,
        defaults.client.disable_connectors
    );
    assert_eq!(example.upstream.endpoint, defaults.upstream.endpoint);
    assert_eq!(example.upstream.websocket, defaults.upstream.websocket);
    assert_eq!(example.upstream.catalog, defaults.upstream.catalog);
    assert_eq!(example.upstream.usage, defaults.upstream.usage);
    assert_eq!(
        example.upstream.key.endpoint,
        defaults.upstream.key.endpoint
    );
    assert_eq!(example.upstream.key.catalog, defaults.upstream.key.catalog);
    assert_eq!(
        example.upstream.client_version,
        defaults.upstream.client_version
    );
    assert_eq!(
        example.upstream.effective_window_percent,
        defaults.upstream.effective_window_percent
    );
    assert!(
        example.accounts.is_empty(),
        "the example must not ship an account section: it would be written into \
         a first persisted change as though the operator had asked for it"
    );
}
