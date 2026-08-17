//! `docs/proxy-behavior.md` §3.1 and §9.4 — session identity and delta
//! invariants.
//!
//! These are the invariants the transports are held to. Incremental upload is
//! the one subsystem whose bugs corrupt conversations instead of failing
//! loudly, so each rule is stated here before any transport uses it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy_core::responses::InputItem;
use codex_cc_proxy_core::session::Baseline;
use codex_cc_proxy_core::session::Plan;
use codex_cc_proxy_core::session::delta;
use codex_cc_proxy_core::session::extends;
use pretty_assertions::assert_eq;
use serde_json::json;

fn items(values: serde_json::Value) -> Vec<InputItem> {
    serde_json::from_value(values).expect("items should deserialize")
}

fn message(text: &str) -> serde_json::Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": text }],
    })
}

/// A valid delta contains exactly the new items.
#[test]
fn a_delta_is_exactly_what_is_new() {
    let baseline = items(json!([message("one"), message("two")]));
    let candidate = items(json!([message("one"), message("two"), message("three")]));

    let new_items = delta(&baseline, &candidate).expect("this extends the baseline");

    assert_eq!(new_items.len(), 1);
    assert_eq!(
        serde_json::to_value(&new_items[0]).unwrap()["content"][0]["text"],
        json!("three")
    );
}

/// An unchanged conversation adds nothing, and that is a valid delta rather
/// than a mismatch.
#[test]
fn an_unchanged_conversation_yields_an_empty_delta() {
    let baseline = items(json!([message("one")]));
    let candidate = items(json!([message("one")]));

    assert_eq!(delta(&baseline, &candidate).map(<[_]>::len), Some(0));
}

/// A non-extending input forces a full send. This is the case that corrupts a
/// conversation if it is got wrong, because the result still looks like a
/// well-formed request.
#[test]
fn an_edited_history_is_not_an_extension() {
    let baseline = items(json!([message("one"), message("two")]));
    let candidate = items(json!([message("one"), message("EDITED"), message("three")]));

    assert!(!extends(&baseline, &candidate));
    assert_eq!(delta(&baseline, &candidate), None);
}

/// A conversation that lost a turn is not an extension either. Compaction and
/// rewind both produce this.
#[test]
fn a_shortened_history_is_not_an_extension() {
    let baseline = items(json!([message("one"), message("two"), message("three")]));
    let candidate = items(json!([message("one"), message("two")]));

    assert!(!extends(&baseline, &candidate));
}

/// Two conversations sharing an opening are indistinguishable until they
/// diverge, and matching them is harmless: the shared prefix is identical, so
/// the baseline is correct for both. What must not happen is a match on a
/// *partial* prefix.
#[test]
fn a_shared_prefix_matches_until_it_diverges() {
    let baseline = items(json!([message("shared opening")]));

    let one = items(json!([message("shared opening"), message("branch A")]));
    let two = items(json!([message("shared opening"), message("branch B")]));

    assert!(extends(&baseline, &one));
    assert!(extends(&baseline, &two));

    // Once either has diverged, the other no longer extends it.
    let after_a = items(json!([message("shared opening"), message("branch A")]));
    assert!(!extends(&after_a, &two));
}

/// Server-assigned ids are ignored in the comparison. They are absent when the
/// client replays the conversation, so comparing them would make every turn
/// look like a divergence and defeat the delta entirely.
#[test]
fn server_assigned_ids_do_not_break_the_match() {
    let baseline: Vec<InputItem> = items(json!([{
        "type": "function_call",
        "id": "fc_server_assigned",
        "call_id": "call_1",
        "name": "Read",
        "arguments": "{}",
    }]));

    let candidate: Vec<InputItem> = items(json!([
        { "type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{}" },
        message("next"),
    ]));

    assert!(extends(&baseline, &candidate));
    assert_eq!(delta(&baseline, &candidate).map(<[_]>::len), Some(1));
}

/// A difference that is not the id still counts. Ignoring ids must not become
/// ignoring content.
#[test]
fn ignoring_ids_does_not_ignore_content() {
    let baseline: Vec<InputItem> = items(json!([{
        "type": "function_call",
        "call_id": "call_1",
        "name": "Read",
        "arguments": "{\"path\":\"/a\"}",
    }]));

    let candidate: Vec<InputItem> = items(json!([{
        "type": "function_call",
        "call_id": "call_1",
        "name": "Read",
        "arguments": "{\"path\":\"/b\"}",
    }]));

    assert!(!extends(&baseline, &candidate));
}

/// Server-returned items are part of the baseline and are never resent.
#[test]
fn server_returned_items_are_never_resent() {
    let mut baseline = Baseline::new();

    let sent = items(json!([message("ask")]));
    let returned = items(json!([{
        "type": "function_call",
        "call_id": "call_1",
        "name": "Read",
        "arguments": "{}",
    }]));
    baseline.advance(&sent, &returned);

    // The next turn replays everything, including what the server produced.
    let candidate = items(json!([
        message("ask"),
        { "type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{}" },
        { "type": "function_call_output", "call_id": "call_1", "output": "contents" },
    ]));

    match baseline.plan(&candidate) {
        Plan::Delta(new_items) => {
            assert_eq!(new_items.len(), 1, "only the tool output is new");
            assert_eq!(
                serde_json::to_value(&new_items[0]).unwrap()["type"],
                json!("function_call_output")
            );
        }
        Plan::Full => panic!("this should have been a delta"),
    }
}

