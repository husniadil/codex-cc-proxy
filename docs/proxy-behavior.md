# Proxy behavior

Normative specification for how codex-cc-proxy translates between the Anthropic
Messages API and the OpenAI Responses API, and how it manages sessions,
transports, credentials, and token accounting.

This is the definition the code is measured against. [`api.md`](api.md) is the
companion contract for what the proxy *exposes*.

Most rules here exist because the obvious implementation is wrong in a way that
does not fail loudly. Where that is the case, the rule says so.

---

## 1. Premise

Claude Code is not an ordinary Messages API client. Several of its built-in tools
depend on behaviour the server provides, and a translator that handles only
messages and function calls leaves those tools broken while every request still
returns 200.

| Path | Server dependency | Failure when unhandled |
|---|---|---|
| `Read` (image, PDF) | attachment blocks nested inside `tool_result` | bytes never arrive; the model describes the file from its name |
| `WebSearch` | a server-side search tool declared in a secondary conversation | search returns nothing, reported as "no results" |
| `WebFetch` | a model call, believed to be on the haiku tier | fails in a way that looks unrelated to tier mapping |
| tool search | `defer_loading` stubs and `tool_reference` discovery | discovered tools stay uncallable, or every stub inflates context |
| context meter | `input_tokens` in `message_start` | the meter collapses to zero each turn |
| `count_tokens` | pre-flight sizing | absent or wrong |

Preserving these is the product. Everything else in this document serves that.

---

## 2. Request translation

### 2.1 Instructions

Claude Code's system prompt arrives in the top-level `system` field. It maps to
the Responses `instructions` field, never to an item in `input` — the backend
rejects system-role and developer-role messages inside `input`.

A conversation message carrying any role other than `user` or `assistant` has its
text folded into `instructions` for the same reason, rather than emitted as an
input item.

### 2.2 Content blocks

| Anthropic | Responses |
|---|---|
| `user` / `text` | `message` / `input_text` |
| `user` / `image` | `message` / `input_image` |
| `user` / `document` | `message` / `input_file` |
| `assistant` / `text` | `message` / `output_text` |
| `user` / `document` inside a `tool_result` | `message` / `input_file`, following the output (§2.3) |
| `tool_use` | `function_call` |
| `tool_result` | `function_call_output` (§2.3) |
| `thinking`, `redacted_thinking` | dropped — no equivalent exists |

Base64 image and document sources encode as data URLs. `image_url` is that URL
directly, not an object wrapping one. URL image sources pass through unchanged
and are not prefetched; they resolve only if the backend can reach them.

`input_file` is the one part in this table with no counterpart in the upstream
client, which has no document representation at all. It is the public API's
shape, it is the only candidate that could carry a PDF, and whether this backend
accepts it is open — roadmap §L. It fails loudly if not: a rejected part is a
request error, not a silently dropped file.

Assistant content is `output_text` only. An attachment appearing in an assistant
message is dropped rather than converted.

### 2.3 Attachments inside tool results

A tool result is not restricted to text. `function_call_output.output` is either
a bare string or a list of content parts, and an `input_image` part inside that
list is how an image reaches the model — attached to the call that produced it,
with no synthetic message standing between them.

The output collapses to a bare string when, and only when, it is a single piece
of text. Every other case stays a list, including the empty one.

Documents are the exception. No document part exists inside a tool output, so
each is re-emitted as a `user` message placed immediately after the
`function_call_output`, which keeps its text. That placement is not a
preference: `input_file` is defined for message content and nowhere else, so it
is the only position where it could be accepted at all.

This is not an edge case. It is how every file Claude Code reads arrives. Without
it the bytes never reach the model, and the model answers from the filename in
hedged wording that reads as success — the failure is invisible in ordinary
output, which is why §9.3 requires unguessable probes.

### 2.4 Tools

Function tools flatten from `{name, description, input_schema}` to `{type:
"function", name, description, strict, parameters}`. A schema with no
`properties` key gains an empty one.

