# Changelog

All notable changes to this project are recorded here. This project follows
[semantic versioning](https://semver.org). The semver-bound surfaces are listed
in [`docs/api.md`](docs/api.md) §6.

## [Unreleased]

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
