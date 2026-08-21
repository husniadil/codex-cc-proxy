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

A conversation message carrying any role other than `user` or `assistant` is
carried as a `user` input item. The backend rejects the role, not the content.

Folding it into `instructions` instead looks equivalent and is not. The client
attaches per-turn content this way — a billing header among it — so
`instructions` changed on every turn. That breaks two things at once: a delta
requires every non-input field to be unchanged, so every turn uploaded the whole
conversation, and `prompt_cache_key` buys nothing when the cached prefix differs
each time. Measured against a real agent loop: three turns, no deltas at all.
Carried as input items instead, the same loop uploads only what is new.

**Nothing that varies per turn belongs in `instructions`.** Per-turn content
goes in the input, where it appends.

What may be added is text that does not vary: an operator-configured lead before
the system prompt and trailer after it.

The lead exists because the prompt the client sends is written for a different
model and opens by saying so. Nothing else in the request tells the model what
it actually is, and nothing in the client can be made to — its own
append-system-prompt flag reaches the same `system` field, so it can add to that
prompt but never precede it. Stating the identity *after* a prompt that already
asserted a different one reads as a correction rather than a fact, which is why
this is a lead and not part of the trailer.

The trailer is last for the mirrored reason: an instruction meant to take
precedence over the prompt above it has to come after it.

Between the two sits the **working budget**, and it is sent by default. The
premise is measured here rather than assumed: the conversation is replayed
upstream on every turn (§4.3) and the backend echoes it back three times per turn
(§4.4), so context pulled in is paid for repeatedly. Without a budget the model
reads broadly and spends the window fast. It asks for the smallest slice that
answers the question — a targeted search or a bounded line range over a whole
file — and for acting once a read is sufficient.

Its position is deliberate. After the client's prompt, because it exists to
overrule the parts of that prompt asking for broad reading before acting, and an
instruction placed before the one it modifies reads as a suggestion. Before the
operator's trailer, because a shipped default has no business outranking text an
operator wrote on purpose.

It is written as decision rules, with no *always*, *never*, or *must*. Those are
reserved for real invariants: a shipped absolute that collides with the client's
own prompt destabilizes more than a missing detail, and this text sits underneath
a prompt written for a different model that already says a great deal.

All three are per conversation, never per turn. A lead carrying a timestamp or a
token count would change `instructions` on every turn and cost the whole
incremental path — the same failure this section already describes, arriving
through the door built to prevent it.

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
shape and the only candidate that could carry a document.

**Claude Code does not use it.** Measured: asked to read a PDF, the client
rasterises it and sends `image` blocks, so a PDF reaches the model through the
image path — which is the shape the upstream client does exercise.

The document path exists for a client that sends `document` blocks, and the
backend does accept it — measured by posting one directly, which returned a
code that existed nowhere but inside the PDF. It would fail loudly if not: a
rejected part is a request error, not a silently dropped file.

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

Two ceilings apply, and the lower wins. The operator's is a cost decision. The
model's comes from the catalog, which states the efforts each one accepts —
they differ, and the client asked for a *tier*, so it cannot know that the model
behind it stops at `xhigh` while another goes to `max`. Forwarding an effort the
model does not support fails the turn for a reason the client could neither
anticipate nor fix. A model whose efforts the catalog never listed caps nothing:
unknown is not a limit.

The ceiling caps and never raises. A request asking for less keeps its own
choice, because capping a maximum is not a request to spend more. With no
request effort at all the ceiling still applies — an operator who capped effort
meant it for the traffic that expresses no preference too, and that is most of
it. With no ceiling and no request effort, the field is omitted and the
backend's own default applies.

`reasoning.summary` is always `auto`.

`prompt_cache_key` derives from session identity (§3.1) and is stable for the
life of a conversation. **It is not what drives the cache.** Sent alone, against
otherwise identical repeated requests, it produced no cached tokens in any
trial — in both orders, with independent prompts per condition. It is kept
because it is harmless and is what the field is for; nothing rests on it.

