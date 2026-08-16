# Roadmap

Ordered phases to v0.1.0. Each is a green committed checkpoint: `just check`
passes at the end of every one, and no phase starts before the previous is done.

The order is not arbitrary. Translation and the fixture corpus come before the
transports that depend on them, because incremental upload is the one subsystem
whose bugs corrupt conversations instead of failing loudly, and it has to land
against tests that can catch that.

## Where this stands

Phases 1 through 11 are complete: `just check` is green, every capability probe
passes against the replay corpus, a long session leaves identical conversation
state over both transports, and the estimator comparison is measured and
decided.

§L is untouched, and is the honest remainder. Nothing in it blocked v0.1.0 and
nothing in it is answered by v0.1.0.

## Everything here is verifiable offline

**No phase's completion criterion requires a live backend, credentials, or
quota.** The suite runs entirely against a local replay server. This is a
constraint on the design, not a workaround: a project whose correctness can only
be demonstrated by spending money is a project whose correctness stops being
demonstrable at an arbitrary moment.

What genuinely cannot be settled offline is collected in §L, stated as open
rather than assumed. Nothing in §L blocks v0.1.0, and nothing in phases 1–11
depends on it.

---

## 1. Workspace and gate

The skeleton and the standard everything else is held to.

Two crates, pinned toolchain, lint configuration, task runner, CI running the
same gate as local. Each crate opts into the workspace lints explicitly.

**Done when** `just check` passes on an empty implementation, and CI runs it on
push and pull request.

---

## 2. Request translation

`proxy-behavior.md` §2, as pure functions in `codex-cc-proxy-core`.

Instructions folding, content blocks, attachments nested in tool results, tool
flattening, `tool_choice`, deferred tool loading, web-search declaration, request
field hardening, upstream headers.

**Done when** every §2 rule has a test written before its implementation, and the
attachment path is covered for images and documents in both positions — directly
in a user message and nested inside a `tool_result`.

---

## 3. Response translation

`proxy-behavior.md` §5, as a state machine in `codex-cc-proxy-core`.

SSE framing including multi-line `data:` reassembly, event mapping, deferred
`tool_use` headers, stop-reason derivation, reasoning blocks, search-result
reconstruction, error and capacity frames.

**Done when** the emitted frame sequence is snapshot-tested for text, reasoning,
tool-call, search, incomplete, and error streams; a tool call whose name arrives
after its block would have opened still produces a valid header; and an event
split across several `data:` lines parses as one payload.

---

## 4. Fixture corpus

The evidence base the rest is tested against, built without spending quota.

Three sources, in descending order of authority:

1. **The upstream's own protocol definitions.** Its typed event set is the
   authoritative statement of what the backend can emit, and its test harness
   shows those events assembled into realistic streams. A fixture derived from
   the types is not a guess — it is the contract, restated.
2. **Ingress captures.** `record ingress` captures what Claude Code actually
   sends. This needs a working client and no credentials, because the exchange is
   recorded before translation. Everything on the request side — tool
   declarations, `defer_loading` stubs, `tool_reference` results, attachment
   blocks, `output_config`, the search sub-request — is observable this way for
   free.
3. **Hand-authored edge cases**, marked as such, for shapes neither source
   covers.

**Done when** the corpus replays as tests without hand-editing, covers at least
one exchange per capability in `proxy-behavior.md` §1, every fixture records
which of the three sources it came from, and a capture parses as a fixture with
no hand-editing at all.

`record ingress` needs the ingress server, so it arrives in phase 5 and its
round trip is asserted there. Pointing a real client at the daemon is the one
step nobody but the operator can take, and it needs no credentials.

A fixture's provenance is part of the fixture. A derived one and a captured one
carry different weight, and a reader must not have to guess which is which.

---

## 5. Ingress and HTTP transport

The first end-to-end path, against a replay server.

`/v1/messages` streaming both ways, `/v1/messages/count_tokens`, `/v1/models`,
the error taxonomy, cancellation propagation, empty-stream recording, port
conflict handling.

**Done when** a streaming request returns a valid Anthropic SSE sequence
including a tool-call round trip, cancelling the client stream aborts the
upstream request, and every row of the `api.md` §1.1 error table is produced by a
test.

---

## 6. Credentials, catalog, tier mapping

OAuth with PKCE, the `CredentialStore` trait with its file implementation,
scope-free single-flight refresh, dead-grant marking, catalog fetch with TTL
cache and fallback, four-tier validation, the `[1m]` rejection.

**Done when** the authorization URL is built to spec, a refresh request provably
omits `scope`, concurrent refreshes collapse to one upstream call, an
invalid-grant response marks the connection dead without retrying, an incomplete
tier mapping refuses startup, an unreachable catalog skips validation instead of
failing it, and a model with no known window is treated as unknown rather than
assumed.

Every one of these is a test against a mock authorization server. The live login
is §L.

---

## 7. Control socket and CLI

`status`, `login`, `models`, `env`, `disconnect`, `record` — through the socket,
not through private paths.

**Done when** every verb works against a running daemon over the socket, `env`
emits all four tier variables plus the context floor, and the CLI holds no state
of its own.

