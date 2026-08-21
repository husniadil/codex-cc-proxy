//! `docs/proxy-behavior.md` §10.3 — every probe, against the replay corpus.
//!
//! Each probe turns on a code that exists nowhere except in the exchange under
//! test. A model handed nothing describes a file confidently from its name, and
//! that output is indistinguishable from success; a random code is not
//! something plausibility can produce.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

mod replay;

use pretty_assertions::assert_eq;
use proxenos::doctor::Corpus;
use proxenos::probe;
use proxenos::probe::Outcome;
use proxenos::probe::Status;
use std::path::PathBuf;

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures")
}

/// A replayed run, described the way `doctor` describes one.
fn replayed(corpus: &str) -> probe::Run {
    probe::Run {
        evidence: probe::Evidence::Replay {
            corpus: corpus.to_owned(),
        },
    }
}

/// Run one probe through `doctor`, which is the path that ships. A test harness
/// that reimplements the runner proves the harness works, not the tool.
async fn run_via_doctor(name: &str) -> Outcome {
    proxenos::doctor::run(&Corpus::Dir(corpus()), Some(name))
        .await
        .expect("the probe should be known")
        .into_iter()
        .next()
        .expect("one outcome")
}

/// Every probe passes against the corpus.
#[tokio::test]
async fn every_probe_passes_against_the_replay_corpus() {
    let outcomes = proxenos::doctor::run(&Corpus::Dir(corpus()), None)
        .await
        .expect("the suite should run");

    let failures: Vec<String> = outcomes
        .iter()
        .filter_map(|outcome| match &outcome.status {
            Status::Failed(reason) => Some(format!("{}: {reason}", outcome.name)),
            _ => None,
        })
        .collect();

    assert!(
        failures.is_empty(),
        "probes failed:\n  {}",
        failures.join("\n  ")
    );
    assert_eq!(outcomes.len(), probe::all().len());
}

/// Each probe runs alone. A suite that only works as a whole cannot be used to
/// diagnose one broken capability.
#[tokio::test]
async fn each_probe_runs_on_its_own() {
    for probe in probe::all() {
        let outcome = run_via_doctor(probe.name).await;
        assert_eq!(
            outcome.status,
            Status::Passed,
            "{} failed alone",
            probe.name
        );
    }
}

/// Asking for a probe that does not exist names the ones that do.
#[tokio::test]
async fn an_unknown_probe_lists_the_known_ones() {
    let error = proxenos::doctor::run(&Corpus::Dir(corpus()), Some("not-a-probe"))
        .await
        .expect_err("an unknown probe should fail");

    assert!(error.message.contains("read-image"), "{}", error.message);
}

/// A probe that cannot run says so, and is not counted as a pass. A probe that
/// established nothing while reporting success is the same lie the probes exist
/// to catch.
#[tokio::test]
async fn a_probe_that_cannot_run_reports_honestly() {
    let empty = tempfile::tempdir().unwrap();

    let outcomes = proxenos::doctor::run(&Corpus::Dir(empty.path().to_path_buf()), None)
        .await
        .expect("the suite should still run");

    assert!(!outcomes.is_empty());
    for outcome in &outcomes {
        match &outcome.status {
            Status::Skipped(reason) => assert!(
                reason.contains("no fixture"),
                "the skip should say why: {reason}"
            ),
            other => panic!("{} should have been skipped, got {other:?}", outcome.name),
        }
    }

    // And a skip is never counted as a pass.
    let rendered = probe::matrix(&outcomes, &replayed("an empty corpus"));
    assert!(rendered.contains("0 passed"), "{rendered}");
}

/// The corpus travels with the binary. An installed `proxenos` has no
/// checkout to read `fixtures/` out of, and a `doctor` that skips every probe
/// there is a first run that establishes nothing.
#[tokio::test]
async fn every_probe_passes_against_the_embedded_corpus() {
    let outcomes = proxenos::doctor::run(&Corpus::Embedded, None)
        .await
        .expect("the suite should run with no directory at all");

    assert_eq!(outcomes.len(), probe::all().len());
    for outcome in &outcomes {
        assert_eq!(outcome.status, Status::Passed, "{} failed", outcome.name);
    }
}

