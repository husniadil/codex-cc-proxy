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

§11's acceptance criterion is now met on all three platforms: CI has run, and
`check` plus the Linux, macOS, and Windows builds are green. The first run failed
on its first command — the pinned toolchain carries no `rustfmt` or `clippy`,
which had been latent locally too — and the three build jobs passed on that same
first attempt.

Windows mattered most: the WebSocket transport was swapped after that platform
was last considered, and nothing had proved it since.

What is intended beyond that release is in **After v0.1.0**, stated as
intentions rather than commitments.

§L has since been worked through against a live backend, and every row that
could be settled is answered. That was not free of consequence: it falsified four things the
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

**Shipped:** five binaries per tag with a checksum file covering all of them,
`cargo install --git`, and an install script that verifies the download against
that checksum file with no way to skip the check. **Not shipped:** the Homebrew
tap and the container image, which are named here rather than quietly dropped —
the README says the same, because a missing package is better stated than
discovered.

Following the README's own instructions is what found the last defect in this
phase: an installed binary's `doctor` skipped all eight probes, because the
corpus was read from a `fixtures/` directory that only a checkout has. The
corpus now travels inside the binary. The lesson generalizes past this bug — every
acceptance check here had been run from a checkout, which is the one environment
no user of a release is in.

---

## After v0.1.0

Intended, not committed. Each entry says what it is for and what would make it
done, in the same terms as the phases above — an entry nobody can tell is
finished is an entry that never finishes. Order within a version is not fixed;
between versions it is.

### v0.2.0 — shipped

**Editing configuration without hand-writing TOML.** `tiers.set` was reserved
for this and answered that it was unimplemented. The hard part was never writing
the file: configuration is read once at startup, so a written change and a
running daemon disagree until the next `run`.

**Done.** `tiers.set` and `effort.set` move what routes turns, not only what
`status` reports, and each answers whether it was persisted. Validation runs
before the write and the write before the apply, so a failed write cannot leave
a daemon running a policy nobody chose.

Two things arrived with it that this section had not anticipated, both because a
front-end that is not a terminal needs them. **`login` over the control socket**
returns the authorization URL and completes in the background, sharing one
callback port rather than handing out a URL whose callback would be refused. And
**`usage.refresh`** asks the backend for a quota figure, covering the one case
the volunteered snapshot cannot — a front-end with a figure to show on a daemon
that has served no turn yet.

### v0.3.0 — shipped

**A launcher.** Starting the client currently means evaluating the output of
`env` in a shell first, which is one manual step that a reader can get wrong and
a script has to reimplement. A verb that sets the environment and execs a
command removes it, forwarding every argument it does not own so the client's
own flags keep working unchanged.

The verb should name what it does rather than what it launches — the naming rule
holds here, and a launcher that only ever starts one program is a launcher that
cannot start the next one. `env` stays: a launcher is a convenience over it, not
a replacement for it.

**Done when** a client started this way is indistinguishable from one started
after `eval "$(codex-cc-proxy env)"`, unknown arguments reach the child
untouched, the child's exit status is the launcher's, and a client given its own
`--settings` fails visibly rather than losing one of the two.

**Done**, as `exec`. It is more than a convenience over `env`, which is a change
from what this entry assumed: client policy has no environment variable
(`proxy-behavior.md` §7.3), so the shell path cannot carry it and this one can.
Each of the three paths now has exactly one limit. Writing the settings document
into a file is complete but touches a file the proxy does not own. `env` leaves
no trace but carries routing only. `exec` is complete and leaves no trace, and
is per invocation.

The last clause of the done-when exists because of a measurement taken while
building it: two `--settings` on one argument list and the client keeps the last
and drops the first, at exit 0 with an empty stderr. Either placement loses a
permission rule silently, so the launcher refuses instead.

**More than one upstream account.** One credential file means one account, and
an account that has run out of quota stops all work rather than some of it.
Credentials are already behind a trait and already carry the account id they
belong to, so what is missing is naming, selection, and a store that holds
several.

Each account holds its own refresh-token family, so a second account is not in
danger from the first refreshing. That is a property of separate grants, and
this entry originally rested it on a measurement instead — that rotation
supersedes without revoking. §L has since downgraded that measurement to a
probable grace window, and the argument does not need it: nothing about one
account's rotation reaches another account's family. Two holders of *one*
account are still in danger, which is the thing to keep out of the design.