What the cache actually rests on is measured in §2.8.

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
| `accept` | `text/event-stream` on the HTTP transport |

All of these go on the WebSocket upgrade too. The upgrade is an HTTP request
like any other, and `originator` and `user-agent` were once absent from it while
every other path sent them — the socket did not enforce them, so nothing failed
and nothing said so.

**`session_id` carries the prompt cache scope**, and is stable for the life of a
conversation — a UUID, because that is the shape measured to work; whether an
arbitrary string is accepted there is unmeasured.

What it is worth depends on the transport, which is the part that is easy to get
wrong. Over WebSocket it changes nothing: the incremental path chains turns with
`previous_response_id` (§4.3), and that already caches. Over HTTP every turn is
a full send with no chain, and there the header is the whole difference —
measured on one four-turn conversation, uncached input per turn fell from
4,465–4,497 tokens to 625–657, with 3,840 reported cached from the second turn
on.

That makes it a fallback-path optimisation rather than a universal one, which is
worth stating plainly: HTTP is a normal operating mode here (§4.2), not an error
path, so a turn that costs seven times its input tokens is a real cost and not a
hypothetical one.

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

Items are compared by content, not by encoding. A server-assigned `id` is
absent when the client replays the same turn, so ids are excluded. A tool call's
`arguments` travel as a JSON *string*, and the backend emits its keys in the
order the model produced them while the client replays the object it parsed, in
whatever order its own serializer chose — so arguments are compared as parsed
values where they parse, and as literal text where they do not. Measured live:
compared as text, every turn after the model wrote a file forked the
conversation and uploaded the whole history again.

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

A prewarm request opens a connection before the turn that will use it, so that
turn pays for neither the handshake nor a cold connection.

The daemon does not prewarm in v0.1, and the capability exists unused. A proxy
learns that a conversation exists only when its first request arrives, at which
point opening the connection and sending on it are the same act. Prewarming
needs a signal that a turn is *about* to happen — a front-end that knows the
user is typing has one; an HTTP surface does not.

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

The connection is part of that validity, not just the session. A response id
names a response held by the connection that produced it, so a delta may only be
sent on the connection that has seen that response. Handed to any other — a
fresh connection opened after an abandoned turn dropped the previous one, or a
connection parked by a turn that produced no response — the backend refuses it
with `400 Invalid previous_response_id` (observed live). That refusal ends the
turn cleanly, so the refusing connection is parked and every following delta
repeats the refusal: the session never heals on its own. A turn whose pooled
connection did not produce the response it would continue is therefore a full
send.

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

Compression belongs to the transport, and the two transports do it differently.

**HTTP**: the body is zstd-compressed and announced with `Content-Encoding:
zstd`. The header is the whole mechanism — compressed bytes without it are bytes
the backend cannot parse, and it refuses the request with an error naming
nothing. Only bodies above a threshold are compressed; below it, compression
adds more than it removes.

This is measured against the subscription backend and asserted about no other.
A request spent with a key is never compressed, whatever `[transport]` says —
§8.2 carries what happens when it is.

**WebSocket**: `permessage-deflate`, negotiated during the upgrade rather than
chosen per message. The client offers the extension and the server selects it
(RFC 7692), so declining to offer it is the only way to switch it off. The frame
is text JSON either way — the library compresses that same text frame and marks
it in the frame header, not in the payload.

**Measured live**, one identical turn with the extension offered and declined,
counted on the wire rather than simulated:

| | offered | declined |
|---|---|---|
| inbound | 104,566 | 300,879 |
| outbound | 40,335 | 110,608 |

About 65% in both directions. The inbound half is the larger one and grows with
the conversation, because the backend echoes the entire request back in
`response.created`, `response.in_progress`, and `response.completed` — three
copies of it per turn, which nothing else here has a lever on.