`strict` is always false. Strict mode constrains the schema — every property
required, no additional properties — and the client's tool schemas do not
comply. Claiming it over a non-compliant schema is a request rejection, not a
stricter model.

`tool_choice` maps: `any` → `required`, `tool` → `{type: "function", name}`,
anything else → `auto`.

### 2.5 Deferred tool loading

Tool discovery happens in the client. Undiscovered tools arrive marked
`defer_loading: true` and are withheld from the upstream request so their schemas
do not occupy context.

The backend has a deferred-loading mechanism of its own, and the flag could be
forwarded to it instead. It is not. Discovery here is driven by the client, and
a second discovery path the client cannot observe would let the model load a
tool whose results never reach the client.

Discovery is observable exactly once: a tool-search result contains
`tool_reference` blocks naming the tools that became available. Those names are
recorded on the session, and a recorded tool is forwarded on later turns *even
though it continues to arrive marked `defer_loading`*. That flag is not cleared
by the client, so the recorded set is the only signal that a tool is live.

A tool-search result has no text content, only `tool_reference` blocks. Its
`function_call_output` therefore carries the discovered names serialized as JSON,
so the output is non-empty and the model can tell which tools it may now call.

### 2.6 Web search

`WebSearch` runs as a secondary conversation declaring the server-side search
tool — `{type: "web_search_<version>", name: "web_search"}`, with no
`input_schema`. Any tool whose `type` begins with `web_search` maps to the
Responses API's native `web_search` tool.

Translating it as a function tool produces a tool the model cannot execute and a
search that silently returns nothing.

Both access flags — external and indexed — are stated rather than left to a
default, because a default of false would also produce a search that returns
nothing.

### 2.7 Request fields

Every request sets `stream: true`, `store: false`, `parallel_tool_calls: true`,
and includes `reasoning.encrypted_content`.

`reasoning.effort` derives from the inbound `output_config.effort`, under an
optional ceiling the operator sets. The client cannot choose that ceiling: it
does not know whose quota it is spending, and effort is the largest lever on
what a turn costs.

The ceiling caps and never raises. A request asking for less keeps its own
choice, because capping a maximum is not a request to spend more. With no
request effort at all the ceiling still applies — an operator who capped effort
meant it for the traffic that expresses no preference too, and that is most of
it. With no ceiling and no request effort, the field is omitted and the
backend's own default applies.

`reasoning.summary` is always `auto`.

`prompt_cache_key` derives from session identity (§3.1) and is stable for the
life of a conversation. Cache hit rate depends on it directly, making it the
largest single cost lever in the system.

Unsupported inbound parameters are dropped through an allowlist rather than
forwarded. Anthropic `cache_control` blocks have no equivalent and are dropped;
upstream caching is implicit.

Server-assigned item ids from a previous response are stripped before an item is
re-sent, and `previous_response_id` is set only by the incremental path (§4.3).

### 2.8 Upstream request headers

| Header | Value |
|---|---|
| `authorization` | `Bearer <access token>` |
| `chatgpt-account-id` | the account id carried in the access token |
| `originator` | a single fixed first-party originator |
| `user-agent` | matching that originator |
| `openai-beta` | the Responses experimental opt-in |
| `session_id` | the session identity (§3.1), truncated to 64 characters |
| `accept` | `text/event-stream` on the HTTP transport |

One originator, always, with no alternate to fall back to. A rejection at this
layer surfaces as an error rather than triggering a retry under a different
identity: a fallback identity is state that has to be tracked, invalidates the
prompt cache when it changes, and turns one clear failure into two unclear ones.

A challenge response — a non-JSON body on a 403 — is reported as an `api_error`
with the body excerpt intact, because the excerpt is the only diagnostic
available.

---

## 3. Sessions

### 3.1 Identity

Claude Code sends no session identifier. Identity is derived from content: a
request belongs to an existing session when its `input` is a strict extension of
that session's baseline.