/// An empty baseline extends into anything: the first turn of a session is a
/// full send by definition.
#[test]
fn the_first_turn_is_a_full_send() {
    let baseline = Baseline::new();
    let candidate = items(json!([message("first")]));

    assert_eq!(baseline.plan(&candidate), Plan::Delta(&candidate[..]));
    assert!(baseline.is_empty());
}

/// A full send is always valid, whatever the baseline says.
#[test]
fn a_full_send_is_always_available() {
    let mut baseline = Baseline::new();
    baseline.advance(&items(json!([message("one")])), &[]);

    let unrelated = items(json!([message("something else entirely")]));
    assert_eq!(baseline.plan(&unrelated), Plan::Full);
}

/// §3.3 — retained reasoning is part of the baseline in exactly the way
/// server-returned items are, so the incremental and full-send paths agree on
/// what the conversation contains.
#[test]
fn retained_reasoning_joins_the_baseline_in_position() {
    let mut baseline = Baseline::new();

    let sent = items(json!([message("ask")]));
    let returned = items(json!([
        {
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "OPAQUE",
        },
        {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "answer" }],
        },
    ]));
    baseline.advance(&sent, &returned);

    assert_eq!(baseline.len(), 3);
    assert_eq!(
        serde_json::to_value(&baseline.items()[1]).unwrap()["type"],
        json!("reasoning"),
        "the reasoning item should sit where the server put it"
    );

    // The next turn extends it, and the reasoning is not resent.
    let candidate = items(json!([
        message("ask"),
        { "type": "reasoning", "id": "rs_1", "encrypted_content": "OPAQUE" },
        {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "answer" }],
        },
        message("follow up"),
    ]));

    match baseline.plan(&candidate) {
        Plan::Delta(new_items) => assert_eq!(new_items.len(), 1),
        Plan::Full => panic!("retained reasoning should not have forced a full send"),
    }
}

/// §3.3 — a baseline holding server-only reasoning still matches the client's
/// replay, and the reasoning is put back where the server produced it.
///
/// Without this every turn after the first reasoning item is a full send: the
/// client cannot replay reasoning, so the baseline and the replay disagree at
/// that position forever.
#[test]
fn reasoning_in_the_baseline_does_not_break_the_match() {
    let baseline = items(json!([
        message("first question"),
        { "type": "reasoning", "id": "rs_1", "encrypted_content": "OPAQUE" },
        {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "first answer" }],
        },
    ]));

    // What the client replays: no reasoning, because it never received any.
    let replay = items(json!([
        message("first question"),
        {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "first answer" }],
        },
        message("second question"),
    ]));

    let reconciled = codex_cc_proxy_core::session::reconcile(&baseline, &replay)
        .expect("the replay should line up around the reasoning item");

    assert_eq!(reconciled.new_items, 1, "only the new question is new");

    // The reasoning is still in place, and still where the server put it.
    assert_eq!(reconciled.input.len(), 4);
    assert_eq!(
        serde_json::to_value(&reconciled.input[1]).unwrap()["type"],
        json!("reasoning")
    );
    assert_eq!(
        serde_json::to_value(&reconciled.input[1]).unwrap()["encrypted_content"],
        json!("OPAQUE")
    );
}

/// A replay that genuinely diverges still refuses to reconcile. Tolerating
/// server-only items must not become tolerating edits.
#[test]
fn reconciliation_still_refuses_an_edited_history() {
    let baseline = items(json!([
        message("first question"),
        { "type": "reasoning", "id": "rs_1", "encrypted_content": "OPAQUE" },
    ]));

    let edited = items(json!([message("a different question"), message("next")]));

    assert_eq!(
        codex_cc_proxy_core::session::reconcile(&baseline, &edited),
        None
    );
}

/// A shortened replay does not reconcile either.
#[test]
fn reconciliation_refuses_a_shortened_history() {
    let baseline = items(json!([
        message("one"),
        { "type": "reasoning", "id": "rs_1", "encrypted_content": "OPAQUE" },
        message("two"),
    ]));

    assert_eq!(
        codex_cc_proxy_core::session::reconcile(&baseline, &items(json!([message("one")]))),
        None
    );
}