/// The embedded copy is compiled from the files, so it cannot go stale — but a
/// fixture added to the directory and not to the list would be missing from
/// every installed binary while the checkout's own runs stayed green.
#[test]
fn the_embedded_corpus_holds_every_fixture_on_disk() {
    let mut on_disk: Vec<String> = std::fs::read_dir(corpus())
        .expect("the corpus directory")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "json")
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect();
    on_disk.sort();

    let mut embedded: Vec<String> = Corpus::embedded_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    embedded.sort();

    assert_eq!(embedded, on_disk);
}

/// Resolution is explicit-first: a directory the operator named is the one that
/// answers, and it is never quietly substituted for the embedded copy. A
/// `--fixtures` that points somewhere empty must still skip, or `record` output
/// could be shadowed by a recording compiled in months earlier.
#[test]
fn a_named_directory_is_never_substituted() {
    let empty = tempfile::tempdir().unwrap();

    assert!(matches!(
        Corpus::resolve(Some(empty.path().to_path_buf())),
        Corpus::Dir(_)
    ));
}

/// The probes must be able to fail. A check that passes against a proxy which
/// dropped the payload is not a probe, it is decoration.
#[tokio::test]
async fn a_probe_fails_when_the_marker_never_arrives() {
    let read_image = probe::all()
        .into_iter()
        .find(|probe| probe.name == "read-image")
        .unwrap();

    // What the proxy would have sent if it silently dropped the attachment.
    let stripped = serde_json::json!({
        "input": [
            { "type": "message", "role": "user", "content": [] },
            { "type": "function_call", "call_id": "toolu_read_1", "name": "Read" },
            { "type": "function_call_output", "call_id": "toolu_read_1", "output": "Read 1 image" },
        ],
    });

    let status = probe::evaluate(&read_image, &stripped, &[]);

    match status {
        Status::Failed(reason) => assert!(reason.contains("FenH7x"), "{reason}"),
        other => panic!("a dropped attachment should fail the probe, got {other:?}"),
    }
}

/// A marker the model spelled across several deltas still counts as received.
///
/// This is not hypothetical: a recorded stream emits a reply as one delta,
/// while a live one emits it a token at a time. Scanning the frames as raw JSON
/// finds the marker in the first case and never in the second — so every
/// attachment probe failed against a backend that had in fact read the
/// attachment and said so.
#[test]
fn a_marker_split_across_deltas_still_counts() {
    let probe = probe::all()
        .into_iter()
        .find(|probe| probe.name == "web-fetch")
        .unwrap();

    let frames: Vec<serde_json::Value> = ["The key is L", "9WQ", "2T."]
        .iter()
        .map(|piece| {
            serde_json::json!({
                "type": "content_block_delta",
                "delta": { "type": "text_delta", "text": piece },
            })
        })
        .collect();

    let request = serde_json::json!({ "input": "L9WQ2T" });

    assert_eq!(probe::evaluate(&probe, &request, &frames), Status::Passed);
}

/// And a marker that never arrives still fails, however the deltas fall.
#[test]
fn reassembly_does_not_invent_a_marker() {
    let probe = probe::all()
        .into_iter()
        .find(|probe| probe.name == "web-fetch")
        .unwrap();

    let frames = vec![serde_json::json!({
        "type": "content_block_delta",
        "delta": { "type": "text_delta", "text": "I could not read the page." },
    })];

    let request = serde_json::json!({ "input": "L9WQ2T" });

    match probe::evaluate(&probe, &request, &frames) {
        Status::Failed(reason) => assert!(reason.contains("L9WQ2T"), "{reason}"),
        other => panic!("a missing marker must fail, got {other:?}"),
    }
}

