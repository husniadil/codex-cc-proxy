//! `docs/proxy-behavior.md` §7.0 — the model catalog.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy::catalog::Catalog;
use pretty_assertions::assert_eq;

/// The share the shipping configuration applies. Stated here rather than
/// imported so that a change to the default has to be made deliberately in two
/// places, one of which is a test that says what the figure means.
const SHIPPING: f64 = 95.0;

const SAMPLE: &str = r#"{
  "data": [
    {
      "id": "gpt-5.6-terra",
      "context_window": 272000,
      "max_context_window": 400000,
      "effective_context_window_percent": 95.0,
      "is_visible": true
    },
    {
      "id": "gpt-5.4-mini",
      "context_window": 200000,
      "is_visible": true
    },
    {
      "id": "internal-preview",
      "context_window": 100000,
      "is_visible": false
    },
    { "id": "windowless" }
  ]
}"#;

/// The shape the backend actually returns: `slug` rather than `id`,
/// `visibility` as a word rather than a boolean, and reasoning levels as
/// objects.
const LIVE_SHAPE: &str = r#"{
  "models": [
    {
      "slug": "gpt-5.6-luna",
      "context_window": 272000,
      "max_context_window": 272000,
      "visibility": "list",
      "supported_reasoning_levels": [
        { "effort": "low", "description": "Fast responses with lighter reasoning" },
        { "effort": "medium", "description": "Balances speed and reasoning depth" }
      ]
    },
    {
      "slug": "codex-auto-review",
      "context_window": 272000,
      "visibility": "hide"
    }
  ]
}"#;

/// §7.0 — visibility arrives as a word, not a boolean.
///
/// Reading it as a boolean field that is never present made every entry look
/// visible, including the ones explicitly marked hidden — so a model the
/// backend withholds was offered for mapping.
#[test]
fn a_hidden_model_is_withheld_however_visibility_is_spelled() {
    let catalog = Catalog::parse(LIVE_SHAPE, SHIPPING).expect("the live shape should parse");

    let offered: Vec<&str> = catalog
        .selectable()
        .iter()
        .map(|model| model.id.as_str())
        .collect();

    assert_eq!(offered, vec!["gpt-5.6-luna"]);
    // Withheld from selection, still known.
    assert!(catalog.get("codex-auto-review").is_some());
}

/// The efforts a model accepts are read from the catalog, so a ceiling naming
/// one it does not support can be recognized rather than sent and rejected.
#[test]
fn supported_efforts_are_read_from_the_catalog() {
    let catalog = Catalog::parse(LIVE_SHAPE, SHIPPING).unwrap();
    let luna = catalog.get("gpt-5.6-luna").unwrap();

    assert_eq!(luna.efforts, vec!["low", "medium"]);
}

/// An entry keyed by `slug` is the same as one keyed by `id`.
#[test]
fn the_live_shape_yields_real_windows() {
    let catalog = Catalog::parse(LIVE_SHAPE, SHIPPING).unwrap();
    let luna = catalog.get("gpt-5.6-luna").unwrap();

    assert_eq!(luna.context_window, Some(272_000));
    // No stated percentage, so the default applies rather than the whole window.
    assert_eq!(luna.effective_window(), Some(258_400));
}

#[test]
fn the_catalog_parses_ids_and_windows() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).expect("the catalog should parse");

    assert!(catalog.authoritative);
    assert_eq!(
        catalog.get("gpt-5.6-terra").and_then(|m| m.context_window),
        Some(272_000)
    );
}

/// Where both windows are present the smaller-scoped one wins. The maximum
/// describes a ceiling this account may not have, so trusting it would let
/// requests through that the account cannot actually serve.
#[test]
fn the_smaller_scoped_window_is_authoritative() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();
    let model = catalog.get("gpt-5.6-terra").unwrap();

    assert_eq!(model.context_window, Some(272_000));
    assert_ne!(model.context_window, Some(400_000));
}

/// The effective window reserves headroom for instructions, tool overhead, and
/// output.
#[test]
fn the_effective_window_applies_the_percentage() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();

    assert_eq!(
        catalog.get("gpt-5.6-terra").unwrap().effective_window(),
        Some(258_400)
    );
    // Stating no percentage means the default applies, not that all of the
    // window is usable.
    assert_eq!(
        catalog.get("gpt-5.4-mini").unwrap().effective_window(),
        Some(190_000)
    );
}

