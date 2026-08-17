# Changelog

All notable changes to this project are recorded here. This project follows
[semantic versioning](https://semver.org). The semver-bound surfaces are listed
in [`docs/api.md`](docs/api.md) §6.

## [Unreleased]

### Added

- **The tier mapping and the effort ceiling can be changed on a running daemon**
  — `tiers.set` and `effort.set`. Both move what *routes turns*, not only what
  `status` reports. Neither writes the configuration file unless asked —
  `{"persist": true}` — and every answer says which it was, because a change
  the caller believed was saved and was not comes back at the next restart with
  nothing to explain it. `tiers.set` is validated
  against the catalog exactly as startup validates it, which is why this daemon
  owns the mapping rather than a front-end. It matters most for the ceiling: a
  capped turn **succeeds** and is simply shallower than it was asked to be, so a
  ceiling set once for one purpose silently governs every front-end that arrives
  afterwards, and nothing about that is visible.
- **`login` over the control socket**, so a front-end that is not a terminal can
  start the flow. It answers with the authorization URL and returns; the flow
  completes in the background and `status` reports when it landed. There is one
  fixed callback port, so a second caller joins the first rather than being
  handed a URL whose callback would then be rejected — and an abandoned flow
  releases the port instead of holding it until the daemon stops.
- **`usage.refresh`** asks the backend for a quota figure rather than waiting for
  one. The volunteered snapshot is still the primary path and still free; this
  covers the case it cannot — a front-end with a figure to show on a daemon that
  has served no turn yet. The response shape was **captured before the parser was
  written**, and differs from the stream event's in three ways a guess would have
  got wrong: the window keys, seconds rather than minutes, and where the plan
  sits.

### Fixed

- **A persisted tier or effort change was applied even when the write failed.**
  The caller was told the write failed while the daemon had already moved —
  running a policy nobody chose, and losing it at the next restart. Validated,
  written, then applied.
- **`effort.set --persist` could write the ceiling into the wrong table.** The
  search for an existing key scanned the whole file, so a commented-out `effort`
  line below a table header was rewritten in place — producing `tiers.effort`,
  which parses, which nothing reads, and which leaves the next daemon with no
  ceiling at all after the operator asked for one.
- **A refused grant read as healthy.** `status` reports `auth.dead` now:
  `connected` stays true while the credential file is readable, so nothing else
  said that every turn was failing.
- **Two concurrent sets could revert each other.** Each setter carried across
  the field it was not changing, read through one call and written through
  another; a mapping write that read the ceiling before another caller changed
  it put the old value back for good. Read and replace now happen under one
  lock, and `Snapshot`'s fields are private so the routing table cannot be set
  apart from the tiers it is derived from.
- **A status line showed this account's quota to a session that was not using
  it.** The wrapper is configured once and renders for every session the client
  runs, including ones pointed at their own provider, and the daemon answers
  `usage` whenever it is up — so switching back and forth painted one account's
  quota over another's, in the direction that reads as headroom. The merge now
  asks whether the session's model is one this daemon serves, and passes the
  payload through untouched when it is not. `usage` reports those ids: the
  configured tiers plus every id a turn was actually made against, since a
  client that names its own model bypasses the tiers entirely.

## [0.1.2]

A cost fix on the HTTP transport, and an install script.

### Fixed

- **The HTTP transport cached nothing.** Every turn over it is a full send with
  no `previous_response_id` chain, and the body's `prompt_cache_key` — which the
  spec called the thing that drives caching — turns out to do nothing on its
  own. A `session_id` header, stable for the life of a conversation, is what the
  cache is scoped by. Measured on one four-turn conversation with the WebSocket
  disabled: uncached input per turn fell from 4,465–4,497 tokens to 625–657.
  Over WebSocket it changes nothing, because chaining already caches.

### Added

- **An install script.** One command detects the platform, downloads the
  matching release, verifies it against the release's own `SHA256SUMS`, and
  installs it. There is no flag to skip verification, and a mismatch installs
  nothing and exits non-zero — proven by serving a deliberately corrupted
  archive, since a verifying script and a non-verifying one are
  indistinguishable on a good download.

## [0.1.1]

A defect in v0.1.0 that only an installed binary could show.

### Fixed

- **`doctor` works on an installed binary.** The fixture corpus was read from a
  `fixtures/` directory relative to the working directory, which only a checkout
  has — so the first command the README suggests skipped all eight probes and
  established nothing. The corpus is now compiled into the binary and answers
  when there is no directory. A directory named with `--fixtures` is still the
  only thing that answers for it, so a fresh `record` capture is never shadowed
  by the compiled-in copy, and the matrix names which corpus it read.

### Added

- **Install instructions**, with the checksum step, and `cargo install --git`.
  The absent package-manager routes — tap, container image, install script — are
  named as absent rather than left to be discovered.

## [0.1.0]

First release.

### Added

- **`doctor --live`** answers the capability probes from the real backend rather
  than from recordings, mapping the corpus's model ids through the configured
  tiers and spending at the configured effort ceiling.
- **`record upstream`** captures the whole exchange — the client's request and
  the stream that answered it — which is both halves of a fixture.
- **`[instructions]`** puts operator text around the client's system prompt: a
  lead naming the model that is actually answering, on by default, and an
  optional trailer placed where an instruction outranks the prompt above it.
- **`usage`** reports the quota the backend volunteers at the start of every
  stream — free, never polled, and absent rather than zero before a turn.
- **`statusline`** wraps an existing status-line script and merges that quota
  into the payload it already reads, so the script keeps working as written.
- **WebSocket compression.** `permessage-deflate`, negotiated on the upgrade.
  Measured on the wire, one identical turn with it offered and declined: about
  65% off in both directions, 267 KB on a single first turn. The inbound half is
  the larger one, because the backend echoes the whole request back three times
  per turn. It saves bytes and **no tokens** — quota is unaffected.
- **A working budget in the instructions, on by default.** The conversation is
  replayed upstream every turn and echoed back three times, so context pulled in
  is paid for repeatedly; without a budget the window goes quickly on reads that
  changed nothing. Switch it off with `working_budget = false`.
- **Defaults for every configuration key, and the file itself is optional.** A
  missing configuration is a first run rather than a failure. All four tiers
  default; a defaulted model this account cannot see is substituted for one it
  has, and said out loud, while a model the operator stated is never touched.
- **`status` reports the plan**, preferring what the backend said on the last
  turn over the older claim in the grant, and naming which answered. It also
  names any mapped model the catalog withholds — those pass validation, so
  nothing else would mention them.
- **`[upstream]`** makes the endpoints, the reported client version, and the
  usable share of a context window configurable, so a pinned binary can be
  repointed rather than rebuilt.

### Fixed

- **A tool call forked the conversation.** Arguments were compared as text, so a
  client replaying the same object with its keys in a different order looked
  like a different call, and every turn after the model wrote a file uploaded
  the whole history again.
- **`record.start` captured nothing.** The switch it set was read by `status`
  and by nothing else.
- **Two capability probes proved nothing.** Their attachments were valid base64
  and were not a PNG or a PDF, so they passed against a recording written to
  pass them. Both now carry real files.
- **A marker split across deltas read as missing**, failing every attachment
  probe against a backend that had read the attachment and said so.
- **An opaque access token refreshed on every request**, because the response's
  own `expires_in` was ignored.
- **The WebSocket upgrade carried no `originator` or `user-agent`**, which every
  other upstream path sends. Nothing tested the upgrade's headers.
- **A compaction window outside 100,000–1,000,000 was emitted and silently
  discarded** by the client, so it was not an early compaction or a late one but
  no setting at all. It is now omitted, with a warning.
- **The effort ceiling and the window guard were inert in the shipping binary**,
  which was handed a fallback catalog while the real one went to the control
  socket.

### Added — the foundation these sit on
- **Anthropic Messages ingress** on loopback: streaming `/v1/messages`,
  `/v1/messages/count_tokens`, and `/v1/models`, with the full error vocabulary
  and cancellation propagated upstream.
- **Request and response translation** covering the capabilities Claude Code
  depends on the server for — attachments inside tool results, the server-side
  search tool and its structured results, deferred tool discovery, reasoning,
  and the context meter.
- **Both transports.** WebSocket with one connection reused per session,
  prewarm, and incremental upload; HTTP with SSE as an equal fallback that a
  session latches to rather than retrying, carrying a zstd-compressed body.
- **OAuth with PKCE**, a `CredentialStore` behind a trait with a `0600` file
  implementation, single-flight refresh, and terminal handling of a refused
  grant.
- **Token accounting** with a self-calibrating estimator, chosen over a
  tokenizer by measurement recorded in `docs/proxy-behavior.md` §6.3.
- **A control socket** carrying every CLI verb, so a second front-end needs no
  new daemon work.
- **Capability probes and `doctor`**, keyed on content a model could not infer,
  reporting a matrix that states what it was run against.

### Known limitations

Listed in [`docs/api.md`](docs/api.md) §5. The suite never touches the network,
so every claim it makes is about the proxy's own half. The backend's half was
settled separately, against a live subscription: `docs/roadmap.md` §L records
each question and its answer, including the four rules the answers falsified.

Compression applies to both transports: zstd on an HTTP body,
`permessage-deflate` on the socket. Measured on a real turn, it takes about two
thirds off the wire in each direction, and the inbound half is the larger one —
the backend echoes the whole request back three times per turn. It saves no
tokens; quota is unaffected.
