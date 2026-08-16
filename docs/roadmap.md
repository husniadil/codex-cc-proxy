# Roadmap

Ordered phases to v0.1.0. Each is a green committed checkpoint: `just check`
passes at the end of every one, and no phase starts before the previous is done.

The order is not arbitrary. Translation and the fixture corpus come before the
transports that depend on them, because incremental upload is the one subsystem
whose bugs corrupt conversations instead of failing loudly, and it has to land
against tests that can catch that.

---

## 1. Workspace and gate

The skeleton and the standard everything else is held to.

Two crates, pinned toolchain, lint configuration, task runner, CI running the
same gate as local.

**Done when** `just check` passes on an empty implementation, and CI runs it on
push and pull request.

---

## 2. Request translation

`proxy-behavior.md` §2, as pure functions in `codex-cc-proxy-core`.

Instructions folding, content blocks, attachments nested in tool results, tool
flattening, `tool_choice`, deferred tool loading, web-search declaration, request
field hardening.

**Done when** every §2 rule has a test written before its implementation, and the
attachment path is covered for both images and documents in both positions —
directly in a user message and nested inside a `tool_result`.

---

## 3. Response translation

`proxy-behavior.md` §5, as a state machine in `codex-cc-proxy-core`.

Event mapping, deferred `tool_use` headers, stop-reason derivation, reasoning
blocks, search-result reconstruction, error and capacity frames.

**Done when** the emitted frame sequence is snapshot-tested for text, reasoning,
tool-call, incomplete, and error streams, and a tool call whose name arrives after
its block would have opened still produces a valid header.

---

## 4. Fixtures and `record`

The instrument that makes the rest test-first.

`record` captures real exchanges to disk in a form the test suite replays.

**Done when** a captured exchange replays as a test without hand-editing, and the
corpus covers at least one exchange per capability in `proxy-behavior.md` §1.

Phases 2 and 3 are written against hand-authored fixtures. This phase replaces
them with recorded ones and is where guesses get corrected.

---

## 5. Ingress and HTTP transport

The first end-to-end path. Testable against a local replay server without
credentials.

`/v1/messages` streaming both ways, `/v1/messages/count_tokens`, `/v1/models`,
the error taxonomy, cancellation propagation, empty-stream recording.

**Done when** a streaming request against a replay server returns a valid
Anthropic SSE sequence including a tool-call round trip, and cancelling the client
stream aborts the upstream request.

---

## 6. Credentials, catalog, tier mapping

OAuth with PKCE, the `CredentialStore` trait with its file implementation,
scope-free single-flight refresh, dead-grant marking, live model catalog with
fallback, four-tier validation.

**Done when** a real login succeeds, a token refresh survives expiry without
invalidating the family, an incomplete tier mapping refuses startup, and the
first real request reaches the live backend.

---

## 7. Control socket and CLI

`status`, `login`, `models`, `env`, `disconnect` — through the socket, not
through private paths.

**Done when** every verb works against a running daemon, `env` output pasted into
a shell produces a working Claude Code session, and the CLI holds no state of its
own.

---

## 8. Token accounting

Upstream figures mapped to Anthropic semantics, the estimator trait, calibration,
and both estimator implementations.

**Done when** cached tokens are subtracted exactly once, `message_start` carries a
non-zero estimate that `message_delta` replaces rather than adds to, the context
meter is steady across a turn in a live session, and both estimators are measured
against the corpus with the result recorded.

---

## 9. Conformance and `doctor`

The probe suite, and whatever the probes reveal is broken.

`Read` with an image, `Read` with a PDF, `WebSearch`, `WebFetch`, tool calling,
parallel tool calls, tool search, reasoning, `count_tokens`, cache accounting.

**Done when** every probe uses content the model could not infer, `doctor` prints
a capability matrix against a live backend, and each probe can be run alone.

This phase is the product. A green `just check` with a failing probe is not done.

---

## 10. WebSocket, incremental upload, compression

The last and riskiest subsystem, landing on a corpus that can catch its failures.

Connection reuse, prewarm, fallback latching, strict-extension delta computation,
zstd.

**Done when** the §9.4 invariants in `proxy-behavior.md` all hold as tests, a
policy close falls back to HTTP without losing the turn, and a long recorded
session replays identically over both transports.

Identical replay across transports is the acceptance criterion that matters. If
WebSocket and HTTP disagree on a single byte of the conversation, the delta logic
is wrong.

---

## 11. Release

Binaries for macOS, Linux, and Windows; `cargo install`; a Homebrew tap; a
container image; an install script. `README`, `CONTRIBUTING`, `SECURITY`,
`CODE_OF_CONDUCT`, `CHANGELOG`.

**Done when** a fresh checkout can install, run `codex-cc-proxy login`, paste the
`env` output, and drive a working Claude Code session following only the README.