This is the same predicate that governs incremental upload (§4.3), so session
matching and delta computation share one definition rather than two that can
disagree.

Two conversations that genuinely share a prefix — the same system prompt and the
same opening turn — are indistinguishable until they diverge, and may match the
same session. This is harmless: the shared prefix is identical, so the baseline
is correct for both, and the first divergent turn separates them. What must not
happen is a match on a *partial* prefix, which is why the predicate requires a
strict extension of the full baseline rather than a longest-common-prefix score.

### 3.2 State

A session holds its input baseline and the output items the server added, its
transport binding, its discovered tool names, its retained reasoning items
(§3.3), and its estimator calibration ratio. Sessions expire on idle, and the
store is bounded — eviction is by least recent use, never by refusing a request.

### 3.3 Reasoning continuity

Requests ask for `reasoning.encrypted_content`, so responses carry reasoning
items the model expects to see again on the next turn.

Those items cannot survive a round trip through the client. Anthropic `thinking`
blocks are dropped on the request path (§2.2), and the client would not return
encrypted upstream reasoning even if they were not. Every turn would therefore
begin with the model's prior reasoning discarded.

The session retains server-returned reasoning items and re-injects them in their
original position on the next request. They are part of the baseline for §4.3 in
exactly the same way other server-returned output items are, so the incremental
and full-send paths agree on what the conversation contains.

A conversation is therefore held in two forms. What the client replays can
never contain the server's reasoning; what the backend holds does. Reconciling
converts the first into the second, and the delta is computed on the second by
strict comparison. Running the reconciling rule on an already-reconciled input
misaligns exactly the items it put back, so the order matters and is not
interchangeable.

Re-injection is not optional, and not only about quality. A baseline holding an
item the client cannot replay is never a strict extension of any later replay,
so a strict comparison stops matching the moment the model reasons. Session
identity (§3.1) and delta computation (§4.3) therefore both judge continuation
by the *reconciling* predicate: server-only items in the baseline are matched
past rather than matched against. Without that, a conversation silently
restarts on its third turn — new session, lost calibration, lost discovered
tools, and a full upload every turn thereafter.

This is the one place the proxy adds content the client did not send. It is
additive and upstream-only: nothing synthesized here is ever surfaced back to the
client as model output.

---

## 4. Transport

### 4.1 WebSocket

WebSocket is primary. One connection is cached per session and opened lazily.
Reuse removes per-turn TCP and TLS setup, which is significant in an agent loop
issuing many sequential requests.

A prewarm request opens the connection before the first real request of a turn,
so that request reuses both the connection and the prior response id.

### 4.2 HTTP fallback

HTTP with SSE is a complete, independently correct transport — not a degraded
path. The backend closes WebSocket connections under policy conditions often
enough that fallback is a normal operating mode.

A session that fails to establish or maintain a WebSocket latches to HTTP for the
rest of its life rather than retrying every turn.

### 4.3 Incremental input

The Messages API is stateless, so the client replays the whole conversation every
turn. Over HTTP with `store: false` the full transcript is re-uploaded each time.
In a long session that dominates both upload cost and time to first token.

On a reused connection only new items are sent, with the previous response id. A
delta is valid only when every non-input request field is unchanged *and* the new
input is a strict extension of the previous input plus the output items the
server added. Server-returned items are part of the baseline and are never
resent.

Any mismatch sends the full input. So does a delta that would be empty: the
backend given a previous response id and no new items answers from that
response, so a client retrying an unchanged conversation would be handed the
previous turn again instead of a fresh one.

A turn only enters the baseline once the backend has accepted it. Recording one
that failed would make the next delta continue a response that never saw those
items, and the question would vanish from the conversation without any error.
A brand-new session is the exception: it claims its conversation immediately, so
a concurrent request cannot match its empty baseline and join a conversation it
has nothing to do with. Nothing is at risk there, because a session with no
completed turn has no response to continue and can only send in full.