**Done when** logging in twice leaves two usable accounts rather than one, the
account in use is stated by `status` and selectable without editing a file, and
a refresh on one account provably leaves the other's grant intact.

**Done.** A store of several grants with one selected, `login --as` and
`accounts --use` over `accounts` and `accounts.select`, and `status` naming the
account serving turns beside the rest. Two things turned out to belong to the
grant rather than to the daemon and had to travel with a switch: a refusal,
which is about one refresh token, and the quota snapshot, which belongs to the
account that earned it.

The isolation proof is offline, as everything here is: two accounts in one
store against the replay server, refreshing one and asserting the other's
stored grant is unchanged and still spends its own refresh token. What that
does not settle is whether the *backend* treats two grants from one client as
independent, which is a §L question rather than a proof this suite can hold.

**Credentials that are not a subscription.** The proxy authenticates one way
today: an OAuth grant against a consumer subscription. An API key is a different
credential against a different endpoint with different billing, and supporting
it makes the proxy useful to someone who has no subscription at all.

This is the first real test of the adapter seam, which has been present and
unused since v0.1. If the seam is wrong, this is where it shows.

**Done when** a key-authenticated request completes end to end, the two
credential kinds are selectable per tier or per account rather than globally,
and no code path can send one kind of credential to the endpoint that expects
the other.

**Done**, per account. The store holds accounts already, so the kind rides on
one: `login --key` stores a secret read from stdin under a name, and
`accounts --use` moves between kinds exactly as it moves between accounts. Per
tier would have needed a second selection mechanism for no capability this one
does not have.

The seam turned out to be in the wrong place rather than wrong. `Transport` was
a trait; what four paths each assembled by hand was the *authorization*. One
resolver now answers that, and the header set is where the two kinds actually
differ — a grant identifies a subscription client and the account it spends, a
key identifies nothing but itself. Two things fell out of the endpoint pairing
rather than being decided: a key account has no socket, because that protocol
is the subscription backend's, and no quota, because that figure is a
subscription entitlement.

**End to end means against the replay server**, which is what this suite can
hold. A live key endpoint has answered three times and settled less than that:
it took the key at the turn endpoint, refused a compressed body there, and
refused the same key at the model list. Whether a turn completes against it is
not recorded anywhere here — see §L.

### v0.4.0

**A graphical front-end.** The control socket was built for exactly this: the
daemon holds authoritative state, the CLI has no privileged path of its own, and
a second front-end needs no new daemon work. Whether that promise is true is
unproven until something other than the CLI speaks the protocol.

Which platforms, and whether it is native per platform or one cross-platform
shell, is open. So is whether the method names survive contact with a second
caller — they become a compatibility surface the moment one exists, and that is
worth settling before it does rather than after.

