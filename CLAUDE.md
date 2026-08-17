# codex-cc-proxy

Claude Code, running on OpenAI models served through a ChatGPT subscription,
without modifying Claude Code. An Anthropic Messages API on the front, the
OpenAI Responses API on the back, and a translation layer between them whose
real job is keeping Claude Code's built-in tools working.

## Commands

- `just test` — the suite. Run it on every edit.
- `just check` — **the gate**, and what CI runs: formatting, `clippy -D
  warnings`, and the suite. Run it before every commit.
- `just test-one <filter>` — one test, by name.
- `just snapshots` — review pending `insta` snapshot changes.
- `just run` / `just record` / `just doctor` — drive the daemon, capture
  fixtures, probe a backend.

**No test touches the network.** Every upstream interaction in the suite runs
against a local replay server, so the suite is fully green without credentials
and without quota. That is a design constraint, not a convenience: a test that
needs a live backend is a test that stops running the moment quota runs out.

`just doctor --live` and `just record upstream` are the only things here that
spend quota, and neither is part of the gate. Plain `just doctor` answers the
same probes from the fixture corpus and contacts nothing; `just record ingress`
captures what the client sends and costs nothing.

## The specification comes first

[`docs/proxy-behavior.md`](docs/proxy-behavior.md) is the normative spec for how
the proxy translates, transports, and accounts. It is not documentation written
after the fact — it is the definition the code is measured against, and most of
its rules exist because the obvious implementation is wrong in a way that does
not fail loudly.

[`docs/api.md`](docs/api.md) is the companion contract for what the proxy
*exposes*: the ingress surface, the error vocabulary, the CLI, the control
socket, the configuration keys, and which of those are semver-bound.

**Read the relevant section before touching translation, transport, sessions, or
token accounting.** If implementation shows a rule is wrong, change the spec in
the same commit as the code that proved it. A spec that drifts from the code is
worse than no spec, because it is still believed.

[`docs/roadmap.md`](docs/roadmap.md) has the ordered phases and what "done" means
for each.

## Non-negotiables

1. **Harness fidelity is the product.** This is not a generic protocol
   translator; a dozen of those exist. It is the one that keeps `Read`,
   `WebSearch`, `WebFetch`, tool search, and the context meter behaving as Claude
   Code expects. Every one of those has a failure mode that returns plausible
   output instead of an error — an empty search that reads as "no results", a
   file described from its name. A change that trades any of them for
   convenience is not a tradeoff this project makes.

2. **Never fabricate a number upstream can supply.** Token counts from a
   completed response are authoritative and are never recomputed.
   `cache_creation_input_tokens` is zero because no write event exists, and it
   stays zero rather than being synthesized into something plausible. Where a
   figure is genuinely unavailable — the two estimation points in
   `docs/proxy-behavior.md` §6.2 — it is estimated, corrected against ground
   truth within the same exchange, and documented as an estimate.

   The same rule governs claims. A capability verified against replayed fixtures
   is derived, not confirmed; say which one it is. `docs/roadmap.md` §L holds
   what only a live backend can settle.

3. **Falling back is always safe; a wrong delta is not.** Incremental upload
   sends less by asserting the conversation is a strict extension of what was
   sent before. When that assertion cannot be proven, send everything. A full
   send costs bandwidth; a wrong delta corrupts a conversation and does not fail
   visibly. Every ambiguous case resolves toward the full send.

4. **A capability claim needs an unguessable probe.** A model handed no file at
   all will describe one confidently from its filename, and that output is
   indistinguishable from success. Any test asserting an attachment, a search
   result, or a tool round-trip must turn on content the model could not infer —
   random codes, verbatim strings. Plausibility is never evidence.

5. **Anthropic error shapes, always.** Every failure leaves as
   `{"type":"error","error":{"type":...}}` with a type Claude Code's own retry
   logic understands. Transient conditions surface as retryable; terminal ones
   surface as terminal. The proxy does not build a second retry loop on top of
   the client's.

6. **Loopback only, no authentication, no telemetry.** The daemon refuses to
   bind anything but `127.0.0.1`, so every caller is already a local process
   running as the user. `ANTHROPIC_AUTH_TOKEN` must be set for Claude Code's
   sake and its value is ignored. Nothing is collected, nothing is transmitted.

7. **Credentials never reach argv or logs.** They live behind `CredentialStore`,
   in files created `0600`. The configuration file is not one of those places.

## Working agreements

- **Development is test-first.** Failing test, then the code that passes it, then
  refactor. For translation this is straightforward: every rule is a pure
  function over data, and the expected output is a specification.
- **Upstream behavior is captured, never guessed.** What the backend sends cannot
  be invented. Record it, make the recording a fixture, write the failing test
  against the fixture, then implement. That is still test-first — the test's
  content comes from observation rather than imagination. `just record` exists
  so this is one command.
- **Spike before proposing a fix to measured behavior.** A probe that falsifies
  the idea is a good outcome; a confident guess is not.
- **The spec is amendable.** If implementation disproves a rule, change it — same
  commit as the code that proved it.
- **Commit at checkpoints.** Small, working increments, each reviewable and
  revertible on its own.

## Layering

```
ingress ──── Anthropic Messages surface (axum)
                        │
core ─────── translation: Messages ⇄ Responses
             pure functions and state machines, no I/O
                        │
session ───── per-conversation state
             input baseline · transport binding · calibration
                        │
transport ─── WebSocket (primary) │ HTTP + SSE (fallback)
                        │
auth ──────── OAuth lifecycle, CredentialStore
```

`codex-cc-proxy-core` holds the middle layer and nothing else: no sockets, no
clock, no filesystem, no configuration policy. That boundary is what makes every
translation rule testable as a pure function over recorded data, and what stops
transport assumptions from leaking into the part of the system that has to be
exhaustively covered. A translation rule that needs I/O to test has been written
in the wrong crate.

Transports are interchangeable below `session`. WebSocket is primary and HTTP is
its fallback, but neither is a degraded version of the other, and HTTP is a
normal operating mode rather than an error path. The backend is documented to
close sockets under policy conditions; **no such close has been seen on the
account tested here**, and one account's experience is not evidence about every
account's — which is why the fallback is covered as an ordinary path rather than
an exceptional one.

Compression applies to both: zstd on an HTTP body, `permessage-deflate` on the
socket. It saves bytes and never tokens.

## Naming

Identifiers describe what they do, not who calls them. The upstream is a
provider, not a brand; the client is a harness, not a product tier. Comments may
name a real client or endpoint where that is the accurate explanation for a rule
— the constraint is on names, not on prose that has to be true to be useful.