/// The marker is unguessable by construction: it appears in the fixture and
/// nowhere else in the tree. A probe keyed on something derivable from a
/// filename would pass against a model that never saw the file.
#[test]
fn every_marker_is_absent_from_the_rest_of_the_corpus() {
    let markers = [
        ("read-image", "P7K4XR"),
        ("read-document", "V2M9QZ"),
        ("web-fetch", "L9WQ2T"),
        // The relay's pair: one in a field the proxy does not model, one
        // spoken back in a delta.
        ("relay", "N8QP4W"),
        ("relay", "T5ZJ9C"),
        // The bytes of the image itself, which is what proves the attachment
        // travelled — the code is rendered as pixels and appears nowhere in
        // the encoding.
        ("read-image", "+FenH7x+dQXRB/+z55/wkvkp/zDUr24A"),
    ];

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures");

    for (owner, marker) in markers {
        for entry in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("");
            if name == owner || path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let body = std::fs::read_to_string(&path).unwrap();
            assert!(
                !body.contains(marker),
                "`{marker}` appears in {name} as well as {owner}, so a probe \
                 keyed on it proves less than it claims"
            );
        }
    }
}

/// The matrix says what it was run against. One built from replayed fixtures
/// that reads like one built from a live backend is exactly the plausible
/// output §10.3 exists to prevent.
#[tokio::test]
async fn the_matrix_states_its_evidence() {
    let outcomes = proxenos::doctor::run(&Corpus::Dir(corpus()), None)
        .await
        .unwrap();
    let rendered = probe::matrix(&outcomes, &replayed("the checkout's fixtures"));

    assert!(
        rendered.contains("the backend was not contacted"),
        "{rendered}"
    );
    assert!(rendered.contains("read-image"), "{rendered}");
}

/// A live run reaches a real transport and says so.
///
/// The transport here answers from a loopback server rather than the backend —
/// no test in this suite reaches the network — but it is the shipping transport
/// carrying a real request, which is what distinguishes this path from the
/// replay one. What it proves is the wiring: that `--live` sends the probe's
/// own request through the stack that ships and evaluates what comes back.
#[tokio::test]
async fn a_live_run_uses_the_transport_and_labels_itself() {
    let fixture: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(corpus().join("tool-calling.json")).unwrap())
            .unwrap();
    let events: Vec<serde_json::Value> =
        serde_json::from_value(fixture["upstream"].clone()).unwrap();

    let server = replay::ReplayServer::start(replay::Behavior::Events(events)).await;
    let transport = std::sync::Arc::new(proxenos::upstream::http::HttpTransport::new(
        server.url.clone(),
    ));

    let outcomes = proxenos::doctor::run_live(
        &Corpus::Dir(corpus()),
        Some("tool-calling"),
        transport,
        std::sync::Arc::new(vec![proxenos::ingress::ModelMapping {
            requested: "claude-sonnet-5".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
            account: None,
        }]),
        None,
    )
    .await
    .expect("the probe should be known");

    assert_eq!(outcomes[0].status, Status::Passed);

    // The request the backend saw is the probe's own, not a fixture replayed
    // back at itself.
    let seen = server.requests();
    assert_eq!(seen.len(), 1, "the live run should have sent one request");
    assert_eq!(seen[0]["model"], serde_json::json!("gpt-5.6-terra"));

    let rendered = probe::matrix(
        &outcomes,
        &probe::Run {
            evidence: probe::Evidence::Live {
                account: Some("work".to_owned()),
            },
        },
    );
    assert!(rendered.contains("the backend answered"), "{rendered}");
}

/// A live run spends at the effort the operator configured.
///
/// `--live` is the one command that bills by design, so it is the last place an
/// effort ceiling should be quietly ignored. Someone who capped effort to
/// control what a session costs did not exempt the probes from it.
#[tokio::test]
async fn a_live_run_honours_the_configured_effort_ceiling() {
    let fixture: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(corpus().join("tool-calling.json")).unwrap())
            .unwrap();
    let events: Vec<serde_json::Value> =
        serde_json::from_value(fixture["upstream"].clone()).unwrap();

    let server = replay::ReplayServer::start(replay::Behavior::Events(events)).await;
    let transport = std::sync::Arc::new(proxenos::upstream::http::HttpTransport::new(
        server.url.clone(),
    ));

    proxenos::doctor::run_live(
        &Corpus::Dir(corpus()),
        Some("tool-calling"),
        transport,
        std::sync::Arc::new(vec![proxenos::ingress::ModelMapping {
            requested: "claude-sonnet-5".to_owned(),
            upstream: "gpt-5.6-terra".to_owned(),
            account: None,
        }]),
        Some(proxenos_core::responses::Effort::Low),
    )
    .await
    .expect("the probe should be known");

    let seen = server.requests();
    assert_eq!(seen[0]["reasoning"]["effort"], serde_json::json!("low"));
}