---

## 8. Token accounting

Upstream figures mapped to Anthropic semantics, the estimator trait, calibration,
and both estimator implementations.

**Done when** cached tokens are subtracted exactly once, a `cached_tokens` value
exceeding `input_tokens` clamps to zero, `message_start` carries a non-zero
estimate that `message_delta` replaces rather than adds to, calibration measurably
improves the estimate across a replayed multi-turn session, and both estimators
are measured against the corpus with the result recorded in
`proxy-behavior.md` §6.3.

The estimator comparison is a real measurement with a real outcome. Do not ship
both and leave the choice open.

---

## 9. Conformance and `doctor`

The probe suite, and whatever the probes reveal is broken.

`Read` with an image, `Read` with a PDF, `WebSearch`, `WebFetch`, tool calling,
parallel tool calls, tool search, reasoning continuity, `count_tokens`, cache
accounting.

**Done when** every probe uses content the model could not infer, every probe
runs green against the replay corpus, `doctor` prints a capability matrix, each
probe can be run alone, and a probe reports honestly when it cannot run.

Running the probes against a replay server proves the proxy does its half
correctly. It does not prove the backend does its half. That is §L, and `doctor`
must not claim otherwise — a matrix built from replayed fixtures says so on its
face.

---

## 10. WebSocket, incremental upload, compression

The last and riskiest subsystem, landing on a corpus that can catch its failures.

Connection reuse, prewarm, fallback latching, strict-extension delta computation,
reasoning-item retention, zstd.

**Done when** the §9.4 invariants in `proxy-behavior.md` all hold as tests, a
policy close mid-turn falls back to HTTP without losing the turn, retained
reasoning items are re-injected in position, and a long replayed session produces
byte-identical conversation state over both transports.

Identical state across transports is the acceptance criterion that matters. If
WebSocket and HTTP disagree on a single item, the delta logic is wrong.

---

## 11. Release

Binaries for macOS, Linux, and Windows; `cargo install`; a Homebrew tap; a
container image; an install script. `README`, `CONTRIBUTING`, `SECURITY`,
`CODE_OF_CONDUCT`, `CHANGELOG`.

**Done when** a fresh checkout builds on all three platforms in CI, the README's
setup instructions are followed end to end against the replay server, and the
documented limitations match what the code actually does.

---

## L. The live gate

Deferred, not skipped. These require a working subscription and cannot be
settled by any amount of offline work. Each is written as a question with a
method, so whoever has quota can close it in one sitting.

| Question | Method |
|---|---|
| ~~Does the login flow complete against the real authorization server?~~ | **Answered.** It completes: the authorization request is accepted, the code exchange succeeds, and the account id is read from the id token. |
| May this client request connector scopes? | Unknown, and unasked: the proxy requests only what it uses. A refusal once suggested otherwise but was a truncated URL. |
| Does a refresh survive expiry without invalidating the family? | Force expiry, refresh, confirm the prior token family still works |
| ~~Does the backend accept the request shape — headers, `instructions`, tools? | **Answered.** Accepted as sent; a turn completes and the frame sequence is correct. |
| ~~Does WebSocket connect, or close with a policy code?~~ | **Answered.** It connects. No policy close was seen, and the catalog marks these models `prefer_websockets`. |
| ~~Does the context meter stay steady across a turn?~~ | **Answered.** `message_start` carries the estimate and `message_delta` replaces it with the true count. |
| ~~What does the model catalog actually contain?~~ | **Answered.** It needs a `client_version` query parameter, and filters by it: a version below a model's `minimal_client_version` returns an empty list rather than an error. Entries are keyed by `slug`, state `visibility` as a word, and carry `supported_reasoning_levels`. |
| Does it reject system and developer roles inside `input`, as assumed? | Deliberately send one; record the error |
| Does it accept an `input_file` part, the one shape with no upstream precedent? | Read a PDF whose content is unguessable; check the answer, not the acceptance |
| Does it accept a `tool_choice` other than `auto`? | Send `required`; the upstream client only ever sends `auto` |
| Does `WebFetch` route through the haiku tier? | Map haiku to a distinguishable model; issue a `WebFetch`; check which model answered |
| Does the backend emit `url_citation` annotations, or is `WebSearch` limited to opened pages? | Run a search that must cite; check whether titles arrive |
| ~~Does incremental upload produce the same conversation live as on replay?~~ | **Answered, and it did not.** The delta was empty on every continuing turn, so the backend answered from the previous response and the turn repeated itself. Fixed; the frame on the wire is now asserted. |
| Do the real capability probes pass? | `doctor` against the live backend |
| Is the true input count linear in the estimator's raw figure, as the offline fit assumes? | Record counts across a growing session; fit and check the residuals |

Until each is answered, the corresponding claim in `proxy-behavior.md` is
**derived from the upstream's own protocol definitions, not confirmed against a
running backend.** That is a meaningful difference and the docs say so where it
applies.

Answering these may falsify a rule. That is the expected outcome of a live gate,
and the fix is to amend the spec in the same commit as the code — not to treat
the offline phases as having been wrong to do.
