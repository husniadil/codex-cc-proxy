//! `docs/proxy-behavior.md` §7.0 — the model catalog.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy::catalog::Catalog;
use pretty_assertions::assert_eq;

const SAMPLE: &str = r#"{
  "data": [
    {
      "id": "gpt-5-codex",
      "context_window": 272000,
      "max_context_window": 400000,
      "effective_context_window_percent": 95.0,
      "is_visible": true
    },
    {
      "id": "gpt-5-codex-mini",
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
    let catalog = Catalog::parse(LIVE_SHAPE).expect("the live shape should parse");

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
    let catalog = Catalog::parse(LIVE_SHAPE).unwrap();
    let luna = catalog.get("gpt-5.6-luna").unwrap();

    assert_eq!(luna.efforts, vec!["low", "medium"]);
}

/// An entry keyed by `slug` is the same as one keyed by `id`.
#[test]
fn the_live_shape_yields_real_windows() {
    let catalog = Catalog::parse(LIVE_SHAPE).unwrap();
    let luna = catalog.get("gpt-5.6-luna").unwrap();

    assert_eq!(luna.context_window, Some(272_000));
    // No stated percentage, so the default applies rather than the whole window.
    assert_eq!(luna.effective_window(), Some(258_400));
}

#[test]
fn the_catalog_parses_ids_and_windows() {
    let catalog = Catalog::parse(SAMPLE).expect("the catalog should parse");

    assert!(catalog.authoritative);
    assert_eq!(
        catalog.get("gpt-5-codex").and_then(|m| m.context_window),
        Some(272_000)
    );
}

/// Where both windows are present the smaller-scoped one wins. The maximum
/// describes a ceiling this account may not have, so trusting it would let
/// requests through that the account cannot actually serve.
#[test]
fn the_smaller_scoped_window_is_authoritative() {
    let catalog = Catalog::parse(SAMPLE).unwrap();
    let model = catalog.get("gpt-5-codex").unwrap();

    assert_eq!(model.context_window, Some(272_000));
    assert_ne!(model.context_window, Some(400_000));
}

/// The effective window reserves headroom for instructions, tool overhead, and
/// output.
#[test]
fn the_effective_window_applies_the_percentage() {
    let catalog = Catalog::parse(SAMPLE).unwrap();

    assert_eq!(
        catalog.get("gpt-5-codex").unwrap().effective_window(),
        Some(258_400)
    );
    // Stating no percentage means the default applies, not that all of the
    // window is usable.
    assert_eq!(
        catalog.get("gpt-5-codex-mini").unwrap().effective_window(),
        Some(190_000)
    );
}

/// A model with no stated window is unknown, not assumed. A guessed window
/// either rejects requests that would have worked or forwards ones that cannot,
/// and both are worse than declining to guess.
#[test]
fn a_model_with_no_window_is_unknown_rather_than_assumed() {
    let catalog = Catalog::parse(SAMPLE).unwrap();
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
    let catalog = Catalog::parse(SAMPLE).unwrap();

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
    let catalog = Catalog::parse(SAMPLE).unwrap();
    assert!(catalog.validate(&["gpt-5-codex".to_owned()]).is_ok());
}

#[test]
fn a_mapping_onto_an_unknown_model_is_rejected_and_says_what_exists() {
    let catalog = Catalog::parse(SAMPLE).unwrap();

    let error = catalog
        .validate(&["gpt-4-imaginary".to_owned()])
        .expect_err("an unknown model should be rejected");

    assert!(
        error.message.contains("gpt-4-imaginary"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("gpt-5-codex"),
        "the error should name what is available: {}",
        error.message
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
    assert!(Catalog::parse("{{ not json").is_err());
}

/// Some responses key the list differently. Both shapes parse.
#[test]
fn either_list_key_parses() {
    let catalog = Catalog::parse(r#"{"models":[{"slug":"gpt-5-codex","context_window":1000}]}"#)
        .expect("the alternate shape should parse");

    assert_eq!(
        catalog.get("gpt-5-codex").and_then(|m| m.context_window),
        Some(1_000)
    );
}