/// A check that only means something against a fixture does not run live.
///
/// The web-search probe asserts a URL invented for the corpus. A live search
/// returns whatever it returns, so applying that check live would fail a
/// working capability — and quietly train whoever reads the matrix to ignore
/// it.
#[test]
fn fixture_bound_checks_are_marked_as_such() {
    let search = probe::all()
        .into_iter()
        .find(|probe| probe.name == "web-search")
        .expect("the web-search probe");

    assert!(
        search
            .replay_only
            .iter()
            .any(|check| format!("{check:?}").contains("example.invalid")),
        "the invented URL must be a replay-only check"
    );
    assert!(
        !format!("{:?}", search.checks).contains("example.invalid"),
        "and must not be one of the checks a live run applies"
    );
}

/// Every capability has a probe. One without is a capability whose silent
/// failure nothing would catch.
#[test]
fn every_capability_has_a_probe() {
    use proxenos_core::fixture::Capability;

    let covered: Vec<Capability> = probe::all().iter().map(|probe| probe.capability).collect();

    for capability in Capability::ALL {
        assert!(covered.contains(&capability), "{capability:?} has no probe");
    }
}

/// Every probe names what breaks silently without it.
#[test]
fn every_probe_says_why_it_exists() {
    for probe in probe::all() {
        assert!(
            probe.rationale.len() > 30,
            "{} does not say what it protects against",
            probe.name
        );
    }
}

#[test]
fn the_matrix_counts_outcomes() {
    let outcomes = vec![
        Outcome {
            name: "a".to_owned(),
            capability: proxenos_core::fixture::Capability::ReadImage,
            surface: probe::Surface::Messages,
            rationale: "a",
            status: Status::Passed,
        },
        Outcome {
            name: "b".to_owned(),
            capability: proxenos_core::fixture::Capability::WebSearch,
            surface: probe::Surface::Messages,
            rationale: "b",
            status: Status::Failed("nope".to_owned()),
        },
        Outcome {
            name: "c".to_owned(),
            capability: proxenos_core::fixture::Capability::CountTokens,
            surface: probe::Surface::CountTokens,
            rationale: "c",
            status: Status::Skipped("no stream".to_owned()),
        },
    ];

    let rendered = probe::matrix(&outcomes, &replayed("replayed fixtures"));
    assert_eq!(
        rendered.lines().last(),
        Some("1 passed, 1 failed, 1 skipped")
    );
}

