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

§L has since been worked through against a live backend, and every row is
answered. That was not free of consequence: it falsified four things the
offline work had believed, and each correction is in the commit that proved it.
The section is kept in full rather than deleted, because what a claim rests on
is part of the claim.

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

Deferred, not skipped. These required a working subscription and could not be
settled by any amount of offline work. Each was written as a question with a
method; all of them have since been asked, and the answers are recorded here
alongside what they cost to learn.

| Question | Method |
|---|---|
| ~~Does the login flow complete against the real authorization server?~~ | **Answered.** It completes: the authorization request is accepted, the code exchange succeeds, and the account id is read from the id token. |
| ~~May this client request connector scopes?~~ | **Not asked, and not going to be.** The proxy requests only the scopes it uses, so this was never a question about the backend — it was a question about whether to widen the grant, and the answer is no. A refusal once suggested the server refused them; that was a truncated URL. |
| ~~Does a refresh survive expiry without invalidating the family?~~ | **Answered: yes.** With the stored expiry forced into the past, the next turn refreshed and completed. The refresh token rotated, and the superseded one still redeemed successfully afterwards — rotation supersedes, it does not revoke. The response also carries `expires_in`, agreeing with the token's own claim to within a second; §8 said no such field existed and has been corrected. |
| ~~Does the backend accept the request shape — headers, `instructions`, tools? | **Answered.** Accepted as sent; a turn completes and the frame sequence is correct. |
| ~~What does the backend expect for a compressed request?~~ | **Answered.** HTTP: a zstd body with `Content-Encoding: zstd`, verified live. WebSocket: `permessage-deflate`, negotiated in the upgrade — the server offers it, and our WebSocket library cannot yet accept. A binary frame means nothing on its own. |
| ~~Does WebSocket connect, or close with a policy code?~~ | **Answered.** It connects. No policy close was seen, and the catalog marks these models `prefer_websockets`. |
| ~~Is `CLAUDE_CODE_DISABLE_1M_CONTEXT` inert for plain model ids?~~ | **Answered: no.** Without it the client appends `[1m]` to the unrecognized id and assumes a million tokens. The flag is load-bearing, not a precaution. |
| ~~Does the context meter stay steady across a turn?~~ | **Answered.** `message_start` carries the estimate and `message_delta` replaces it with the true count. |
| ~~What does the model catalog actually contain?~~ | **Answered.** It needs a `client_version` query parameter, and filters by it: a version below a model's `minimal_client_version` returns an empty list rather than an error. Entries are keyed by `slug`, state `visibility` as a word, and carry `supported_reasoning_levels`. |
| ~~Does it reject system and developer roles inside `input`, as assumed?~~ | **Answered: yes** — `400 System messages are not allowed`. §2.1 rests on this, and it is now measured rather than assumed. |
| ~~Does it accept an `input_file` part, the one shape with no upstream precedent?~~ | **Answered: yes.** Claude Code rasterises PDFs into `image` blocks, so no turn from that client reaches `input_file` — the path was closed by posting a `document` block to the ingress surface directly. The backend accepted the part and read the file: a generated PDF containing one random code returned exactly that code. The image path is separately confirmed. |
| ~~Does it accept a `tool_choice` other than `auto`?~~ | **Answered: yes.** `any` → `required` produced a `tool_use` for the named tool. |
| ~~Does `WebFetch` route through the haiku tier?~~ | **Answered: yes, both of them.** With haiku on a distinguishable model, `WebSearch` reported `query_source: web_search_tool` and `WebFetch` reported `query_source: web_fetch_apply`, both against the haiku model, while the main turns used sonnet's. An unmapped haiku breaks both in a way that looks unrelated to tier mapping. |
| ~~Does the backend emit `url_citation` annotations, or is `WebSearch` limited to opened pages?~~ | **Answered: it emits them.** A captured live search carried two `response.output_text.annotation.added` events, each a `url_citation` with a title, a URL, and the span of the reply it supports. Both reached the client as `web_search_result` entries, so the reconstruction is built on citations rather than on opened pages. |
| ~~Does incremental upload produce the same conversation live as on replay?~~ | **Answered, and it did not — twice.** The delta was empty on every continuing turn, so the backend answered from the previous response and the turn repeated itself. With that fixed, the session stopped matching as soon as the model returned a reasoning item, and every turn from the third on uploaded the whole conversation. Both fixed; a live four-turn conversation now uploads one item per turn. |
| ~~Do the real capability probes pass?~~ | **Answered: all eight, twice.** `doctor --live` was built to ask, and asking found two things replay could not. The corpus's attachments were stand-ins — a base64 string that was not a PNG — so the image and document probes passed on replay while proving nothing; they now carry a real PNG and a real PDF. And a marker spoken across several deltas was never contiguous in the raw frames, so every attachment probe failed against a backend that had read the attachment and said so. |
| ~~Is the true input count linear in the estimator's raw figure?~~ | **Answered: yes.** Six live turns, residuals under 3% from the second turn on. The uncalibrated first turn was +95%. Recorded in §6.3. |

Before a question was answered, the corresponding claim in `proxy-behavior.md`
was **derived from the upstream's own protocol definitions, not confirmed
against a running backend.** That is a meaningful difference, and the point of
this section is that the difference was never left implicit.

Answering these falsified rules, which is the expected outcome of a live gate:
the empty delta, the reasoning mismatch, the compressed WebSocket frame, and the
response's expiry field were all found this way. Each was fixed by amending the
spec in the same commit as the code — not by treating the offline phases as
having been wrong to do.

New questions belong here as they are found. A section that is complete because
nothing was added to it is not a section anyone is still using.
