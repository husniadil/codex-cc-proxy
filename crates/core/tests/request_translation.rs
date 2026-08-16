//! `docs/proxy-behavior.md` §2 — request translation.
//!
//! Expected values are the spec's, not a recomputation of what the code does.

// clippy's in-test detection covers `#[test]` functions and `#[cfg(test)]`
// modules, neither of which a helper in an integration-test file is. A panic
// here is an assertion.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy_core::anthropic::MessagesRequest;
use codex_cc_proxy_core::translate::TranslateOptions;
use codex_cc_proxy_core::translate::translate_request;
use pretty_assertions::assert_eq;
use rstest::rstest;
use serde_json::Value;
use serde_json::json;

fn translate(request: Value) -> Value {
    let request: MessagesRequest =
        serde_json::from_value(request).expect("request should deserialize");
    let translated = translate_request(&request, &TranslateOptions::default());
    serde_json::to_value(translated).expect("translation should serialize")
}

/// §2.1 — the system prompt maps to `instructions`, never to an input item.
#[test]
fn system_prompt_becomes_instructions() {
    let out = translate(json!({
        "model": "gpt-5",
        "max_tokens": 1024,
        "system": "You are Claude Code.",
        "messages": [{ "role": "user", "content": "hello" }],
    }));

    assert_eq!(out["instructions"], json!("You are Claude Code."));
    assert_eq!(
        out["input"],
        json!([{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "hello" }],
        }])
    );
}

/// §2.1 — `system` also arrives as a list of blocks. Claude Code sends it that
/// way whenever it attaches `cache_control` to part of the prompt, which is
/// most turns.
#[test]
fn system_blocks_join_into_one_instructions_string() {
    let out = translate(json!({
        "model": "gpt-5",
        "system": [
            { "type": "text", "text": "You are Claude Code." },
            { "type": "text", "text": "Be concise.", "cache_control": { "type": "ephemeral" } },
        ],
        "messages": [{ "role": "user", "content": "hello" }],
    }));

    assert_eq!(
        out["instructions"],
        json!("You are Claude Code.\n\nBe concise.")
    );
}

/// §2.2 — list-form content, each block mapped in order.
#[test]
fn user_text_blocks_become_input_text_parts() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" },
            ],
        }],
    }));

    assert_eq!(
        out["input"],
        json!([{
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": "first" },
                { "type": "input_text", "text": "second" },
            ],
        }])
    );
}

/// §2.2 — assistant text is `output_text`, not `input_text`. Sending assistant
/// turns as input text loses the distinction between what the model said and
/// what it was told.
#[test]
fn assistant_text_becomes_output_text() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": [{ "type": "text", "text": "hi" }] },
        ],
    }));

    assert_eq!(
        out["input"][1],
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "hi" }],
        })
    );
}

/// §2.2 — `thinking` has no equivalent and is dropped. A message left with no
/// content at all is dropped with it rather than sent empty.
#[test]
fn thinking_blocks_are_dropped() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [
            { "role": "user", "content": "hello" },
            {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "...", "signature": "abc" },
                    { "type": "redacted_thinking", "data": "..." },
                    { "type": "text", "text": "hi" },
                ],
            },
        ],
    }));

    assert_eq!(
        out["input"][1]["content"],
        json!([{ "type": "output_text", "text": "hi" }])
    );
}

/// §2.2 — an assistant message that carried nothing but thinking leaves no item
/// behind.
#[test]
fn a_message_emptied_by_translation_is_dropped() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [
            { "role": "user", "content": "hello" },
            {
                "role": "assistant",
                "content": [{ "type": "thinking", "thinking": "...", "signature": "abc" }],
            },
        ],
    }));

    assert_eq!(out["input"].as_array().map(Vec::len), Some(1));
}

/// §2.4 — function tools flatten, and `input_schema` becomes `parameters`.
#[test]
fn function_tools_flatten() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "tools": [{
            "name": "Read",
            "description": "Read a file",
            "input_schema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            },
        }],
    }));

    assert_eq!(
        out["tools"],
        json!([{
            "type": "function",
            "name": "Read",
            "description": "Read a file",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            },
        }])
    );
}

/// Strict mode is never asserted. It requires schemas that satisfy constraints
/// the client's tool schemas do not — every property required, no additional
/// properties — and claiming it over a schema that does not comply is a request
/// rejection, not a stricter model.
#[test]
fn tools_never_claim_strict_mode() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "tools": [{
            "name": "Grep",
            "input_schema": {
                "type": "object",
                "properties": { "pattern": { "type": "string" }, "path": { "type": "string" } },
                "required": ["pattern"],
            },
        }],
    }));

    assert_eq!(out["tools"][0]["strict"], json!(false));
}

/// §2.4 — a schema with no `properties` gains an empty one. Some backends
/// reject an object schema without it, and a rejected tool list fails the whole
/// request rather than the one tool.
#[test]
fn a_schema_without_properties_gains_an_empty_one() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "tools": [{ "name": "Now", "input_schema": { "type": "object" } }],
    }));

    assert_eq!(out["tools"][0]["parameters"]["properties"], json!({}));
}