Two further figures are **derived, not confirmed**: running deflate offline over
a captured turn predicted 37,488 and 99,100 bytes, and put the contribution of
context takeover at about 3% — the window is 32 KiB and cannot reach back across
a 99 KB event. The server does negotiate takeover (it selects bare
`permessage-deflate`, with no `no_context_takeover` and no window limit), but
nothing here has measured what it is worth on the wire.

The read limit is raised far above the library's 1 MiB default for the same
reason: one event legitimately carries a whole conversation, and a cap sized for
ordinary messages would sever long ones mid-turn.

This saves bytes and **no tokens at all**. It is worth doing because two thirds
of the traffic is an echo the proxy has no other lever on, not because bandwidth
is scarce.

A binary frame is *not* a way to say "compressed". Nothing in the protocol
attaches that meaning to it: the backend reads a binary frame as JSON, fails,
and refuses the request. Measured directly — plain JSON in a binary frame is
accepted, the same JSON compressed is not.

This compounds with §4.3 where it applies: incremental upload removes most
turns' bulk, and compression reduces what remains on the turns where a full send
is unavoidable. Those are the HTTP turns, which is where compression is
available.

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

That comparison was against a *modelled* count, linear in the same structure the
raw estimate measures, so a linear fit could absorb it exactly. It demonstrated
the mechanism and not the accuracy.

**Measured against the real backend**, over a growing six-turn conversation:

| turn | estimated | actual | error |
|---|---|---|---|
| 1 | 146 | 75 | +95% |
| 2 | 221 | 215 | +2.8% |
| 3 | 420 | 427 | −1.6% |
| 4 | 703 | 687 | +2.3% |
| 5 | 1026 | 1007 | +1.9% |
| 6 | 1406 | 1387 | +1.4% |

So the real relationship is tractable: one observation is enough to bring the
estimate inside 3%, and it stays there as the conversation grows.

The first turn is the weak point and cannot be otherwise — nothing has been
observed yet, so it is the uncalibrated ratio and here it nearly doubled the
true figure. It is the one turn where the context meter is visibly wrong, and it
corrects on the next one.

Before a session's first completed request the estimate is uncalibrated.

---

## 7. Models

### 7.0 Catalog

The catalog is fetched from the backend at startup and held for the life of the
daemon. There is no TTL: a model added, renamed, or withdrawn after the daemon
started is not noticed until something else makes it ask again. The one thing
that does is the daemon changing which account it serves, for the reason below.
That is also why a mapping validated at startup cannot go stale on its own, and
why the only mismatch worth reporting otherwise is a mapped model the catalog
withholds (§7.1).

**A catalog is one account's menu.** The plan decides which models appear and
which efforts each one offers, so a list fetched for one account is not a
statement about another. It is attributed to the account it was fetched for,
and fetched again when the daemon changes which account it serves — selecting
another, or forgetting the one that was serving. A failed refetch **keeps the
list already in force**: fetch failure is not evidence that a model went away
(§7.1), and replacing a real list with the fallback would withdraw models the
account has.

Attribution is what covers the rest. A grant can still arrive without anything
to refetch on: a login started over the control socket completes in the
background, and a login made in the CLI while no daemon is running has no socket
to hand over on. The list stays the previous account's, and every answer built
from it says so (`api.md` §3) rather than presenting it as this account's.

Each entry contributes an id, a visibility flag, and window metadata: a context
window, an optional maximum context window, and an optional effective percentage.
Hidden entries and non-conversational pseudo-models are excluded from what is
offered for mapping, but their window metadata is retained — a session may
reference a model the picker filters out, and knowing its window is better than
not.