/// A failure names what breaks silently without the probe.
///
/// A row that says only "FAIL" and a reason sends whoever reads it to work out
/// for themselves whether the capability matters. The rationale is already on
/// every probe; printing it where a failure appears is the difference between a
/// diagnostic and a verdict. Passes stay one line — eight rows of prose is a
/// matrix nobody reads.
#[test]
fn a_failed_row_prints_its_rationale_and_a_passing_row_does_not() {
    let outcomes = vec![
        Outcome {
            name: "passing".to_owned(),
            capability: proxenos_core::fixture::Capability::ReadImage,
            surface: probe::Surface::Messages,
            rationale: "the rationale of a probe that passed",
            status: Status::Passed,
        },
        Outcome {
            name: "failing".to_owned(),
            capability: proxenos_core::fixture::Capability::WebSearch,
            surface: probe::Surface::Messages,
            rationale: "the rationale of a probe that failed",
            status: Status::Failed("nope".to_owned()),
        },
    ];

    let rendered = probe::matrix(&outcomes, &replayed("a corpus"));

    assert!(
        rendered.contains("the rationale of a probe that failed"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("the rationale of a probe that passed"),
        "{rendered}"
    );
}

/// Under `--live` the header says the backend answered and was billed. That is
/// not true of `count-tokens`, which never leaves the proxy by design — so the
/// row says so rather than being quietly dropped from a list whose whole job is
/// to be complete.
#[test]
fn a_live_run_marks_the_probe_that_never_reaches_the_backend() {
    let outcomes = vec![Outcome {
        name: "count-tokens".to_owned(),
        capability: proxenos_core::fixture::Capability::CountTokens,
        surface: probe::Surface::CountTokens,
        rationale: "an absent estimate leaves the client sizing nothing",
        status: Status::Passed,
    }];

    let live = probe::matrix(
        &outcomes,
        &probe::Run {
            evidence: probe::Evidence::Live {
                account: Some("work".to_owned()),
            },
        },
    );
    assert!(live.contains(probe::NEVER_REACHES_THE_BACKEND), "{live}");

    // Replayed, nothing reached the backend anyway, so the mark would say
    // nothing about this row that the header does not already say about all of
    // them.
    let replay = probe::matrix(&outcomes, &replayed("a corpus"));
    assert!(
        !replay.contains(probe::NEVER_REACHES_THE_BACKEND),
        "{replay}"
    );
}

/// The matrix names what the run actually exercised.
///
/// Eight green rows say nothing about the WebSocket transport or the relay, and
/// a reader with no line to tell them otherwise will read the green as coverage
/// of the whole proxy.
#[tokio::test]
async fn the_matrix_names_what_the_run_exercised() {
    let outcomes = proxenos::doctor::run(&Corpus::Dir(corpus()), None)
        .await
        .unwrap();

    let rendered = probe::matrix(&outcomes, &replayed("the checkout's fixtures"));
    assert!(
        rendered.contains("Not exercised: the WebSocket transport"),
        "{rendered}"
    );
    assert!(rendered.contains("no account was contacted"), "{rendered}");
    // The relay probe ran, and the line says so rather than leaving §9 unnamed.
    assert!(
        rendered.contains("the relay path (§9) was replayed"),
        "{rendered}"
    );

    let live = probe::matrix(
        &outcomes,
        &probe::Run {
            evidence: probe::Evidence::Live {
                account: Some("work".to_owned()),
            },
        },
    );
    assert!(live.contains("over the HTTP transport"), "{live}");
    assert!(live.contains("as `work`"), "{live}");
    assert!(
        live.contains("Not exercised: the WebSocket transport"),
        "{live}"
    );
}

/// The relay path (§9) has a probe of its own.
///
/// `doctor` built its own `AppState` with no relay at all, so nothing in the
/// suite drove the branch that forwards a turn instead of translating it — the
/// one path whose entire claim is that the bytes are not touched. The marker is
/// inside a field this proxy does not model: a body round-tripped through its
/// own types loses it, and loses it silently.
#[tokio::test]
async fn the_relay_probe_drives_the_relay_path() {
    let outcome = run_via_doctor("relay").await;
    assert_eq!(outcome.status, Status::Passed, "{outcome:?}");
}

/// A live relay run is not wired, and the row says so rather than passing on
/// evidence it does not have.
///
/// Driving it live needs the serving account switched to the second provider
/// for the length of the run, which is a change to what the daemon is serving
/// while it is serving it. That is left out of this slice deliberately.
#[tokio::test]
async fn a_live_run_skips_the_relay_probe() {
    let server = replay::ReplayServer::start(replay::Behavior::Events(Vec::new())).await;
    let transport = std::sync::Arc::new(proxenos::upstream::http::HttpTransport::new(
        server.url.clone(),
    ));

    let outcomes = proxenos::doctor::run_live(
        &Corpus::Dir(corpus()),
        Some("relay"),
        transport,
        std::sync::Arc::new(Vec::new()),
        None,
    )
    .await
    .expect("the probe should be known");

    match &outcomes[0].status {
        Status::Skipped(reason) => assert!(
            reason.contains("serving account"),
            "the skip should say what is missing: {reason}"
        ),
        other => panic!("a live relay probe should be skipped, got {other:?}"),
    }
}