/// §2.6 — any tool whose `type` begins with `web_search` is the server-side
/// search tool. Translating it as a function produces a tool the model cannot
/// execute and a search that silently returns nothing.
#[rstest]
#[case("web_search_20250305")]
#[case("web_search_20260209")]
fn web_search_maps_to_the_native_tool(#[case] tool_type: &str) {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "tools": [{ "type": tool_type, "name": "web_search" }],
    }));

    // Both access flags are stated rather than defaulted. If either defaulted
    // to false, search would return nothing and the client would report "no
    // results" — the exact silent failure this proxy exists to prevent.
    assert_eq!(
        out["tools"],
        json!([{
            "type": "web_search",
            "external_web_access": true,
            "indexed_web_access": true,
        }])
    );
}

/// §2.4 — `tool_choice` mapping. Anything the spec does not name is `auto`,
/// because a choice the backend does not understand fails the request.
#[rstest]
#[case(json!({ "type": "auto" }), json!("auto"))]
#[case(json!({ "type": "any" }), json!("required"))]
#[case(json!({ "type": "none" }), json!("auto"))]
#[case(json!({ "type": "tool", "name": "Read" }), json!({ "type": "function", "name": "Read" }))]
fn tool_choice_maps(#[case] input: Value, #[case] expected: Value) {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "tools": [{ "name": "Read", "input_schema": { "type": "object" } }],
        "tool_choice": input,
    }));

    assert_eq!(out["tool_choice"], expected);
}

/// §2.2 — `tool_use` becomes `function_call`, with `input` serialized into the
/// `arguments` string the backend expects.
#[test]
fn tool_use_becomes_a_function_call() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [
            { "role": "user", "content": "read it" },
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_01",
                    "name": "Read",
                    "input": { "path": "/etc/hosts" },
                }],
            },
        ],
    }));

    assert_eq!(
        out["input"][1],
        json!({
            "type": "function_call",
            "call_id": "toolu_01",
            "name": "Read",
            "arguments": "{\"path\":\"/etc/hosts\"}",
        })
    );
}

/// §2.2 — a `tool_use` is its own input item, not a content part, so an
/// assistant turn mixing prose and a call produces two items in order.
#[test]
fn an_assistant_turn_with_prose_and_a_call_produces_two_items() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [
            { "role": "user", "content": "read it" },
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Reading." },
                    { "type": "tool_use", "id": "toolu_01", "name": "Read", "input": {} },
                ],
            },
        ],
    }));

    assert_eq!(out["input"].as_array().map(Vec::len), Some(3));
    assert_eq!(out["input"][1]["type"], json!("message"));
    assert_eq!(out["input"][2]["type"], json!("function_call"));
}

/// §2.2 — `tool_result` becomes `function_call_output`, keyed by the same id.
#[test]
fn tool_result_becomes_a_function_call_output() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01",
                "content": [{ "type": "text", "text": "127.0.0.1 localhost" }],
            }],
        }],
    }));

    assert_eq!(
        out["input"][0],
        json!({
            "type": "function_call_output",
            "call_id": "toolu_01",
            "output": "127.0.0.1 localhost",
        })
    );
}

/// A `tool_result` whose content is a bare string carries it through unchanged.
#[test]
fn a_string_tool_result_carries_through() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01",
                "content": "done",
            }],
        }],
    }));

    assert_eq!(out["input"][0]["output"], json!("done"));
}

/// §2.5 — a tool-search result has no text content, only `tool_reference`
/// blocks. Its output carries the discovered names as JSON so the output is
/// non-empty and the model can tell which tools it may now call. An empty
/// output would leave the model unable to act on a search it just ran.
#[test]
fn a_tool_search_result_reports_the_discovered_names() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01",
                "content": [
                    { "type": "tool_reference", "name": "Slack" },
                    { "type": "tool_reference", "name": "Jira" },
                ],
            }],
        }],
    }));

    assert_eq!(
        out["input"][0]["output"],
        json!("{\"available_tools\":[\"Slack\",\"Jira\"]}")
    );
}

/// §2.5 — undiscovered tools are withheld so their schemas do not occupy
/// context.
#[test]
fn deferred_tools_are_withheld() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "tools": [
            { "name": "Read", "input_schema": { "type": "object" } },
            { "name": "Slack", "input_schema": { "type": "object" }, "defer_loading": true },
        ],
    }));

    assert_eq!(out["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(out["tools"][0]["name"], json!("Read"));
}

/// §2.5 — a tool discovered earlier in the session is forwarded even though it
/// still arrives marked `defer_loading`. The client never clears that flag, so
/// the recorded set is the only signal that a tool is live. Trusting the flag
/// alone leaves every discovered tool permanently uncallable.
#[test]
fn a_discovered_tool_is_forwarded_despite_still_being_marked_deferred() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "tools": [
            { "name": "Slack", "input_schema": { "type": "object" }, "defer_loading": true },
            { "name": "Jira", "input_schema": { "type": "object" }, "defer_loading": true },
        ],
    }))
    .unwrap();

    let options = TranslateOptions {
        discovered_tools: ["Slack".to_owned()].into_iter().collect(),
        ..TranslateOptions::default()
    };
    let out = serde_json::to_value(translate_request(&request, &options)).unwrap();

    assert_eq!(out["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(out["tools"][0]["name"], json!("Slack"));
}