The effective window is the context window scaled by the effective percentage,
which reserves headroom for instructions, tool overhead, and output. Where the
entry states no percentage, the configured `upstream.effective_window_percent`
applies — a default, never an override: a percentage the catalog states for its
own model wins. It is resolved when the catalog is parsed, so there is no
compiled-in figure left to fall back to. Where both a context window
and a maximum context window are present, the smaller-scoped `context_window` is
authoritative — the maximum describes a ceiling the account may not have.

A fixed fallback list covers a failed fetch, so the daemon starts and reports
honestly rather than blocking on an unreachable catalog. The fallback carries ids
only. A model with no known window is **unknown, not assumed**: the window guard
(§7.2) does not fire for it, and no percentage is derived from a guess.

Fetch failure is not the same claim as absence. Validation that depends on the
catalog is skipped when the catalog is unavailable, never failed.

### 7.1 Tier mapping

All four tiers — `opus`, `sonnet`, `haiku`, `fable` — are mapped, by the
operator or by the shipped defaults, and each is validated against the live
catalog. The daemon refuses to start on an invalid mapping; an incomplete one is
completed rather than refused.

The client routes different work to different tiers, and background and
summarization traffic runs on the cheapest one. An earlier rule required all four
to be stated, on the grounds that a defaulted mapping hides which model handles
that traffic and what it costs. `status` prints the mapping in use whether or not
it was written down, which meets that concern without making a first run fail on
a file nobody had written yet. A tier written blank is still refused: an omission
accepts the default, a blank is a mistake.

If the catalog cannot be fetched, validation is skipped rather than failed. An
unreachable catalog is not evidence that a model went away.

**The mapping belongs to an account.** A catalog is one account's menu (§7.0),
so a single mapping is only ever right for the models every stored account has,
and that intersection shrinks with each account added: two subscriptions on
different plans are offered different models, and a key account beside a
subscription need not overlap at all. An account therefore states what differs
for it — the tiers it names and nothing else, plus its own effort ceiling — and
the shared tables answer for everything it does not state. Keyed by the name the
store files it under, because that is what every account verb takes and a key
account carries no id to be named by.

**A switch re-resolves the mapping and can be refused by it.** Selecting an
account resolves that account's tiers and ceiling and validates them against
the catalog fetched for it, before anything else moves. A mapping naming a
model that account's catalog does not have refuses the switch and leaves the
daemon where it was, catalog included — the alternative is a daemon serving an
account whose every turn is dispatched to a model the backend will not answer
for, which fails one turn later, upstream, saying nothing about tier mapping.
Validation is skipped where the catalog cannot speak for the account being
selected, which is the fallback list and a refetch that failed; refusing a
mapping over somebody else's menu would be worse than not checking.

An account's ceiling **replaces** the shared one rather than being capped by it.
Capping would make an account section unable to raise, and an operator who
writes a different ceiling for one account means that one. The cap that is not
negotiable is the model's own (§2.7), and that one is derived from the catalog
rather than from either line.

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

The generated environment sets `CLAUDE_CODE_DISABLE_1M_CONTEXT=1`, and it is not
a precaution against a hypothetical future client — it is load-bearing now.
Measured: without it, this client appends `[1m]` to the unrecognized id and
assumes a million tokens. With it, the id stays plain. That is the four-times
overestimate this section warns about, and the flag is what prevents it.

Where the catalog knows the window, the environment also states it:
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` and `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, both set
to the effective window, and to the smallest across the mapped tiers since one
value covers them all.

Both are needed. Stating the window alone is worse than saying nothing: the
client stops applying its own 200,000 assumption and, not recognizing the model,
then enforces no limit at all — so the session grows until the backend refuses
it. The compact window is what turns a stated figure into an enforced one.

**`CLAUDE_CODE_AUTO_COMPACT_WINDOW` is only set when the effective window falls
between 100,000 and 1,000,000**, which is the range the client accepts. Outside
it, the client answers "Expected 'auto' or 100k–1M tokens", and the settings key
of the same meaning is declared to *discard* an out-of-range value rather than
reject it — so a figure outside the range is not an early compaction or a late
one, it is no setting at all, and nothing would say so. The proxy omits it and
warns instead. Both ends are reachable: a small model can fall under the floor,
and so can a large window with a low `upstream.effective_window_percent`.