**Falling back is always safe; a wrong delta is not.** A full send costs
bandwidth. A wrong delta corrupts the conversation and does not fail visibly.
Every ambiguous case resolves toward the full send, and the check is conservative
by construction.

### 4.4 Compression

Request payloads may be zstd-compressed. This compounds with §4.3: incremental
upload removes most turns' bulk, and compression reduces what remains on the
turns where a full send is unavoidable.

---

## 5. Response translation

### 5.0 Framing

On the HTTP transport, events arrive as SSE. An event block may carry more than
one `data:` line, and the SSE specification defines those as one logical payload
joined with newlines — not as independent JSON documents. Parsing each line
separately corrupts any event large enough to be split, which is exactly the
events that matter: long tool-call arguments and long text deltas.

A `data:` payload of `[DONE]` is a terminator, not content. A payload that does
not parse as JSON is ignored rather than treated as an error.

On the WebSocket transport the same events arrive as discrete messages and need
no reassembly. Both transports produce the same event stream before translation
begins, so §5.1 onward is transport-independent.

### 5.1 Events

Responses events become Anthropic SSE frames through one state machine.
Anthropic permits a single open content block at a time.

| Responses event | Anthropic output |
|---|---|
| `response.created` | `message_start` |
| `response.reasoning_summary_text.delta` | `thinking` block, `thinking_delta` |
| `response.output_text.delta` | `text` block, `text_delta` |
| `response.output_item.added` (function call) | `tool_use` block |
| `response.function_call_arguments.delta` | `input_json_delta` |
| `response.output_item.done` | `content_block_stop` |
| `response.completed` / `.done` | `message_delta` + `message_stop` |
| `response.incomplete` | `message_delta`, `stop_reason: max_tokens` |
| `error`, `response.failed` | `error` frame |

A `tool_use` block's `content_block_start` is deferred until the function name is
known, because Anthropic clients cannot patch a block header after it is emitted.

`stop_reason` is `tool_use` when the turn produced any function call,
`max_tokens` on an incomplete response, `end_turn` otherwise.

A stream opening with a capacity or overload condition becomes an
`overloaded_error` frame so the client retries on its own.

### 5.2 Search results

The backend runs web search server-side and reports it through search call items
and citation annotations. These are reconstructed into Anthropic's structured
shapes — `server_tool_use` and `web_search_tool_result` blocks carrying `url` and
`title` per result.

The client extracts `url` and `title` from those blocks. Passing the model's prose
answer through as the tool result leaves that extraction empty, so the structured
form is required, not preferred.

A search call names the query. The sources come from `url_citation`
annotations, which arrive while the answer is being written — after the search
itself has completed. Both blocks are therefore emitted as the message closes
rather than where the search ran. Their position in the message does not affect
what the client extracts.

Citations are the one part of this the upstream client cannot corroborate: it
discards annotations entirely and so never sees a cited URL. The annotation
shape is the public API's, and whether this backend emits it is a §L question.

A page the model opened is treated as a source even when nothing cited it.
Without that, a search that fetched pages but produced no citations reaches the
client as an empty result — which reads as "nothing found", the precise failure
this section exists to prevent.

A source cited repeatedly is one result. The client renders the list verbatim.

### 5.3 Cancellation

Cancelling the outbound stream aborts the upstream request. Without propagation
the backend generates to completion against a reader that no longer exists,
spending quota on output nobody receives.

### 5.4 Empty streams

A stream that completes having produced no content frames is recorded with its
request and the raw upstream bytes. It is always a defect, and it is otherwise
invisible.

---

## 6. Token accounting

### 6.1 Upstream figures are authoritative

Completed responses report real input, output, and cached token counts. These are
never recomputed.

One conversion is required. OpenAI's `input_tokens` includes cached tokens;
Anthropic's excludes them and reports cache counters separately. So `input_tokens`
becomes `input_tokens - cached_tokens`, clamped at zero, and `cached_tokens`
becomes `cache_read_input_tokens`.

