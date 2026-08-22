//! `docs/proxy-behavior.md` §10.3 — every probe, against the replay corpus.
//!
//! Each probe turns on a code that exists nowhere except in the exchange under
//! test. A model handed nothing describes a file confidently from its name, and
//! that output is indistinguishable from success; a random code is not
//! something plausibility can produce.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

mod replay;

use pretty_assertions::assert_eq;
use proxenos::auth::store::AccountStore;
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
        // The launch contract replays nothing, so an empty corpus takes
        // nothing away from it. Every probe that reads an exchange skips.
        if outcome.surface == probe::Surface::Environment {
            continue;
        }
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
    assert!(rendered.contains("1 passed"), "{rendered}");
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
        Err("not the probe under test".to_owned()),
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
                relay: None,
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
        Err("not the probe under test".to_owned()),
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
            note: None,
        },
        Outcome {
            name: "b".to_owned(),
            capability: proxenos_core::fixture::Capability::WebSearch,
            surface: probe::Surface::Messages,
            rationale: "b",
            status: Status::Failed("nope".to_owned()),
            note: None,
        },
        Outcome {
            name: "c".to_owned(),
            capability: proxenos_core::fixture::Capability::CountTokens,
            surface: probe::Surface::CountTokens,
            rationale: "c",
            status: Status::Skipped("no stream".to_owned()),
            note: None,
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
            note: None,
        },
        Outcome {
            name: "failing".to_owned(),
            capability: proxenos_core::fixture::Capability::WebSearch,
            surface: probe::Surface::Messages,
            rationale: "the rationale of a probe that failed",
            status: Status::Failed("nope".to_owned()),
            note: None,
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
        note: None,
    }];

    let live = probe::matrix(
        &outcomes,
        &probe::Run {
            evidence: probe::Evidence::Live {
                account: Some("work".to_owned()),
                relay: None,
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
                relay: None,
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

/// The live relay arm picks its account by reading the store, and refuses to
/// guess.
///
/// The account it relays as is never the account serving turns — it is named,
/// and `Authorizer::authorize(Some(name))` reads and refuses by that name — so
/// the choice is made here, before anything is contacted.
#[test]
fn the_live_relay_arm_names_the_account_it_would_spend() {
    let account = |name: &str, provider: proxenos::auth::store::Provider, selected: bool| {
        proxenos::auth::store::Account {
            name: name.to_owned(),
            kind: "key",
            provider: provider.as_str(),
            account_id: None,
            email: None,
            plan: None,
            expires_at: None,
            selected,
        }
    };
    let codex = account("work", proxenos::auth::store::Provider::Codex, true);
    let first = account(
        "personal-claude",
        proxenos::auth::store::Provider::Anthropic,
        false,
    );
    let second = account(
        "other-claude",
        proxenos::auth::store::Provider::Anthropic,
        false,
    );

    // Exactly one: no question to ask.
    assert_eq!(
        proxenos::doctor::relay_account(&[codex.clone(), first.clone()], None),
        Ok("personal-claude".to_owned())
    );

    // None: the reason says what the store holds, never the retired text about
    // wiring.
    let reason = proxenos::doctor::relay_account(std::slice::from_ref(&codex), None)
        .expect_err("no anthropic account, so nothing to relay as");
    assert!(reason.contains("no account"), "{reason}");
    assert!(!reason.contains("not wired"), "{reason}");

    // Several: the operator names which, rather than one being picked for them.
    let reason = proxenos::doctor::relay_account(&[first.clone(), second.clone()], None)
        .expect_err("two candidates cannot be resolved without being told");
    assert!(reason.contains("--relay-account"), "{reason}");
    assert!(reason.contains("personal-claude"), "{reason}");
    assert_eq!(
        proxenos::doctor::relay_account(&[first.clone(), second], Some("other-claude")),
        Ok("other-claude".to_owned())
    );

    // A name on the wrong provider is refused by name rather than relayed to an
    // endpoint its credential was never issued for.
    let reason = proxenos::doctor::relay_account(&[codex, first], Some("work"))
        .expect_err("`work` is on the translating provider");
    assert!(reason.contains("work"), "{reason}");
}

/// A store holding no account on the second provider skips, and says why.
#[tokio::test]
async fn a_live_run_without_a_second_provider_account_skips_the_relay_probe() {
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
        Err("the store holds no account on the anthropic provider".to_owned()),
    )
    .await
    .expect("the probe should be known");

    match &outcomes[0].status {
        Status::Skipped(reason) => {
            assert!(reason.contains("no account"), "{reason}");
            assert!(!reason.contains("not wired"), "{reason}");
        }
        other => panic!("a relay probe with no account should be skipped, got {other:?}"),
    }
}

/// The live relay arm runs the probe rather than skipping it, and names the
/// half it cannot establish.
///
/// The endpoint here is a loopback stand-in — no test in this suite reaches the
/// network — but everything else is the shipping path: the real `Relay`, a real
/// `FileStore`, and an `AccountAuthorizer` pinned by name. What the arm cannot
/// do live is watch the outbound bytes, so the request-half checks do not run
/// and the row says so instead of passing over a `Null`.
#[tokio::test]
async fn the_live_relay_arm_answers_and_names_what_it_cannot_establish() {
    let (store, dir) = relay_store();
    let serving_before = selected_account(store.as_ref());

    let (backend, backend_headers) = relay_backend().await;
    let outcomes = proxenos::doctor::run_live(
        &Corpus::Dir(corpus()),
        Some("relay"),
        std::sync::Arc::new(proxenos::upstream::http::HttpTransport::new(
            "http://127.0.0.1:1/unused",
        )),
        std::sync::Arc::new(Vec::new()),
        None,
        Ok(proxenos::doctor::LiveRelay {
            endpoint: backend,
            store: std::sync::Arc::clone(&store),
            authorizer: authorizer_for(&store),
            account: "personal-claude".to_owned(),
        }),
    )
    .await
    .expect("the probe should be known");

    assert_eq!(outcomes[0].status, Status::Passed, "{:?}", outcomes[0]);
    let note = outcomes[0]
        .note
        .as_deref()
        .expect("the live arm should name the half it cannot establish");
    assert!(note.contains("outbound"), "{note}");

    // §9 forwards the client's headers as sent, so the probe has to send what a
    // client sends. The real endpoint refuses a call without this one, and the
    // refusal reads as a broken relay rather than as an incomplete probe.
    let headers = backend_headers
        .lock()
        .expect("the stand-in recorded the headers")
        .clone()
        .expect("the backend was called");
    assert_eq!(
        headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok()),
        Some("2023-06-01")
    );

    // The probe never changes, and never depends on, which account serves turns.
    assert_eq!(selected_account(store.as_ref()), serving_before);
    assert_eq!(serving_before.as_deref(), Some("work"));
    drop(dir);
}

/// The coverage line names whose account the relay spent.
#[test]
fn the_coverage_line_names_the_relayed_account() {
    let rendered = probe::matrix(
        &[Outcome {
            name: "relay".to_owned(),
            capability: proxenos_core::fixture::Capability::Relay,
            surface: probe::Surface::Relay,
            rationale: "",
            status: Status::Passed,
            note: None,
        }],
        &probe::Run {
            evidence: probe::Evidence::Live {
                account: Some("work".to_owned()),
                relay: Some("personal-claude".to_owned()),
            },
        },
    );

    assert!(rendered.contains("as `personal-claude`"), "{rendered}");
    assert!(!rendered.contains("was replayed"), "{rendered}");
}

/// A store with one account on each provider, the translating one serving.
fn relay_store() -> (
    std::sync::Arc<dyn proxenos::auth::store::AccountStore>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = std::sync::Arc::new(proxenos::auth::store::FileStore::new(
        dir.path().join("credentials.json"),
    ));
    store
        .add_key("work", "probe-key", proxenos::auth::store::Provider::Codex)
        .expect("the codex account");
    store
        .add_key(
            "personal-claude",
            "probe-key",
            proxenos::auth::store::Provider::Anthropic,
        )
        .expect("the anthropic account");
    store.select("work").expect("the serving account");
    (
        store as std::sync::Arc<dyn proxenos::auth::store::AccountStore>,
        dir,
    )
}

fn selected_account(store: &dyn proxenos::auth::store::AccountStore) -> Option<String> {
    store
        .accounts()
        .expect("the store should read")
        .into_iter()
        .find(|account| account.selected)
        .map(|account| account.name)
}

fn authorizer_for(
    store: &std::sync::Arc<dyn proxenos::auth::store::AccountStore>,
) -> std::sync::Arc<dyn proxenos::auth::authorize::Authorizer> {
    std::sync::Arc::new(proxenos::auth::authorize::AccountAuthorizer::new(
        std::sync::Arc::clone(store),
        std::sync::Arc::new(proxenos::auth::tokens::TokenSource::new(
            std::sync::Arc::clone(store)
                as std::sync::Arc<dyn proxenos::auth::store::CredentialStore>,
            proxenos::auth::flow::token_endpoint(),
            proxenos::auth::flow::CLIENT_ID,
            std::sync::Arc::new(proxenos::auth::tokens::SystemClock),
        )),
    ))
}

/// A loopback stand-in for the second provider, answering the probe's marker.
async fn relay_backend() -> (
    String,
    std::sync::Arc<std::sync::Mutex<Option<axum::http::HeaderMap>>>,
) {
    let marker = proxenos::doctor::answer_marker(
        &probe::all()
            .into_iter()
            .find(|probe| probe.name == "relay")
            .expect("the relay probe"),
    )
    .expect("the relay probe requires a marker in the answer");
    let stream = format!(
        "event: message_start\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\n",
        serde_json::json!({
            "type": "message_start",
            "message": {"id": "msg_probe", "type": "message", "role": "assistant",
                        "model": "probe", "content": [], "stop_reason": null,
                        "usage": {"input_tokens": 1, "output_tokens": 1}}
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": marker}
        })
    );
    let seen: std::sync::Arc<std::sync::Mutex<Option<axum::http::HeaderMap>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let sink = std::sync::Arc::clone(&seen);
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |headers: axum::http::HeaderMap, _body: String| {
            if let Ok(mut sink) = sink.lock() {
                *sink = Some(headers);
            }
            let stream = stream.clone();
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    stream,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let addr = listener.local_addr().expect("the bound port");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/v1/messages"), seen)
}

/// The launch surface still emits the two variables the client cannot work
/// without.
///
/// Both were settled live and both fail silently: without `ENABLE_TOOL_SEARCH`
/// the client disables deferred tool loading on a custom base URL and hands
/// back a context the deferral was there to save, and without
/// `CLAUDE_CODE_DISABLE_1M_CONTEXT` it appends `[1m]` to an id it does not
/// recognize and assumes four times the window the model has. A regression in
/// either presents as a broken-looking client over a fully green matrix.
#[tokio::test]
async fn the_env_contract_probe_passes_against_the_launch_surface() {
    let outcome = run_via_doctor("env-contract").await;
    assert_eq!(outcome.status, Status::Passed, "{outcome:?}");
}

/// It fails when the deferral override stops being emitted.
///
/// This is the regression the probe exists for, applied to the rendered
/// environment rather than to the flag behind it: what the client reads is the
/// environment, and a probe asserting the switch would pass over a launch that
/// never emitted it.
#[test]
fn the_env_contract_probe_fails_when_the_deferral_override_is_dropped() {
    let translating = vec![("CLAUDE_CODE_DISABLE_1M_CONTEXT".to_owned(), "1".to_owned())];
    let relayed: Vec<(String, String)> = Vec::new();

    match probe::check_environment(&translating, &relayed) {
        Status::Failed(reason) => assert!(reason.contains("ENABLE_TOOL_SEARCH"), "{reason}"),
        other => panic!("a dropped override must fail the probe, got {other:?}"),
    }
}

/// The window flag is asserted in both directions.
///
/// It is emitted only where at least one tier translates (§7.2). Missing on a
/// translating mapping is a fabricated million-token window; present on an
/// all-relay one is an entitlement stripped from ids the client recognizes
/// itself.
#[test]
fn the_env_contract_probe_asserts_the_window_flag_both_ways() {
    let search = || ("ENABLE_TOOL_SEARCH".to_owned(), "true".to_owned());
    let flag = || ("CLAUDE_CODE_DISABLE_1M_CONTEXT".to_owned(), "1".to_owned());

    // Missing where a tier translates.
    match probe::check_environment(&[search()], &[search()]) {
        Status::Failed(reason) => assert!(
            reason.contains("CLAUDE_CODE_DISABLE_1M_CONTEXT"),
            "{reason}"
        ),
        other => panic!("a missing window flag must fail, got {other:?}"),
    }

    // Present where every tier is relayed.
    match probe::check_environment(&[search(), flag()], &[search(), flag()]) {
        Status::Failed(reason) => assert!(
            reason.contains("CLAUDE_CODE_DISABLE_1M_CONTEXT"),
            "{reason}"
        ),
        other => panic!("a window flag on an all-relay mapping must fail, got {other:?}"),
    }

    // And the contract itself passes.
    assert_eq!(
        probe::check_environment(&[search(), flag()], &[search()]),
        Status::Passed
    );
}

/// Under `--live` the row says the backend was not billed for it, the same way
/// `count-tokens` does. The header's claim is true of every other row and false
/// of this one.
#[tokio::test]
async fn a_live_env_contract_row_says_it_never_reached_the_backend() {
    let outcomes = proxenos::doctor::run(&Corpus::Dir(corpus()), Some("env-contract"))
        .await
        .expect("the probe should be known");

    let rendered = probe::matrix(
        &outcomes,
        &probe::Run {
            evidence: probe::Evidence::Live {
                account: Some("work".to_owned()),
                relay: None,
            },
        },
    );
    assert!(
        rendered.contains(probe::NEVER_REACHES_THE_BACKEND),
        "{rendered}"
    );
}