**Done when** every daemon capability is reachable without the CLI, and the
socket needed no method that only the graphical client would ever call.

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
| ~~What does the backend expect for a compressed request?~~ | **Answered.** HTTP: a zstd body with `Content-Encoding: zstd`, verified live. WebSocket: `permessage-deflate`, offered by the client and selected by the server on the upgrade — confirmed live, with full context takeover and no window limit. A binary frame means nothing on its own. |
| Does a refresh return a fresh `id_token`? | **Open.** It matters because the grant's `chatgpt_plan_type` is what `status` falls back to when no turn has been made, and a plan that never updates would report a stale entitlement indefinitely. The refresh path already stores a new id token when one comes back — whether one does is unmeasured. Attempting it on this account returned `refresh_token_reused`, so it cannot be settled here. Mitigated rather than assumed: the backend's own `plan_type`, reported on every turn, is preferred, and the grant's copy is labelled "as of last login" when it is what answered. |
| Does the key endpoint accept a compressed request body? | **Answered: no.** A turn sent with a zstd body and a valid key came back `400 invalid_json`, "encountered a unicode decode error when parsing this JSON value" — the compressed bytes parsed as JSON. Auth is checked after the body is parsed, so a bogus key cannot reproduce it: probed both ways, an invalid key answers 401 regardless. Key requests are no longer compressed. |
| ~~Can a key list models at the catalog endpoint?~~ | **Answered: yes**, and the earlier `401 Unauthorized` does not reproduce. The request the catalog fetch makes — `GET /v1/models?client_version=...`, this proxy's user agent, the key as a bearer token — answers `200` with the real list, and a running daemon holds that list for a key account rather than the fallback. Neither candidate survived: the key is not scoped without model-read, and dropping `client_version` changes nothing. What produced the 401 is not established, so nothing here rests on a cause. |
| ~~Does the key endpoint accept the request shape this proxy sends?~~ | **Answered: yes.** All eight capability probes pass live against `/v1/responses` with a key: both attachment paths, web search, web fetch, tool search, tool calling, the context meter, and token counting. That is the same matrix the subscription backend answers, on unguessable content in each case. |
| Does the key endpoint behave as this proxy assumes? | **Open.** The key path is proven end to end against the replay server: the request carries its key, carries neither `originator` nor `chatgpt-account-id`, and the turn completes. What that cannot settle is the real endpoint — whether it accepts this request shape, what its model list looks like (the catalog falls back and says so if the shape is unreadable), and whether anything about streaming differs. Derived, not confirmed. `doctor --live` probes the key endpoint as the key, so it is the way to settle this row. |
| Do two grants held by one client stay independent? | **Open.** Two accounts in one store hold two refresh-token families, and the offline proof is that this store never writes one account's rotation into another's slot. Whether the *authorization server* treats two grants issued to one `client_id` as independent is a question only a second live account answers. The design assumes it does, because that is what separate grants mean; the thing already known to be unsafe is the other arrangement — two holders of one account, where the last refresh retires everyone else's token. |
| Does a superseded refresh token stay redeemable? | **Previously answered yes; now doubtful.** That measurement saw a superseded token still redeem, and this account's stored token is now refused with `refresh_token_reused`. The earlier result most likely described a grace window rather than a durable property. Do not rely on it, and do not run a daemon against a *copy* of a credential file — the refresh-token family is shared, and whichever copy refreshes last leaves the other holding a dead token. |
| ~~Do the `session-id` and `thread-id` headers matter?~~ | **Answered: yes, and it cost a wrong conclusion twice on the way.** A `session_id` header scopes the prompt cache. The body's `prompt_cache_key` — which §2.1 called the thing that drives caching — produced no cached tokens on its own in any trial. The first probe run appeared to prove the header caused caching outright; it did not, because every condition shared one prompt and cache entries leaked between them. Rerun with a prompt per condition and the order reversed, the effect held. Then the shipping proxy showed **no** improvement end to end, because over WebSocket the incremental path already chains turns with `previous_response_id` and that caches by itself. With the socket disabled the difference is stark: uncached input per turn 4,465–4,497 without the header against 625–657 with it, 3,840 cached from the second turn on. So it is a fallback-path optimisation, and HTTP is a normal operating mode rather than an error path (§4.2). `thread-id` was not isolated and is not sent. |
| ~~Does the socket actually compress, and by how much?~~ | **Answered, measured live.** The server selects `permessage-deflate` when offered, with context takeover and no window limit. One identical turn, compression on versus off, counted on the wire: 104,566 in / 40,335 out against 300,879 in / 110,608 out — 65% off in both directions, 267 KB on a single first turn. Inbound is the larger half and grows with the conversation. **Zero tokens either way.** |
| ~~Does WebSocket connect, or close with a policy code?~~ | **Answered.** It connects. No policy close was seen, and the catalog marks these models `prefer_websockets`. |
| ~~Is `CLAUDE_CODE_DISABLE_1M_CONTEXT` inert for plain model ids?~~ | **Answered: no.** Without it the client appends `[1m]` to the unrecognized id and assumes a million tokens. The flag is load-bearing, not a precaution. |
| ~~Does the context meter stay steady across a turn?~~ | **Answered.** `message_start` carries the estimate and `message_delta` replaces it with the true count. |
| ~~What does the model catalog actually contain?~~ | **Answered.** It needs a `client_version` query parameter, and filters by it: a version below a model's `minimal_client_version` returns an empty list rather than an error. Entries are keyed by `slug`, state `visibility` as a word, and carry `supported_reasoning_levels`. |
| ~~Does it reject system and developer roles inside `input`, as assumed?~~ | **Answered: yes** — `400 System messages are not allowed`. §2.1 rests on this, and it is now measured rather than assumed. |
| ~~Does it accept an `input_file` part, the one shape with no upstream precedent?~~ | **Answered: yes.** Claude Code rasterises PDFs into `image` blocks, so no turn from that client reaches `input_file` — the path was closed by posting a `document` block to the ingress surface directly. The backend accepted the part and read the file: a generated PDF containing one random code returned exactly that code. The image path is separately confirmed. |
| ~~Does it accept a `tool_choice` other than `auto`?~~ | **Answered: yes.** `any` → `required` produced a `tool_use` for the named tool. |
| Does `client.disable_connectors` still suppress the connector notice on the current client version? | **Open, and carried in rather than measured here.** The setting is documented to be the only thing that silences it, and an environment variable of similar meaning does not — but that was established against an earlier client build, and this repo has not put it through a probe of its own. Low stakes: the failure mode is a banner, not a wrong answer. Method: start the client through the launcher with the setting on and off and read stderr on the first frame. |
| ~~Does `WebFetch` route through the haiku tier?~~ | **Answered: yes, both of them.** With haiku on a distinguishable model, `WebSearch` reported `query_source: web_search_tool` and `WebFetch` reported `query_source: web_fetch_apply`, both against the haiku model, while the main turns used sonnet's. An unmapped haiku breaks both in a way that looks unrelated to tier mapping. |
| ~~Does the backend emit `url_citation` annotations, or is `WebSearch` limited to opened pages?~~ | **Answered: it emits them.** A captured live search carried two `response.output_text.annotation.added` events, each a `url_citation` with a title, a URL, and the span of the reply it supports. Both reached the client as `web_search_result` entries, so the reconstruction is built on citations rather than on opened pages. |
| ~~Does incremental upload produce the same conversation live as on replay?~~ | **Answered, and it did not — twice.** The delta was empty on every continuing turn, so the backend answered from the previous response and the turn repeated itself. With that fixed, the session stopped matching as soon as the model returned a reasoning item, and every turn from the third on uploaded the whole conversation. Both fixed; a live four-turn conversation now uploads one item per turn. |
| ~~Do the real capability probes pass?~~ | **Answered: all eight, twice.** `doctor --live` was built to ask, and asking found two things replay could not. The corpus's attachments were stand-ins — a base64 string that was not a PNG — so the image and document probes passed on replay while proving nothing; they now carry a real PNG and a real PDF. And a marker spoken across several deltas was never contiguous in the raw frames, so every attachment probe failed against a backend that had read the attachment and said so. |
| ~~Does any client read `anthropic-ratelimit-unified-*` from a proxy?~~ | **Answered: no, and it cannot.** A stub endpoint setting those headers left `rate_limits` absent from the status-line payload. The reason is in the client's own schema rather than inferred: the payload is gated on a flag documented as false "when plan rate limits do not apply (API key, Bedrock, Vertex, or missing profile scope)", and pointing the client at a proxy means setting `ANTHROPIC_AUTH_TOKEN`, which is that path by definition. No header can change it, so §2.1's status-line proxy is the only route. The headers are still emitted — they are the accurate form of a figure the response carries, and the client does read them for its retry banner on a quota 429. |
| ~~Is `ultra` gated by plan as well as by model?~~ | **Answered: yes, both.** It exists only on `gpt-5.6-sol` and needs at least a Plus subscription. The account here is `free` — now read from the id token and reported by `status` — which is why its requests are refused with `Invalid value: 'ultra'`, the schema-level refusal rather than the model-specific one `minimal` gets. This is what makes the catalog a menu of what a client may offer rather than a statement of what the wire accepts: it advertises `ultra` on `gpt-5.6-terra`, which refuses it. The refusal is surfaced verbatim rather than guessed around, and the plan is reported so the two can be told apart locally. Not reproducible here without a paid plan. |
| Does compaction actually fire at the window the proxy supplies? | **Largely answered, derived not observed.** The client's history check compares the token count against a function of `autoCompactWindow`, and its own schema describes the trigger as the effective window minus a summary buffer, lowered further by a separate percentage override. So the figure the proxy supplies is the one compaction is measured against. Reading this also turned up a constraint the proxy had been ignoring: the value is accepted only between 100,000 and 1,000,000 — "Expected 'auto' or 100k–1M tokens" — and the settings form *discards* an out-of-range value silently. The proxy now omits it outside that range and warns. What remains unobserved is a session long enough to watch compaction happen. |
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