Compaction does fire against this figure. The client's own threshold is
described as the effective window minus a summary buffer, lowered further by an
override it reads separately, and the history-length check compares the token
count against a function of that window. **Derived from the client's code, not
observed** — no session here has been run long enough to watch it happen.

The client warns that its 200,000 limit is not being enforced. That warning is
correct and expected: exceeding 200,000 is the point, and the real window is
larger. It is silenced only by compacting at 200,000, which would discard a
fifth of the usable context to avoid a message.

The percentage the client displays is computed client-side and is now computed
against the right number.

The proxy independently enforces the real window from catalog metadata, rejecting
an over-window request with a clear error rather than forwarding it into an opaque
upstream rejection.

### 7.3 Client policy

Two of the things a client has to be told cannot be an environment variable.
They live in its settings file, and no export reaches them: checked against the
whole settings schema, there is no per-skill variable and nothing that points at
an extra settings file. The one variable that comes close relocates the client's
entire state directory, credentials and history included, which is not a price
this buys anything worth.

So the proxy **publishes** this policy and never installs it. It is emitted
beside the environment (`docs/api.md` §2.2) and applied by whoever starts the
client: a person writing it into a settings file, `exec` splicing it into one
launch (§2.3), or a supervisor merging it into the argument list it already
builds. Nothing here writes into a file the proxy does not own.

Settings layers union, measured: a rule in a project settings file and a rule on
the command line were both enforced in the same session, while a control skill
denied by neither still launched. A deny rule also survives an untrusted
workspace, where an allow rule is dropped. So the policy can be delivered
through any layer without displacing what is already there — with one exception,
which is that **two `--settings` flags on one argument list are not two layers**:
the client keeps the last and drops the first, silently. That is why `exec`
refuses a collision rather than choosing a side, and why a supervisor that
already passes the flag merges into its own document rather than adding a
second.

**A bundled skill is denied by default.** `claude-api` is a reference for another
provider's API. A session served here is not talking to that API, so the
reference is wrong twice over: it costs context, and a model that reads it
answers confidently about model ids, prices, and parameters that are not its own.

The figures are measured, against a local capture stub with nothing forwarded
anywhere:

| | |
|---|---|
| Skill content injected on one invocation | 73,000 to 93,000 bytes, roughly 18,000 to 23,000 tokens |
| Cost of a refused invocation | one 43-byte error result |
| Effect on the listing the client sends | **none** |

That third row is the one that decides the design. Denying does not remove the
skill from the listing, so the model may still reach for it and lose a turn
finding out. What the deny stops is the load. The arithmetic is what settles it:
one blocked call costs 43 bytes, one allowed call costs four orders of magnitude
more than that and then keeps costing.

The skill figure is a range because both ends were measured and it moves with
what else the session has loaded — the same probe read 92,601 bytes in a
populated environment and 73,214 in a bare one. Quoting either alone would claim
a precision the measurement does not have. Nothing about the decision turns on
where in that range it lands.

**What "keeps costing" means precisely.** In tokens, always: the content lands as
a user item, so it sits in the conversation for the rest of the session and is
charged on every turn, and it moves compaction earlier by that much. In bytes, it
depends on the transport. Over HTTP the whole conversation is re-sent every turn
(§4.2), so it is re-uploaded each time. Over WebSocket the incremental path sends
only what is new (§4.3), so it is uploaded once — but the backend echoes the
entire request back in `response.created`, `response.in_progress`, and
`response.completed` (§4.4), so it returns three times per turn on either
transport. The token cost is the one that holds regardless.

