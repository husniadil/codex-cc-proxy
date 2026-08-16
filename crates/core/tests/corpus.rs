//! Replays the fixture corpus.
//!
//! Every fixture is translated in both directions and snapshotted. The corpus
//! is the evidence base for the transports built on top of it, so a change in
//! translation shows here as a diff across every capability it touches.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use codex_cc_proxy_core::anthropic::MessagesRequest;
use codex_cc_proxy_core::fixture::Capability;
use codex_cc_proxy_core::fixture::Fixture;
use codex_cc_proxy_core::translate::ResponseOptions;
use codex_cc_proxy_core::translate::ResponseTranslator;
use codex_cc_proxy_core::translate::TranslateOptions;
use codex_cc_proxy_core::translate::discovered_tool_names;
use codex_cc_proxy_core::translate::translate_request;
use std::collections::BTreeSet;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures")
}

fn load_all() -> Vec<Fixture> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(corpus_dir())
        .expect("the corpus directory should exist")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();

    paths
        .iter()
        .map(|path| {
            let raw = std::fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
            serde_json::from_str::<Fixture>(&raw)
                .unwrap_or_else(|error| panic!("parsing {}: {error}", path.display()))
        })
        .collect()
}

/// Replay one fixture end to end and render it as text for snapshotting.
fn replay(fixture: &Fixture) -> String {
    let request: MessagesRequest = serde_json::from_value(fixture.request.clone())
        .unwrap_or_else(|error| panic!("{}: request should deserialize: {error}", fixture.name));

    // A session that had already seen this conversation would have recorded any
    // discovery in it, so the corpus replays with that knowledge rather than
    // without — otherwise every discovered tool would appear withheld.
    let options = TranslateOptions {
        discovered_tools: discovered_tool_names(&request),
        prompt_cache_key: Some(format!("fixture-{}", fixture.name)),
        ..TranslateOptions::default()
    };

    let translated = translate_request(&request, &options);
    let mut out = String::new();
    out.push_str("--- request ---\n");
    out.push_str(&serde_json::to_string_pretty(&translated).unwrap_or_default());

    out.push_str("\n\n--- frames ---\n");
    let mut translator = ResponseTranslator::new(ResponseOptions {
        message_id: format!("msg_{}", fixture.name),
        model: request.model,
        estimated_input_tokens: 100,
    });
    let mut frames = Vec::new();
    for event in &fixture.upstream {
        frames.extend(translator.push(&event.to_string()));
    }
    frames.extend(translator.finish());

    for frame in &frames {
        out.push_str(&serde_json::to_string(frame).unwrap_or_default());
        out.push('\n');
    }

    out
}

#[test]
fn every_fixture_replays() {
    for fixture in load_all() {
        let name = fixture.name.as_str();
        insta::assert_snapshot!(name, replay(&fixture));
    }
}

/// A fixture's file name and its declared name must agree, or a snapshot points
/// at the wrong evidence.
#[test]
fn fixture_names_match_their_files() {
    for path in std::fs::read_dir(corpus_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
    {
        let raw = std::fs::read_to_string(&path).unwrap();
        let fixture: Fixture = serde_json::from_str(&raw).unwrap();
        let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap();
        assert_eq!(
            fixture.name,
            stem,
            "{} declares a different name",
            path.display()
        );
    }
}

/// Every capability in `proxy-behavior.md` §1 has at least one exchange. A
/// capability with no fixture is one whose silent failure nothing would catch.
#[test]
fn the_corpus_covers_every_capability() {
    let covered: BTreeSet<Capability> = load_all()
        .iter()
        .map(|fixture| fixture.capability)
        .collect();

    let missing: Vec<Capability> = Capability::ALL
        .into_iter()
        .filter(|capability| !covered.contains(capability))
        .collect();

    assert!(
        missing.is_empty(),
        "capabilities with no fixture: {missing:?}"
    );
}

/// Provenance is a required field, and its note must say something. A fixture
/// that does not record where it came from is a fixture whose weight a reader
/// has to guess at.
#[test]
fn every_fixture_explains_its_provenance() {
    for fixture in load_all() {
        assert!(
            fixture.note.len() > 40,
            "{} has no meaningful provenance note",
            fixture.name
        );
    }
}
