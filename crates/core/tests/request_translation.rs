//! `docs/proxy-behavior.md` §2 — request translation.
//!
//! Expected values are the spec's, not a recomputation of what the code does.

// clippy's in-test detection covers `#[test]` functions and `#[cfg(test)]`
// modules, neither of which a helper in an integration-test file is. A panic
// here is an assertion.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy_core::anthropic::MessagesRequest;
use codex_cc_proxy_core::translate::TranslateOptions;
use codex_cc_proxy_core::translate::discovered_tool_names;
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

/// §2.1 — a message with any role other than user or assistant is carried as a
/// user item. The backend rejects system and developer roles inside `input`,
/// but nothing requires the content to leave the conversation.
///
/// Folding it into `instructions` looked equivalent and is not: the client
/// sends per-turn content this way, so instructions changed on every turn.
#[test]
fn a_non_conversational_role_becomes_a_user_item() {
    let out = translate(json!({
        "model": "gpt-5",
        "system": "Base prompt.",
        "messages": [
            { "role": "developer", "content": "Prefer tabs." },
            { "role": "user", "content": "hello" },
        ],
    }));

    // The system prompt alone, and nothing that varies per turn.
    assert_eq!(out["instructions"], json!("Base prompt."));

    assert_eq!(out["input"].as_array().map(Vec::len), Some(2));
    assert_eq!(out["input"][0]["role"], json!("user"));
    assert_eq!(out["input"][0]["content"][0]["text"], json!("Prefer tabs."));
    // And never the role the backend refuses.
    for item in out["input"].as_array().unwrap() {
        assert_ne!(item["role"], json!("developer"));
        assert_ne!(item["role"], json!("system"));
    }
}

/// §2.1 and §4.3 — instructions stay identical across turns, whatever the
/// client attaches to individual ones.
///
/// A delta requires every non-input field to be unchanged, and the cached
/// prefix requires the same. Anything that varies per turn must live in the
/// input, where it appends.
#[test]
fn instructions_do_not_change_between_turns() {
    let first = translate(json!({
        "model": "gpt-5",
        "system": "Base prompt.",
        "messages": [
            { "role": "system", "content": "billing-header: build-1" },
            { "role": "user", "content": "hello" },
        ],
    }));

    let second = translate(json!({
        "model": "gpt-5",
        "system": "Base prompt.",
        "messages": [
            { "role": "system", "content": "billing-header: build-1" },
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": "hi" },
            { "role": "system", "content": "billing-header: build-2" },
            { "role": "user", "content": "again" },
        ],
    }));

    assert_eq!(first["instructions"], second["instructions"]);
    // And the second turn is an extension of the first, which is what lets it
    // upload only what is new.
    let before = first["input"].as_array().unwrap();
    let after = second["input"].as_array().unwrap();
    assert_eq!(&after[..before.len()], &before[..]);
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

/// §2.7 — the fields every request carries regardless of what came in.
#[test]
fn every_request_sets_the_fixed_fields() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
    }));

    assert_eq!(out["stream"], json!(true));
    assert_eq!(out["store"], json!(false));
    assert_eq!(out["parallel_tool_calls"], json!(true));
    // Without this the model's reasoning cannot be carried into the next turn
    // (§3.3), and every turn restarts its thinking from nothing.
    assert_eq!(out["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(out["reasoning"]["summary"], json!("auto"));
}

/// §2.7 — effort comes from the request. An absent or unrecognized value is
/// omitted rather than defaulted: the backend's own default is a better guess
/// than ours, and inventing one silently changes what the user asked for.
#[rstest]
#[case(json!({ "effort": "low" }), Some("low"))]
#[case(json!({ "effort": "high" }), Some("high"))]
#[case(json!({ "effort": "xhigh" }), Some("xhigh"))]
#[case(json!({ "effort": "enthusiastic" }), None)]
#[case(json!({}), None)]
fn effort_derives_from_output_config(#[case] config: Value, #[case] expected: Option<&str>) {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "output_config": config,
    }));

    assert_eq!(out["reasoning"]["effort"].as_str(), expected);
}

/// §2.7 — the cache key is supplied by the caller and is stable for the life of
/// a conversation. Cache hit rate depends on it directly.
#[test]
fn the_prompt_cache_key_comes_from_the_session() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
    }))
    .unwrap();

    let options = TranslateOptions {
        prompt_cache_key: Some("session-77".to_owned()),
        ..TranslateOptions::default()
    };
    let out = serde_json::to_value(translate_request(&request, &options)).unwrap();

    assert_eq!(out["prompt_cache_key"], json!("session-77"));
}

/// §2.7 — unsupported inbound parameters are dropped through an allowlist
/// rather than forwarded. `cache_control` has no equivalent; upstream caching
/// is implicit.
#[test]
fn unsupported_parameters_are_dropped() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "temperature": 0.7,
        "top_k": 5,
        "metadata": { "user_id": "u1" },
        "thinking": { "type": "enabled", "budget_tokens": 1024 },
    }));

    for dropped in ["temperature", "top_k", "metadata", "thinking"] {
        assert_eq!(out.get(dropped), None, "{dropped} should not be forwarded");
    }
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