**The connector notice** is suppressed by the same document. The client prints it
whenever an auth token is set, which here is always. Whether the setting still
silences it on the current client version is **unverified here** — see
`docs/roadmap.md` §L.

**Both are switchable**, in `[client]`, where the comments explaining them live.
An operator building against that provider's API wants the reference the rest of
us are paying to avoid, and an empty `deny_skills` gives it back. The default is
on for the same reason the working budget is (§2.1): the cost was measured and
the alternative is worse.

**The policy is published even when it is empty.** One file is both the daemon
and the CLI, and replacing it on disk does not restart a running daemon, so a
newer CLI against an older daemon is what an ordinary upgrade leaves behind. If
an empty policy and a daemon that cannot answer for one looked the same, nothing
could tell the operator which they had — so the payload always carries the field
and absence means only that the daemon predates this. The verbs whose output
would otherwise be quietly incomplete refuse; the one that carries routing alone
continues and says which daemon answered. `docs/api.md` §2.2 and §6.

**A denied call is attributed by `status` and nowhere else.** The client refuses
with "Skill execution blocked by permission rules" and names no source, so
`status` reports the policy under the configuration's own key names — the person
holding that message needs the key that undoes it, not a restatement of it.

---

## 8. Credentials

Authentication uses OAuth with PKCE. The proxy operates its own client
registration and owns its own refresh-token family.

Credentials belonging to other tools are not imported. Refresh-token families
rotate, and sharing one means two clients writing over each other's stored
grant. A superseded token was measured still redeeming shortly after rotation,
so this is about ownership rather than immediate breakage — but a client that
does not hold the current token is one refresh away from holding nothing. One
flow, one family, no ambiguity about which tool holds a valid session.

Refresh requests send `grant_type`, `refresh_token`, and `client_id` — **never
`scope`**. Including it causes the authorization server to re-scope the grant and
invalidate sibling refresh-token families. The body is JSON; the authorization
code exchange that precedes it is form-encoded. They differ, and sending the
wrong encoding is rejected.

The expiry is a claim inside the access token, and is read from there. That is
the figure the backend validates against, so it is the one that decides.
Nothing verifies the signature, and nothing should: the token arrived over TLS
from the server that issued it, and the proxy is reading its own credentials to
learn when they lapse — not deciding whether to trust them.

The token response also carries `expires_in`, which agrees with the claim. It is
the fallback for an access token this proxy cannot read, and only that. Where
neither can be read the token counts as expired, because refreshing needlessly
costs one request while using a dead token fails the turn.

The account id is likewise a claim, read from the id token and sent upstream as
a header.

Refresh begins ahead of expiry and is single-flight: concurrent requests share one
in-flight refresh. A refusal naming an expired, reused, or invalidated grant —
or any 401 — retires that refresh token and requires re-authentication; it is
never retried in a loop, and the retirement follows the token rather than the
process holding it (§8.1). Every other refusal is transient and leaves the grant
alone, because marking it dead on a recoverable failure forces a re-login that a
retry would have made unnecessary.

Credentials sit behind a `CredentialStore` trait. The default implementation is a
file created `0600`. Platform keychains satisfy the same trait. Credentials never
appear in process arguments, logs, or the configuration file.

### 8.1 More than one account

One store holds several grants, and one of them is **selected**: the account
every turn is made as, and the only one `CredentialStore` reports. A caller that
authenticates a request sees one grant and needs to know nothing else. Which
grants exist and which is selected is the `AccountStore` half.

An account is **identified** by the account id its grant carries, and *named*
by an operator's label where one was given, otherwise by that same id,
otherwise by an assigned `account-N`. The name can be changed afterwards
without touching the grant: an id is what the backend calls the account and a
name is what the operator calls it, and correcting the second is not a reason
to spend an authorization on the first. A name another account already holds is
refused, because two entries answering to one name would hand the turns to
whichever was found first. The two are different questions: a login
carrying a label for an account already stored renames that account, and a
login carrying none keeps the name it is already filed under. Neither adds a
second entry for it. Nothing in a
grant other than the account id is an account id, so a store with neither label
nor id assigns a name rather than deriving one from a token — a name taken from
a token would be a fabricated fact about the account, and a secret in a field
meant to be printed.