`cache_creation_input_tokens` is always zero. Upstream caching is implicit, with
no distinct write event to report. It stays zero rather than being synthesized
into something plausible.

### 6.2 The two points that need an estimate

`count_tokens` is a pre-flight call: nothing has been sent, and the Responses API
has no token-counting endpoint.

`message_start` carries `input_tokens` in Anthropic's protocol, but upstream
reports usage only at completion. Emitting zero is not neutral — the client
renders that value live, so the context meter collapses to zero at the start of
every turn and snaps back when the real figure arrives.

Both use a local estimator, and both are followed by ground truth within the same
exchange. `message_delta` carries cumulative final usage, not an increment, so
writing the true value there replaces the estimate rather than adding to it.

### 6.3 Calibration

The estimator corrects itself against upstream. Each completed request yields a
true input count for a request that was also estimated, and the pair is folded
into a fit retained on the session.

**The fit is a line, not a multiplier.** Part of the unmodelled cost scales with
the conversation and part does not: the instructions wrapper is charged once
however long the session runs. A single ratio cannot represent both. Fitting one
anyway makes it converge from whichever regime it saw first — an early short
request, where the fixed cost dominates, pulls the ratio high, and it then
decays for the remainder of the session while every estimate reads over. Scale
and offset are fitted together instead, by incremental least squares.

Where the fit is underdetermined it is not invented. One observation, or several
at the same size, cannot separate scale from offset; the estimator falls back to
a plain ratio and extrapolates nothing.

This absorbs what a tokenizer alone cannot. The upstream count includes framing
the proxy does not model identically — the instructions blob, serialized tool
schemas, per-item overhead. A byte-exact tokenizer over structurally different
inputs produces a number that is authoritatively wrong, which is worse than one
that is approximate and self-correcting.

**The measurement, and what it settles.** Both estimators were run over a
growing multi-turn session against a modelled upstream count — text cost plus a
per-item framing charge plus a fixed wrapper. Mean absolute error over the
second half: **0.01% calibrated, 68% tokenizer**. The tokenizer is low by
almost exactly the framing it cannot see, and no amount of exactness closes
that, because the gap is not in the text.

The calibrated estimator therefore ships and the tokenizer stays behind a
feature flag, as a comparison instrument rather than a candidate.

What the measurement does **not** settle is accuracy against this backend. The
modelled count is linear in the same structure the raw estimate measures, so a
linear fit can absorb it exactly; that the real relationship is equally
tractable is not demonstrated here and belongs to §L. What is demonstrated is
the mechanism: a multiplicative correction cannot absorb a fixed cost, a linear
one can, and exactness over the wrong quantity loses to self-correction over
roughly the right one.

Before a session's first completed request the estimate is uncalibrated.

---

## 7. Models

### 7.0 Catalog

The catalog is fetched from the backend and cached in memory with a short TTL.
Each entry contributes an id, a visibility flag, and window metadata: a context
window, an optional maximum context window, and an optional effective percentage.
Hidden entries and non-conversational pseudo-models are excluded from what is
offered for mapping, but their window metadata is retained — a session may
reference a model the picker filters out, and knowing its window is better than
not.

The effective window is the context window scaled by the effective percentage,
which reserves headroom for instructions, tool overhead, and output. Where the
percentage is absent, the upstream default applies. Where both a context window
and a maximum context window are present, the smaller-scoped `context_window` is
authoritative — the maximum describes a ceiling the account may not have.

A fixed fallback list covers a failed fetch, so the daemon starts and reports
honestly rather than blocking on an unreachable catalog. The fallback carries ids
only. A model with no known window is **unknown, not assumed**: the window guard
(§7.2) does not fire for it, and no percentage is derived from a guess.

Fetch failure is not the same claim as absence. Validation that depends on the
catalog is skipped when the catalog is unavailable, never failed.

### 7.1 Tier mapping

All four tiers — `opus`, `sonnet`, `haiku`, `fable` — must be mapped explicitly,
each validated against the live catalog. The daemon refuses to start on an
incomplete or invalid mapping.

