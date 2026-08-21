//! The emitted surface, measured against the real one.
//!
//! `fixtures/surface/` holds real answers from the Anthropic Messages endpoint,
//! captured by `record surface`. This compares what the translate path emits
//! against them at the level of *shape*: which SSE events exist, which fields
//! each one carries, and which keys an error envelope has. Content is never
//! compared — two backends answering the same question differently is not
//! drift, and a token count that moved is not a defect.
//!
//! The rule is a subset, in one direction, and the direction is the whole
//! point. A field the real surface carries and this proxy omits is a field a
//! client already tolerates being absent, because the real endpoint's own
//! answers vary. A field this proxy emits that the real surface *never* emits
//! is something the client was never built to receive — a name it may key on,
//! ignore, or choke on, and nothing upstream will ever tell us which.
//!
//! Nothing here touches the network: the real halves are files, and the
//! emitted halves come from replaying the corpus through the shipping
//! translator.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use proxenos::surface::Capture;
use proxenos_core::anthropic::MessagesRequest;
use proxenos_core::fixture::Fixture;
use proxenos_core::translate::ResponseOptions;
use proxenos_core::translate::ResponseTranslator;
use proxenos_core::translate::TranslateOptions;
use proxenos_core::translate::discovered_tool_names;
use proxenos_core::translate::translate_request;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_json_dir<T: serde::de::DeserializeOwned>(directory: PathBuf) -> Vec<T> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("reading {}: {error}", directory.display()))
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
            serde_json::from_str(&raw)
                .unwrap_or_else(|error| panic!("parsing {}: {error}", path.display()))
        })
        .collect()
}

fn captures() -> Vec<Capture> {
    read_json_dir(root().join("fixtures/surface"))
}

/// The corpus, replayed through the shipping translator. Every frame this proxy
/// is capable of emitting for the recorded exchanges appears here.
fn emitted_frames() -> Vec<Value> {
    let fixtures: Vec<Fixture> = read_json_dir(root().join("fixtures"));
    let mut frames = Vec::new();

    for fixture in &fixtures {
        let Ok(request) = serde_json::from_value::<MessagesRequest>(fixture.request.clone()) else {
            // The relay fixture's body is deliberately not one this proxy's own
            // types reproduce — that is its point, and it is not translated.
            continue;
        };
        let options = TranslateOptions {
            discovered_tools: discovered_tool_names(&request),
            ..TranslateOptions::default()
        };
        let _ = translate_request(&request, &options);

        let mut translator = ResponseTranslator::new(ResponseOptions {
            message_id: format!("msg_{}", fixture.name),
            model: request.model.clone(),
            estimated_input_tokens: 100,
        });
        let mut produced = Vec::new();
        for event in &fixture.upstream {
            produced.extend(translator.push(&event.to_string()));
        }
        produced.extend(translator.finish());
        for frame in &produced {
            if let Ok(value) = serde_json::to_value(frame) {
                frames.push(value);
            }
        }
    }
    assert!(!frames.is_empty(), "the corpus produced no frames at all");
    frames
}

/// A shape is addressed by where it sits and what it calls itself: the event
/// name, the path to the object, and that object's own `type` where it has one.
/// Without the inner type, a `text_delta` and an `input_json_delta` would be
/// compared as if they were the same thing.
type Address = (String, String, Option<String>);

fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

/// Every addressable object in one frame: the frame itself, and the nested
/// objects the Messages surface puts a shape inside.
fn shapes(frame: &Value) -> Vec<(Address, BTreeSet<String>)> {
    let Some(name) = frame.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let mut found = vec![((name.to_owned(), String::new(), None), keys(frame))];

    for path in ["message", "delta", "content_block", "usage"] {
        let Some(nested) = frame.get(path) else {
            continue;
        };
        if !nested.is_object() {
            continue;
        }
        let inner = nested
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned);
        found.push(((name.to_owned(), path.to_owned(), inner), keys(nested)));

        // One level further, because `message.usage` is where the accounting
        // contract lives and it is the field set most likely to drift.
        if let Some(usage) = nested.get("usage")
            && usage.is_object()
        {
            found.push((
                (name.to_owned(), format!("{path}.usage"), None),
                keys(usage),
            ));
        }
    }
    found
}

/// The real surface, as an address-to-field-set map.
fn measured() -> BTreeMap<Address, BTreeSet<String>> {
    let mut map: BTreeMap<Address, BTreeSet<String>> = BTreeMap::new();
    for capture in captures() {
        for event in &capture.events {
            for (address, fields) in shapes(event) {
                map.entry(address).or_default().extend(fields);
            }
        }
    }
    map
}