A label that already names a *different* account is refused rather than
honoured. Taking it would write the new grant over the one holding that name,
which is the silent retirement this split exists to prevent. The refusal costs
the authorization just spent, and one more login replaces it; the other way
costs a grant that may not be replaceable.

**A login adds; a refresh saves.** They are different verbs on purpose. An
authorization produces a new grant, and writing it over whichever account
happened to be selected would retire a working one with nothing said. A refresh
writes the grant of the account it read, and appending there would leave two
entries sharing one refresh-token family. Authorizing an account that is already
stored replaces that account's grant rather than adding a second entry for it,
for the same reason.

Both resolve the entry by account id rather than by the selection at the moment
of writing. A refresh is a read, a network round trip, and a write; the
selection can move in between, and a write aimed at whatever is selected *then*
drops one account's rotated grant into another's entry — destroying a refresh
token that only a re-login replaces, and leaving that account authenticating as
somebody else. Only a grant carrying no account id falls back to the selection.

The file is **replaced, never truncated in place**: the new content is written
beside it and moved over it, under a name carrying the writing process's id so
two writers cannot interleave into a file that is neither. One account's
rotated token is not worth risking every account to a write that stops halfway.

Every write is taken **under a lock the filesystem enforces**, held for as long
as it takes to read the file, change it, and replace it. Every write rewrites
the whole file, so two overlapping writers would otherwise mean one discarding
whatever the other had just done — a whole account, not a stale token — and the
pair that overlaps in practice is real: `login` in the CLI writes this file
directly while the daemon may be persisting a refresh.

The lock is a file of its own beside the credentials, never read and never
written. It cannot be the credential file, because a write replaces that one by
rename, and a lock held on it would be a lock on an inode the next writer never
opens. It is advisory and the kernel drops it when the descriptor closes, so a
process that dies partway through a write leaves nothing behind for the next one
to wait on, and it stays on disk when the credentials are cleared, because
removing it would leave the next two writers locking two different files.

A filesystem that cannot take the lock **cannot hold credentials**, and the
write says so rather than proceeding without one. Locking is not universal — a
home on a network mount is the case that exists — and the alternative is a write
that reports success while doing exactly what this rule was written to stop.
The failure names the file and names the move: `CODEX_CC_PROXY_HOME` points the
whole directory somewhere local. Falling back to an unlocked write is
deliberately not offered; if it ever is, it belongs behind something the
operator chose, not behind a log line nobody reads.

A write that finds the file changed since it read still **starts over** rather
than replacing it. The lock reaches only writers that take it, and an older
binary or a hand edit takes none; the comparison is what catches those. It
cannot close the window on its own — the comparison and the replacement are two
operations, and a writer landing between them is lost — which is what the lock
is for.

**Accounts do not interfere with each other.** Each holds its own
refresh-token family, so rotating one leaves every other exactly where it was.
This is a property of separate grants, not a measured property of rotation: a
superseded token was once observed still redeeming and later refused as
`refresh_token_reused`, so nothing here depends on a superseded token staying
usable. What must be kept out of the design is the other arrangement — two
holders of *one* account — because there the last refresh retires the token
every other holder is still carrying.

**A refusal is about one refresh token**, and is held as that token rather than
as a flag. Held as a flag it becomes a fact about the process: the message a
refusal produces tells the operator to log in again, they do, and the grant they
produce lands in this store — where a flag would go on refusing it until the
daemon restarted, and a login through the CLI never reaches the daemon at all.
Held as the token, a grant that is not the refused one is tried and the refused
one is still never retried, whether it comes back by a switch or by a login.