/// §2.2 — a base64 image in a user message becomes an `input_image` data URL.
#[test]
fn a_base64_image_becomes_a_data_url() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "TUFSSzc3" },
            }],
        }],
    }));

    assert_eq!(
        out["input"][0]["content"][0],
        json!({ "type": "input_image", "image_url": "data:image/png;base64,TUFSSzc3" })
    );
}

/// §2.2 — a document directly in a user message becomes an `input_file` part
/// in place, with no trailing message: it is already in the only position where
/// `input_file` is defined.
#[test]
fn a_document_in_a_user_message_becomes_an_input_file_part() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "summarize" },
                {
                    "type": "document",
                    "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": "TUFSSzc3",
                    },
                },
            ],
        }],
    }));

    assert_eq!(out["input"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        out["input"][0]["content"][1],
        json!({
            "type": "input_file",
            "filename": "attachment.pdf",
            "file_data": "data:application/pdf;base64,TUFSSzc3",
        })
    );
}

/// §2.2 — a URL source passes through unchanged and is not prefetched.
#[test]
fn an_image_url_passes_through() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image",
                "source": { "type": "url", "url": "https://example.invalid/a.png" },
            }],
        }],
    }));

    assert_eq!(
        out["input"][0]["content"][0]["image_url"],
        json!("https://example.invalid/a.png")
    );
}

/// §2.3 — an image nested in a `tool_result` travels inside the output itself.
/// This is how every image the client reads arrives, and dropping it produces a
/// model that describes the file from its name in wording that reads as
/// success.
#[test]
fn an_image_in_a_tool_result_travels_inside_the_output() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01",
                "content": [
                    { "type": "text", "text": "Read 1 image" },
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "TUFSSzc3",
                        },
                    },
                ],
            }],
        }],
    }));

    assert_eq!(
        out["input"][0],
        json!({
            "type": "function_call_output",
            "call_id": "toolu_01",
            "output": [
                { "type": "input_text", "text": "Read 1 image" },
                { "type": "input_image", "image_url": "data:image/png;base64,TUFSSzc3" },
            ],
        })
    );
}

/// §2.3 — the output collapses to a bare string when, and only when, it is a
/// single piece of text. Anything else stays an array.
#[test]
fn a_lone_text_output_collapses_to_a_string() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01",
                "content": [{ "type": "text", "text": "only text" }],
            }],
        }],
    }));

    assert_eq!(out["input"][0]["output"], json!("only text"));
}

/// §2.3 — a document has no representation inside a tool output, so it is
/// re-emitted as a user message placed immediately after the output it came
/// from. The output keeps its text, so the model is never left with a call
/// whose result vanished.
#[test]
fn a_document_in_a_tool_result_follows_as_its_own_message() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01",
                "content": [
                    { "type": "text", "text": "Read invoice.pdf" },
                    {
                        "type": "document",
                        "source": {
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": "TUFSSzc3",
                        },
                    },
                ],
            }],
        }],
    }));

    assert_eq!(out["input"][0]["type"], json!("function_call_output"));
    assert_eq!(out["input"][0]["output"], json!("Read invoice.pdf"));
    assert_eq!(
        out["input"][1],
        json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_file",
                "filename": "attachment.pdf",
                "file_data": "data:application/pdf;base64,TUFSSzc3",
            }],
        })
    );
}

/// §2.2 — an attachment in an assistant message is dropped. Assistant content
/// is `output_text` only.
#[test]
fn an_attachment_in_an_assistant_message_is_dropped() {
    let out = translate(json!({
        "model": "gpt-5",
        "messages": [
            { "role": "user", "content": "hi" },
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "here" },
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "TUFSSzc3",
                        },
                    },
                ],
            },
        ],
    }));

    assert_eq!(
        out["input"][1]["content"],
        json!([{ "type": "output_text", "text": "here" }])
    );
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

/// §2.5 — discovery is observable exactly once, in the `tool_reference` blocks
/// of a search result. The session records what it sees there; nothing later in
/// the conversation says it again.
#[test]
fn discovered_names_are_recoverable_from_a_request() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "messages": [
            { "role": "user", "content": "find a tool" },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01",
                    "content": [
                        { "type": "tool_reference", "name": "Slack" },
                        { "type": "tool_reference", "name": "Jira" },
                    ],
                }],
            },
        ],
    }))
    .unwrap();

    let names = discovered_tool_names(&request);

    assert_eq!(
        names,
        ["Jira".to_owned(), "Slack".to_owned()]
            .into_iter()
            .collect()
    );
}

/// A conversation with no search result discovers nothing.
#[test]
fn a_conversation_without_a_search_discovers_nothing() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
    }))
    .unwrap();

    assert!(discovered_tool_names(&request).is_empty());
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

