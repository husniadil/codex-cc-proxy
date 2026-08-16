//! `docs/proxy-behavior.md` §5.0 — SSE framing.

// clippy's in-test detection covers `#[test]` functions and `#[cfg(test)]`
// modules, neither of which a helper in an integration-test file is. A panic
// here is an assertion.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use codex_cc_proxy_core::sse::SseDecoder;
use pretty_assertions::assert_eq;

/// Feed the whole input as one chunk and collect every payload.
fn decode(input: &str) -> Vec<String> {
    let mut decoder = SseDecoder::default();
    decoder.push(input.as_bytes()).collect()
}

#[test]
fn one_data_line_is_one_payload() {
    assert_eq!(decode("data: {\"a\":1}\n\n"), vec!["{\"a\":1}".to_owned()]);
}

/// The specification joins multiple `data:` lines in one event with newlines.
/// Parsing each line as its own JSON document corrupts exactly the events large
/// enough to be split — long tool-call arguments and long text deltas.
#[test]
fn several_data_lines_join_into_one_payload() {
    let decoded = decode("data: {\"a\":\ndata: 1}\n\n");
    assert_eq!(decoded, vec!["{\"a\":\n1}".to_owned()]);
}

/// A single leading space after the colon is part of the framing, not the
/// payload. Any further whitespace is content.
#[test]
fn only_one_leading_space_is_stripped() {
    assert_eq!(decode("data:  x\n\n"), vec![" x".to_owned()]);
    assert_eq!(decode("data:x\n\n"), vec!["x".to_owned()]);
}

/// Fields other than `data` do not contribute to the payload.
#[test]
fn other_fields_are_ignored() {
    let decoded = decode("event: response.created\nid: 1\nretry: 10\ndata: {}\n\n");
    assert_eq!(decoded, vec!["{}".to_owned()]);
}

/// A line beginning with a colon is a comment. Some servers send them as
/// keep-alives, and treating one as an event would emit a spurious frame.
#[test]
fn comments_are_ignored() {
    assert_eq!(
        decode(": keep-alive\n\ndata: {}\n\n"),
        vec!["{}".to_owned()]
    );
}

/// An event carrying no `data` field at all produces nothing.
#[test]
fn an_event_without_data_produces_nothing() {
    assert!(decode("event: ping\n\n").is_empty());
}

/// Events arrive split across arbitrary read boundaries, including mid-line and
/// mid-multibyte-character. A decoder that assumes chunks align with events
/// works in tests and drops data against a real socket.
#[test]
fn payloads_survive_arbitrary_chunk_boundaries() {
    let input = "data: {\"text\":\"héllo wörld\"}\n\ndata: {\"b\":2}\n\n";
    let expected = vec![
        "{\"text\":\"héllo wörld\"}".to_owned(),
        "{\"b\":2}".to_owned(),
    ];

    for split in 1..input.len() {
        if !input.is_char_boundary(split) {
            continue;
        }
        let mut decoder = SseDecoder::default();
        let mut out: Vec<String> = decoder.push(&input.as_bytes()[..split]).collect();
        out.extend(decoder.push(&input.as_bytes()[split..]));
        assert_eq!(out, expected, "split at {split}");
    }
}

/// A byte split in the middle of a multi-byte character must not corrupt it.
#[test]
fn a_split_multibyte_character_survives() {
    let input = "data: \"é\"\n\n";
    let bytes = input.as_bytes();
    let split = input.find('é').unwrap() + 1;

    let mut decoder = SseDecoder::default();
    let mut out: Vec<String> = decoder.push(&bytes[..split]).collect();
    out.extend(decoder.push(&bytes[split..]));

    assert_eq!(out, vec!["\"é\"".to_owned()]);
}

/// `\r\n` and a bare `\r` are line terminators too.
#[test]
fn carriage_returns_terminate_lines() {
    assert_eq!(decode("data: a\r\n\r\n"), vec!["a".to_owned()]);
    assert_eq!(decode("data: a\r\rdata: b\r\r"), vec!["a", "b"]);
}

/// A trailing event with no blank line after it is unterminated. It is only
/// emitted when the stream is known to have ended.
#[test]
fn an_unterminated_event_waits_for_the_end_of_the_stream() {
    let mut decoder = SseDecoder::default();
    assert_eq!(decoder.push(b"data: {}").count(), 0);

    assert_eq!(decoder.finish(), vec!["{}".to_owned()]);
}