A quota belongs to the account that earned it, so switching accounts drops the
snapshot and the next turn supplies one. Reporting the previous account's
headroom is wrong in the direction that reads as room to spend.

Clearing forgets one account and leaves the rest usable, selecting another to
serve turns; clearing the last one removes the file, so "not authenticated" is
still read from its absence. Clearing what is already gone is not an error.

A credential file written before the store held more than one account is a bare
grant, and is read as the single account it describes, named by its account id.
It migrates on the next write rather than on read: reading credentials is not a
reason to rewrite them. A `selected` naming an account that is not stored falls
back to the first one, because the file still holds usable grants and answering
"not authenticated" there sends an operator to re-authorize for nothing.

### 8.2 A credential that is not a subscription

An account holds one of two kinds. A **grant** is the OAuth credential above:
refreshed, expiring, carrying an account id and a plan. A **key** is one
secret. It has no refresh, no expiry, no account id and no plan, and nothing
reports a plausible value in place of any of them — an invented expiry would
drive a refresh that cannot happen, and an invented account id would put a
header on the wire the endpoint taking a key never asked for.

Every account verb works on either: list, select, rename, forget. What differs
is where the credential may be spent, and that difference is the point.

**One place resolves a credential into headers**, and every path that
authenticates asks it: both transports, the catalog fetch, and the quota fetch.
A grant answers with its bearer token, the `originator` identifying a
subscription client, and the account id it is spending. A key answers with its
bearer token and nothing else. The `originator` moved here from the transports
for that reason: it belongs to the subscription dialect rather than to every
request. A transport holding no credential at all still sends it, which is what
the replay paths have always seen.

**A credential is refused against the other kind's endpoint**, before anything
is sent, in a message naming both halves. The model list is paired
structurally rather than checked: the daemon holds one endpoint per kind and
picks by the credential it is about to spend, so there is no argument that
could cross them. The endpoint would otherwise answer
with something about an invalid token, which sends whoever reads it looking for
the wrong problem.

Three things follow from that pairing rather than being decided separately. A
key request is **never compressed**: zstd on a request body is measured against
the subscription backend (§4.4) and nowhere else, and an endpoint that does not
decompress it parses the bytes as JSON and rejects them — observed live as a
unicode decode error naming neither compression nor the endpoint. A key
account uses **HTTP only**: the WebSocket protocol here belongs to the
subscription backend, and nothing has been observed about a key endpoint
speaking it, so there is no socket to fall back from. And a key account has
**no quota to report**: the figure is a subscription entitlement, so asking for
one with a key is the same refusal rather than a request spent to be told so.

A fourth follows from the catalog rather than the pairing. The key endpoint's
model list is real and authoritative — it answers with every model the key can
reach — and it carries no window and no supported efforts for any of them. So
for a key account the window guard (§7.2) never fires and the model half of the
effort cap (§2.7) has nothing to cap against; the operator's configured ceiling
still applies, because that one is not derived from the catalog. Nothing is
silently wrong: the list is the account's own, and every entry states no window
rather than a guessed one.

A login through the CLI **hands over to a running daemon**, because the daemon
reads the file on every request but nothing else about a switch happens on its
own: the conversations bound to the previous account keep the endpoint they
dialed, and after a change of kind that endpoint refuses every turn they carry.
No daemon running is the ordinary case for a login and not a failure of one. A
credential file edited by hand still gets none of that.

A key is stored from **stdin**, never from an argument, and under a name the
operator gives — a command line is visible to every process on the machine and
lands in shell history, and a key carries no id to be named by.

Neither kind can be stored over the other. A key written where a grant is
would retire that grant with nothing said, and a rotation whose account is no
longer stored is refused rather than appended — appending it would create an
account nobody asked for and make it the one serving turns, from a background
refresh.

An account with no kind recorded is a grant, the same read-the-old-shape rule
§8.1 applies to the file as a whole.

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