/// A model with no stated window is unknown, not assumed. A guessed window
/// either rejects requests that would have worked or forwards ones that cannot,
/// and both are worse than declining to guess.
#[test]
fn a_model_with_no_window_is_unknown_rather_than_assumed() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();
    let model = catalog
        .get("windowless")
        .expect("it should still be listed");

    assert_eq!(model.context_window, None);
    assert_eq!(model.effective_window(), None);
}

/// Hidden entries are not offered for mapping, but their metadata is kept: a
/// session may reference a model the picker filters out, and knowing its window
/// is better than not.
#[test]
fn hidden_models_are_withheld_from_selection_but_still_known() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();

    assert!(
        !catalog
            .selectable()
            .iter()
            .any(|model| model.id == "internal-preview")
    );

    assert_eq!(
        catalog
            .get("internal-preview")
            .and_then(|m| m.context_window),
        Some(100_000),
        "its window should still be known"
    );
}

#[test]
fn a_mapping_onto_known_models_validates() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();
    assert!(catalog.validate(&["gpt-5.6-terra".to_owned()]).is_ok());
}

#[test]
fn a_mapping_onto_an_unknown_model_is_rejected_and_says_what_exists() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();

    let error = catalog
        .validate(&["gpt-4-imaginary".to_owned()])
        .expect_err("an unknown model should be rejected");

    assert!(
        error.message.contains("gpt-4-imaginary"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("gpt-5.6-terra"),
        "the error should name what is available: {}",
        error.message
    );
}

/// A mapping onto a hidden model validates, and that is the problem.
///
/// `validate` asks whether the catalog knows the id, and it knows the hidden
/// ones too — so a tier mapped onto a withheld model starts cleanly and then
/// never appears in `models`. Nothing else in the system would mention it.
#[test]
fn a_tier_mapped_onto_a_withheld_model_is_reported_without_being_refused() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();
    let mapped = ["internal-preview".to_owned()];

    assert!(
        catalog.validate(&mapped).is_ok(),
        "a hidden model is known, so it is not a mapping error"
    );
    assert_eq!(catalog.unlisted(&mapped), vec!["internal-preview"]);
}

/// A mapping onto listed models has nothing to report.
#[test]
fn a_mapping_onto_listed_models_is_not_flagged() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();
    assert!(catalog.unlisted(&["gpt-5.6-terra".to_owned()]).is_empty());
}

/// A model the catalog never mentioned is not "withheld" — it is unknown, and
/// `validate` is what speaks to that. Reporting it here as well would say the
/// same thing twice in two different vocabularies.
#[test]
fn an_unknown_model_is_not_reported_as_withheld() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();
    assert!(catalog.unlisted(&["gpt-4-imaginary".to_owned()]).is_empty());
}

/// An unreachable catalog withholds nothing, because it knows nothing. Every
/// mapped model would otherwise read as hidden the moment the network blinked.
#[test]
fn an_unavailable_catalog_reports_nothing_as_withheld() {
    assert!(
        Catalog::fallback()
            .unlisted(&["gpt-5.6-terra".to_owned()])
            .is_empty()
    );
}

/// §7.1 — an unreachable catalog skips validation rather than failing it. Fetch
/// failure is not evidence that a model went away, and refusing to start
/// because the network was briefly unavailable is the worse failure.
#[test]
fn an_unavailable_catalog_skips_validation_instead_of_failing_it() {
    let catalog = Catalog::fallback();

    assert!(!catalog.authoritative);
    assert!(
        catalog.validate(&["anything-at-all".to_owned()]).is_ok(),
        "validation must be skipped, not failed"
    );
}

/// The fallback carries ids only. Inventing windows for it would make the guard
/// fire on figures nobody measured.
#[test]
fn the_fallback_states_no_windows() {
    let catalog = Catalog::fallback();

    assert!(!catalog.ids().is_empty());
    for model in catalog.selectable() {
        assert_eq!(
            model.context_window, None,
            "{} should carry no invented window",
            model.id
        );
    }
}

/// A catalog that arrives unreadable is an error rather than an empty catalog.
/// An empty one reads as "no models exist", which would fail every mapping.
#[test]
fn an_unreadable_catalog_is_an_error() {
    assert!(Catalog::parse("{{ not json", SHIPPING).is_err());
}

/// Some responses key the list differently. Both shapes parse.
#[test]
fn either_list_key_parses() {
    let catalog = Catalog::parse(
        r#"{"models":[{"slug":"gpt-5.6-terra","context_window":1000}]}"#,
        SHIPPING,
    )
    .expect("the alternate shape should parse");

    assert_eq!(
        catalog.get("gpt-5.6-terra").and_then(|m| m.context_window),
        Some(1_000)
    );
}