/// §2.7 — an operator's effort ceiling applies to traffic that expresses no
/// preference of its own, which is most of it. The client cannot choose this:
/// it does not know whose quota it is spending.
#[test]
fn an_effort_ceiling_applies_when_the_request_asks_for_nothing() {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
    }))
    .unwrap();

    let options = TranslateOptions {
        effort_ceiling: Some(codex_cc_proxy_core::responses::Effort::Low),
        ..TranslateOptions::default()
    };
    let out = serde_json::to_value(translate_request(&request, &options)).unwrap();

    assert_eq!(out["reasoning"]["effort"], json!("low"));
}

/// A ceiling caps, it does not raise. A request asking for less than the
/// ceiling keeps its own choice — capping the maximum is not a request to
/// spend more.
#[rstest]
#[case("minimal", "minimal")]
#[case("low", "low")]
#[case("high", "low")]
#[case("max", "low")]
fn a_ceiling_caps_without_raising(#[case] requested: &str, #[case] expected: &str) {
    let request: MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": "hello" }],
        "output_config": { "effort": requested },
    }))
    .unwrap();

    let options = TranslateOptions {
        effort_ceiling: Some(codex_cc_proxy_core::responses::Effort::Low),
        ..TranslateOptions::default()
    };
    let out = serde_json::to_value(translate_request(&request, &options)).unwrap();

    assert_eq!(out["reasoning"]["effort"], json!(expected));
}

/// The ordering the cap relies on. A value out of place would silently let a
/// request exceed the ceiling an operator set.
#[test]
fn efforts_are_ordered_from_least_to_most() {
    use codex_cc_proxy_core::responses::Effort;

    let ascending = [
        Effort::None,
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ];

    for pair in ascending.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{:?} should be less than {:?}",
            pair[0],
            pair[1]
        );
    }
}

/// §2.1 — the operator may lead and follow the system prompt with text of their
/// own.
///
/// The prompt the client sends is written for a different model and says so in
/// its opening line. Nothing else in the request tells the model what it
/// actually is, and nothing in the client can be made to.
///
/// Order is the whole design. The lead comes first because an identity stated
/// after a prompt that already asserted a different one reads as a correction
/// rather than a fact. The trailer comes last because last is what an
/// instruction has to be in order to take precedence over the prompt above it.
#[test]
fn the_operator_may_lead_and_follow_the_system_prompt() {
    let request: codex_cc_proxy_core::anthropic::MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "max_tokens": 1024,
        "system": "You are Claude Code.",
        "messages": [{ "role": "user", "content": "hello" }],
    }))
    .unwrap();

    let out = serde_json::to_value(translate_request(
        &request,
        &TranslateOptions {
            instructions_lead: Some("You are gpt-5.".to_owned()),
            instructions_trailer: Some("Answer briefly.".to_owned()),
            ..TranslateOptions::default()
        },
    ))
    .unwrap();

    assert_eq!(
        out["instructions"],
        json!("You are gpt-5.\n\nYou are Claude Code.\n\nAnswer briefly.")
    );
}

/// A request carrying no system prompt still gets the operator's text, and no
/// blank run where the prompt would have been.
#[test]
fn operator_text_stands_alone_without_a_system_prompt() {
    let request: codex_cc_proxy_core::anthropic::MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "max_tokens": 1024,
        "messages": [{ "role": "user", "content": "hello" }],
    }))
    .unwrap();

    let out = serde_json::to_value(translate_request(
        &request,
        &TranslateOptions {
            instructions_lead: Some("You are gpt-5.".to_owned()),
            instructions_trailer: Some("Answer briefly.".to_owned()),
            ..TranslateOptions::default()
        },
    ))
    .unwrap();

    assert_eq!(
        out["instructions"],
        json!("You are gpt-5.\n\nAnswer briefly.")
    );
}

/// §4.3 — injected text is per conversation, never per turn.
///
/// `instructions` must be byte-identical across turns or the delta is refused
/// and `prompt_cache_key` buys nothing. Text that varies — a timestamp, a
/// token count — would cost the whole incremental path, so this asserts the
/// injected form is stable for a conversation that has grown.
#[test]
fn injected_instructions_are_identical_on_a_later_turn() {
    let options = TranslateOptions {
        instructions_lead: Some("You are gpt-5.".to_owned()),
        instructions_trailer: Some("Answer briefly.".to_owned()),
        ..TranslateOptions::default()
    };

    let first: codex_cc_proxy_core::anthropic::MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "max_tokens": 1024,
        "system": "Base prompt.",
        "messages": [{ "role": "user", "content": "one" }],
    }))
    .unwrap();
    let later: codex_cc_proxy_core::anthropic::MessagesRequest = serde_json::from_value(json!({
        "model": "gpt-5",
        "max_tokens": 1024,
        "system": "Base prompt.",
        "messages": [
            { "role": "user", "content": "one" },
            { "role": "assistant", "content": "two" },
            { "role": "user", "content": "three" },
        ],
    }))
    .unwrap();

    assert_eq!(
        translate_request(&first, &options).instructions,
        translate_request(&later, &options).instructions
    );
}