The client routes different work to different tiers, and background and
summarization traffic runs on the cheapest one. A defaulted mapping hides which
model handles that traffic and what it costs, so the mapping is stated rather
than inferred.

If the catalog cannot be fetched, validation is skipped rather than failed. An
unreachable catalog is not evidence that a model went away.

### 7.2 Context window

A mapped model id must not contain a `[1m]` marker; the daemon rejects one that
does.

The client infers a context window from the model id. An unrecognized id yields a
200,000-token assumption; an id carrying `[1m]` yields 1,000,000.

Real windows are smaller than 1,000,000, so the marker would make the client
believe it has roughly four times the headroom it has, and auto-compaction would
never fire before the window overran. The 200,000 assumption sits *below* the real
effective window instead, so compaction runs early. Early compaction wastes
context; late compaction fails the session.

The generated environment sets `CLAUDE_CODE_DISABLE_1M_CONTEXT=1`. It has no
effect on plain model ids and exists as a one-sided floor: should a future client
classify unknown ids as long-context, the assumption is pinned down rather than
raised.

The proxy independently enforces the real window from catalog metadata, rejecting
an over-window request with a clear error rather than forwarding it into an opaque
upstream rejection.

The percentage the client displays is computed client-side against its own
assumption and cannot be corrected from here.

---

## 8. Credentials

Authentication uses OAuth with PKCE. The proxy operates its own client
registration and owns its own refresh-token family.

Credentials belonging to other tools are not imported. Refresh-token families
rotate, so sharing one means whichever client refreshes last invalidates the
other. One flow, one family, no ambiguity about which tool holds a valid session.

Refresh requests send `grant_type`, `refresh_token`, and `client_id` — **never
`scope`**. Including it causes the authorization server to re-scope the grant and
invalidate sibling refresh-token families. The body is JSON; the authorization
code exchange that precedes it is form-encoded. They differ, and sending the
wrong encoding is rejected.

A token response carries no expiry field. The expiry is a claim inside the
access token, and is read from there. Nothing verifies the signature, and
nothing should: the token arrived over TLS from the server that issued it, and
the proxy is reading its own credentials to learn when they lapse — not
deciding whether to trust them. Where no expiry can be read the token counts as
expired, because refreshing needlessly costs one request while using a dead
token fails the turn.

The account id is likewise a claim, read from the id token and sent upstream as
a header.

Refresh begins ahead of expiry and is single-flight: concurrent requests share one
in-flight refresh. A refusal naming an expired, reused, or invalidated grant —
or any 401 — marks the connection dead and requires re-authentication; it is
never retried in a loop. Every other refusal is transient and leaves the grant
alone, because marking it dead on a recoverable failure forces a re-login that a
retry would have made unnecessary.

Credentials sit behind a `CredentialStore` trait. The default implementation is a
file created `0600`. Platform keychains satisfy the same trait. Credentials never
appear in process arguments, logs, or the configuration file.

---

## 9. Testing

Development is test-first.

### 9.1 Translation

Every rule is a pure function over data and is specified by a failing test before
it is implemented. Table-driven cases cover mappings; snapshots cover emitted
frame sequences.

### 9.2 Upstream contract

What the backend sends cannot be invented. Ground truth is captured first, becomes
a fixture, and the fixture becomes the failing test. This is still test-first —
the test's content comes from observation rather than imagination.

### 9.3 Capabilities

A capability test must turn on content the model could not infer — random codes,
verbatim strings. A model handed nothing at all describes a file confidently from
its name, and that output is indistinguishable from success. Plausibility is never
evidence.

### 9.4 Transport and sessions

Transport tests run against a local server replaying recorded exchanges.
WebSocket coverage includes reuse, prewarm, fallback latching, and cancellation.

Incremental upload is specified by its invariants:

- a valid delta contains exactly the new items
- any change to a non-input field forces a full send
- a non-extending input forces a full send
- server-returned items are never resent
- a full send is always valid