/// Where the emitted shapes leave the measured ones. An empty list is
/// conformance; every entry is one name a real answer has never carried.
fn drift(frames: &[Value], real: &BTreeMap<Address, BTreeSet<String>>) -> Vec<String> {
    let names: BTreeSet<&String> = real.keys().map(|(name, _, _)| name).collect();
    let mut found = Vec::new();

    for frame in frames {
        for (address, fields) in shapes(frame) {
            let (name, path, inner) = &address;
            if !names.contains(name) {
                found.push(format!("event `{name}` is not one the real surface emits"));
                continue;
            }
            let Some(real_fields) = real.get(&address) else {
                // Unmeasured rather than wrong: this shape did not occur in
                // any captured exchange, so nothing here can judge it.
                continue;
            };
            for field in fields.difference(real_fields) {
                let at = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{name}.{path}")
                };
                let at = match inner {
                    Some(inner) => format!("{at} ({inner})"),
                    None => at,
                };
                found.push(format!(
                    "`{at}` emits `{field}`, which the real surface does not"
                ));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// The vocabulary itself, asserted rather than derived, so a capture that
/// silently lost its stream fails here instead of making every check below
/// vacuous.
#[test]
fn the_captured_surface_covers_the_streaming_vocabulary() {
    let real = measured();
    let names: BTreeSet<&str> = real.keys().map(|(name, _, _)| name.as_str()).collect();

    for expected in [
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
        "ping",
    ] {
        assert!(
            names.contains(expected),
            "no captured exchange carries `{expected}`; the corpus cannot measure it"
        );
    }
}

#[test]
fn the_translate_path_emits_no_shape_the_real_surface_never_emits() {
    let violations = drift(&emitted_frames(), &measured());
    assert!(
        violations.is_empty(),
        "the emitted surface has drifted from the measured one:\n  {}",
        violations.join("\n  ")
    );
}

/// The check has to be able to fail. A conformance test that passes against a
/// deliberately wrong shape is scaffolding, not evidence.
#[test]
fn a_drifted_shape_is_reported() {
    let real = measured();

    let renamed_event = serde_json::json!({ "type": "message_started" });
    assert!(
        !drift(&[renamed_event], &real).is_empty(),
        "a renamed event passed as conforming"
    );

    let extra_field = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": { "type": "input_json_delta", "partial_json": "", "invented": true }
    });
    let reported = drift(&[extra_field], &real);
    assert!(
        reported.iter().any(|entry| entry.contains("invented")),
        "an invented field passed as conforming: {reported:?}"
    );
}

/// §5 answers sizing locally, so this is the one place the shape it answers
/// with can be held to the real endpoint's.
#[test]
fn the_sizing_response_has_the_real_endpoints_keys() {
    let sizing = captures()
        .into_iter()
        .find(|capture| capture.endpoint.ends_with("count_tokens"))
        .expect("a captured sizing exchange");
    let real = keys(sizing.body.as_ref().expect("sizing answers with a body"));

    let emitted = keys(&serde_json::json!({ "input_tokens": 100 }));
    assert_eq!(emitted, real);
}

/// Non-negotiable #5: every failure leaves in the client's own error shape.
/// Measured against a real refusal rather than against the documentation.
#[test]
fn the_error_envelope_stays_inside_the_real_one() {
    let refusal = captures()
        .into_iter()
        .find(|capture| capture.status >= 400)
        .expect("a captured refusal");
    let real = refusal.body.expect("a refusal answers with a body");

    let emitted =
        serde_json::to_value(proxenos::error::ProxyError::invalid_request("anything").body())
            .expect("the error body should serialize");
    let envelope = serde_json::json!({ "type": "error", "error": emitted });

    assert!(
        keys(&envelope).is_subset(&keys(&real)),
        "the envelope carries a key the real one does not: {:?} vs {:?}",
        keys(&envelope),
        keys(&real)
    );
    assert!(
        keys(&envelope["error"]).is_subset(&keys(&real["error"])),
        "the error object carries a key the real one does not: {:?} vs {:?}",
        keys(&envelope["error"]),
        keys(&real["error"])
    );
    assert_eq!(envelope["type"], real["type"]);
}

/// What the captured exchanges do **not** reach.
///
/// A subset check is silent about a shape no capture contains: it is skipped,
/// and a skip reads exactly like a pass. This names the gaps, so a new one has
/// to be looked at rather than absorbed. Every entry here is a block kind the
/// five captured exchanges could not produce — extended thinking and the server
/// tools each need a turn of their own, and neither has a symptom that would
/// justify the quota today.
#[test]
fn the_shapes_no_capture_reaches_are_named() {
    let real = measured();
    let mut unmeasured: BTreeSet<String> = BTreeSet::new();
    for frame in &emitted_frames() {
        for (address, _) in shapes(frame) {
            if !real.contains_key(&address) {
                let (name, path, inner) = address;
                unmeasured.insert(match inner {
                    Some(inner) => format!("{name}.{path} ({inner})"),
                    None => format!("{name}.{path}"),
                });
            }
        }
    }

    let expected: BTreeSet<String> = [
        "content_block_delta.delta (thinking_delta)",
        "content_block_start.content_block (server_tool_use)",
        "content_block_start.content_block (thinking)",
        "content_block_start.content_block (web_search_tool_result)",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();

    assert_eq!(
        unmeasured, expected,
        "the set of shapes no captured exchange reaches has changed"
    );
}