/// The configured share is the one applied, not a compiled-in constant.
///
/// This is the figure the client is told, and the client compacts against it.
/// A configuration key that parses and then changes nothing would leave an
/// operator believing they had moved it.
#[test]
fn the_configured_share_is_what_the_window_is_measured_against() {
    let catalog = Catalog::parse(SAMPLE, 50.0).unwrap();

    assert_eq!(
        catalog.get("gpt-5.4-mini").unwrap().effective_window(),
        Some(100_000),
        "half of a 200,000 window"
    );
}

/// A share stated by the catalog itself still wins over the configured default.
/// The default is what applies where the catalog said nothing — it is not an
/// override of what the backend reported about its own model.
#[test]
fn a_share_the_catalog_states_is_not_overridden_by_the_default() {
    let catalog = Catalog::parse(SAMPLE, 50.0).unwrap();

    assert_eq!(
        catalog.get("gpt-5.6-terra").unwrap().effective_window(),
        Some(258_400),
        "the entry states 95% of its own, and that is authoritative"
    );
}

// ---------------------------------------------------------------------------
// What the fetch actually sends. The configured client version is the one that
// goes on the wire, because a version the backend does not like returns an
// empty list rather than an error — and an empty list is indistinguishable from
// an account with no models.
// ---------------------------------------------------------------------------

/// A catalog endpoint that records the query it was asked with.
async fn recording_catalog() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::extract::RawQuery;
    use axum::extract::State;
    use axum::routing::get;

    type Seen = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    let seen: Seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    async fn handle(State(seen): State<Seen>, RawQuery(query): RawQuery) -> &'static str {
        if let Ok(mut seen) = seen.lock() {
            seen.push(query.unwrap_or_default());
        }
        r#"{"data":[{"id":"gpt-5.6-luna","context_window":272000}]}"#
    }

    let app = axum::Router::new()
        .route("/models", get(handle))
        .with_state(std::sync::Arc::clone(&seen));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}/models"), seen)
}

#[tokio::test]
async fn the_configured_client_version_is_the_one_sent() {
    let (endpoint, seen) = recording_catalog().await;

    let catalog = codex_cc_proxy::catalog::fetch(
        &reqwest::Client::new(),
        &endpoint,
        "token",
        None,
        "9.9.9",
        SHIPPING,
    )
    .await;

    assert!(catalog.authoritative);
    let queries = seen.lock().unwrap().clone();
    assert_eq!(queries, vec!["client_version=9.9.9".to_owned()]);
}

/// And the configured share reaches the models the fetch returns, not just the
/// ones parsed directly. This is the path the daemon actually takes.
#[tokio::test]
async fn a_fetched_catalog_carries_the_configured_share() {
    let (endpoint, _) = recording_catalog().await;

    let catalog = codex_cc_proxy::catalog::fetch(
        &reqwest::Client::new(),
        &endpoint,
        "token",
        None,
        "2.0.0",
        25.0,
    )
    .await;

    assert_eq!(
        catalog.get("gpt-5.6-luna").unwrap().effective_window(),
        Some(68_000),
        "a quarter of a 272,000 window"
    );
}

/// A model mapped to several tiers is named once.
///
/// Four tiers pointing at one model produced "gpt-5.6-luna, gpt-5.6-luna,
/// gpt-5.6-luna, gpt-5.6-luna", which reads as four separate problems and
/// buries the one that matters — that the catalog came back empty.
#[test]
fn a_model_mapped_to_several_tiers_is_named_once() {
    let catalog = Catalog::parse(SAMPLE, SHIPPING).unwrap();
    let mapped = vec![
        "gpt-4-imaginary".to_owned(),
        "gpt-4-imaginary".to_owned(),
        "gpt-4-imaginary".to_owned(),
    ];

    let error = catalog
        .validate(&mapped)
        .expect_err("still a mapping error");

    assert_eq!(
        error.message.matches("gpt-4-imaginary").count(),
        1,
        "{}",
        error.message
    );
}

/// An empty catalog says so, rather than printing an empty list and leaving the
/// reader to infer it. This is what a stale `client_version` looks like, and it
/// is the one case where the list of alternatives is the least useful part.
#[test]
fn an_empty_catalog_explains_itself_rather_than_listing_nothing() {
    let catalog = Catalog::parse(r#"{"data":[]}"#, SHIPPING).unwrap();

    let error = catalog
        .validate(&["gpt-5.6-luna".to_owned()])
        .expect_err("an empty catalog cannot satisfy a mapping");

    assert!(
        error.message.contains("client_version"),
        "the reader needs to be pointed at the cause: {}",
        error.message
    );
}