/// §3.1 — the two rules operate on different forms of the same conversation,
/// and composing them is what makes them agree.
///
/// `reconcile` takes a client replay, which can never contain the server's own
/// reasoning, and returns the conversation as the backend holds it. `plan`
/// takes *that* and compares strictly. Running the reconciling rule twice
/// misaligns precisely the items the first pass put back, which is why this
/// asserts the composition rather than either rule alone.
#[test]
fn the_plan_and_the_match_agree_about_server_only_items() {
    let mut baseline = Baseline::new();
    baseline.advance(
        &items(json!([message("first question")])),
        &items(json!([
            { "type": "reasoning", "id": "rs_1", "encrypted_content": "OPAQUE" },
            {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "answer" }],
            },
        ])),
    );

    let replay = items(json!([
        message("first question"),
        {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "answer" }],
        },
        message("second question"),
    ]));

    // The session matches, and reconciliation says what the backend should see.
    let reconciled = baseline
        .reconcile(&replay)
        .expect("a replay missing only server-only items still continues");
    assert_eq!(reconciled.new_items, 1);

    // Planning against that form yields the same one new item.
    match baseline.plan(&reconciled.input) {
        Plan::Delta(new_items) => assert_eq!(new_items.len(), 1),
        Plan::Full => panic!("the match and the plan disagree"),
    }

    // Planning against the raw replay does not, and must not pretend to: the
    // reasoning item sits where the replay has the answer, and a delta computed
    // there would be wrong rather than merely large.
    assert_eq!(baseline.plan(&replay), Plan::Full);
}

/// §3.3 — a re-injected reasoning item carries every field the backend requires.
///
/// `summary` is required and refusing without it is not a soft failure: the
/// backend rejects the whole request with `missing_required_parameter`, so a
/// turn in which the model reasoned makes the *next* turn fail. Omitting an
/// empty array is the easy mistake, and low effort produces empty summaries.
#[test]
fn a_reasoning_item_keeps_the_fields_the_backend_requires() {
    let items: Vec<InputItem> = serde_json::from_value(json!([{
        "type": "reasoning",
        "id": "rs_1",
        "summary": [],
        "encrypted_content": "OPAQUE",
    }]))
    .unwrap();

    let rendered = serde_json::to_value(&items[0]).unwrap();

    assert!(
        rendered.get("summary").is_some(),
        "summary must survive even when empty: {rendered}"
    );
    assert_eq!(rendered["summary"], json!([]));
    assert_eq!(rendered["encrypted_content"], json!("OPAQUE"));
}

/// And when the server sent no encrypted content at all, the field is still
/// there rather than dropped.
#[test]
fn a_reasoning_item_without_encrypted_content_still_carries_the_field() {
    let items: Vec<InputItem> =
        serde_json::from_value(json!([{ "type": "reasoning", "id": "rs_1" }])).unwrap();

    let rendered = serde_json::to_value(&items[0]).unwrap();

    assert_eq!(rendered["summary"], json!([]));
    assert!(rendered.get("encrypted_content").is_some());
}

/// A tool call whose arguments differ only in key order is the same call.
///
/// Measured against a real client: the backend emitted
/// `{"file_path":…,"content":…}` and the client replayed the same object as
/// `{"content":…,"file_path":…}`. `arguments` is a string on the wire, so a
/// string comparison called those two different calls and forked the
/// conversation — every turn after the model wrote a file uploaded the whole
/// history again.
#[test]
fn a_tool_call_is_the_same_call_however_its_arguments_are_ordered() {
    let server = items(json!([{
        "type": "function_call",
        "call_id": "call_1",
        "name": "Write",
        "arguments": "{\"file_path\":\"/tmp/a\",\"content\":\"hi\"}",
    }]));
    let client = items(json!([{
        "type": "function_call",
        "call_id": "call_1",
        "name": "Write",
        "arguments": "{\"content\":\"hi\",\"file_path\":\"/tmp/a\"}",
    }]));

    assert!(
        extends(&server, &client),
        "the same call written in a different key order must still continue the conversation"
    );
}

/// And a call whose arguments genuinely differ is genuinely different.
///
/// The normalization must not become "any two calls to the same tool match" —
/// that would send a delta for a conversation the backend does not hold.
#[test]
fn arguments_that_differ_in_value_are_a_different_call() {
    let one = items(json!([{
        "type": "function_call",
        "call_id": "call_1",
        "name": "Write",
        "arguments": "{\"file_path\":\"/tmp/a\",\"content\":\"hi\"}",
    }]));
    let two = items(json!([{
        "type": "function_call",
        "call_id": "call_1",
        "name": "Write",
        "arguments": "{\"file_path\":\"/tmp/a\",\"content\":\"bye\"}",
    }]));

    assert!(!extends(&one, &two));
}

/// Arguments that are not JSON at all are compared as they arrived.
#[test]
fn unparseable_arguments_fall_back_to_the_literal_text() {
    let one = items(json!([{
        "type": "function_call", "call_id": "c", "name": "T", "arguments": "not json",
    }]));
    let same = items(json!([{
        "type": "function_call", "call_id": "c", "name": "T", "arguments": "not json",
    }]));
    let other = items(json!([{
        "type": "function_call", "call_id": "c", "name": "T", "arguments": "also not json",
    }]));

    assert!(extends(&one, &same));
    assert!(!extends(&one, &other));
}
