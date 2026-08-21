# Changelog

All notable changes to this project are recorded here. This project follows
[semantic versioning](https://semver.org). The semver-bound surfaces are listed
in [`docs/api.md`](docs/api.md) §6.

## [Unreleased]

### Added

- **More than one account in one credential store.** One file meant one
  account, so an account out of quota stopped all work rather than some of it.
  `login` now adds an account and selects it rather than replacing the one
  already stored, `--as NAME` gives it a local name, `accounts` lists what is
  stored with the serving one marked, and `accounts --use NAME` switches —
  through the daemon, which is the side holding the selection. `status` names
  the account serving turns beside the rest, and `disconnect` says which one it
  cleared and leaves the others usable. Two things turned out to belong to the
  grant rather than to the daemon and travel with a switch: a refusal, which is
  a statement about one refresh token, and the quota snapshot, which belongs to
  the account that earned it — carrying either across reports the new account as
  finished, or reports headroom it may not have. A credential file written by an
  earlier version is read as the single account it describes and migrates on the
  next write, so an upgrade costs no re-login.

  Three things about that store are worth stating on their own, because each
  was a way to lose a grant rather than a feature. An account is identified by
  the account id its grant carries, not by the name it is filed under, so
  authorizing one already stored replaces it instead of leaving two entries
  sharing one refresh-token family; a label already naming a *different*
  account is refused rather than honoured. A refresh writes the account its
  grant belongs to, not whichever is selected when the write lands, so
  switching accounts during one cannot drop A's rotated grant into B's entry.
  And the file is replaced rather than truncated in place, because it now holds
  every account and a write that stops halfway would take all of them.

- **`run --detach` starts the daemon in the background.** The command returns
  once the daemon answers the control socket, printing the pid, the log path,
  and `stop` as the counterpart. A child that dies at startup is reported with
  its own log quoted and a nonzero exit; a second detach is refused while a
  daemon still answers, because it would take over the first one's socket file.

- **The proxy publishes the policy a client cannot be told by environment
  variable.** Two of the things a client has to be told live in its settings
  file, and no export reaches them — checked against the whole settings schema,
  there is no per-skill variable and nothing that points at an extra settings
  file. `[client]` now carries `deny_skills` and `disable_connectors`, the `env`
  control method carries a `settings` half beside `variables`, and a new
  `settings` verb prints one complete client settings document. That document is
  complete on its own: measured, a client reading only its `env` block, with no
  `ANTHROPIC_*` in its environment, still reached the proxy.
- **The bundled `claude-api` skill is denied by default.** Measured against a
  local capture stub, one invocation lands 73,000 to 93,000 bytes — roughly
  18,000 to 23,000 tokens — in the conversation as a user item, where it sits
  for the rest of the session and is charged every turn, while a refused
  invocation costs a 43-byte error. A range because both ends were measured and
  the figure moves with what else the session has loaded. Denying does not remove it from the listing the client sends; what it
  stops is the load. It is also the wrong reference for a session served here,
  documenting another provider's model ids, prices, and parameters. Switchable
  in `[client]`, and `status` names it so the person holding "Skill execution
  blocked by permission rules" can find the key that undoes it.
- **`stop`**, so a daemon can be replaced with the CLI that replaced it. It
  watches the `instance` id `status` now carries rather than watching the socket
  fall silent, because silence is a statement about timing: a supervisor quick
  enough leaves no gap to see, and one that throttles a respawn leaves a gap
  longer than any sensible wait. Under a
  supervisor, stopping is how a running daemon picks up the build on disk, and
  this reports what it observed afterwards: gone, or started again and on which
  version. The answer reaches the caller before the process goes, because a
  closed connection with no reply cannot be told apart from a crash. An
  in-flight turn is cut, which is what the person typing it asked for.
- **A newer CLI will not quietly serve an older daemon.** One file is both, and
  replacing it on disk does not restart what is already running, so this is what
  an ordinary upgrade leaves behind. The policy half of the `env` payload is
  therefore always present, empty where there is none, and absence means only
  that the daemon predates it. `settings` and `exec` refuse such a daemon rather
  than producing a document that looks complete and lacks a permission rule;
  `env` continues, because routing is all it ever carried, and names the daemon
  it is talking to. `status` reports the version actually serving the socket and
  says so only when it differs from the binary that asked. The decision reads a
  capability rather than comparing version strings, which would force a policy
  about which differences matter and get it wrong for a patched build.
- **`exec`**, a launcher: it applies both halves and runs the command, so
  starting a client is one step. Nothing is written to disk — the policy rides
  inline on the client's own settings flag. It refuses before starting anything
  when the daemon is not answering, and when the forwarded arguments already
  carry `--settings`, because the client keeps only the last such flag and drops
  the first without a word.

### Changed

- **A refusal follows the grant, not the process.** The backend refusing a
  refresh used to latch a flag for the life of the daemon, so the re-login its
  own message asks for did not help until a restart — and a login through the
  CLI never reaches the daemon at all. The refused refresh token is what is
  remembered now: a different grant is tried, and that one is still never
  retried. `status.auth.dead` means the grant currently stored is the refused
  one, and answers itself again if the account is selected back.

- **Switching accounts reaches conversations already running.** A WebSocket
  conduit fixes its account when it dials and reuses that connection for the
  conversation's life, so a switch used to leave every live session billed to
  the account just moved off. They are dropped and dial again, each paying one
  full upload.

- **The model catalog says which account it belongs to.** It is fetched once, at
  startup, for the account selected then, and `status.catalog_stale`,
  `status.catalog_account`, and `models.stale` report when that is no longer the
  account serving turns. Nothing refetches it yet; what changed is that the
  answers stop presenting another account's plan as this one's.

- **An account can hold an API key instead of a subscription grant.**
  `login --key` reads a secret from stdin — never from an argument, which is
  visible to every process on the machine — and stores it under the name `--as`
  gives it; `accounts --use` moves between the two kinds exactly as it moves
  between accounts. A key has no refresh, no expiry, no account id and no plan,
  and nothing reports a plausible value in place of any of them.

  One resolver decides what authenticates a request, and every path that
  authenticates asks it: both transports, the catalog fetch, and the quota
  fetch. A grant sends its token, the originator identifying a subscription
  client, and the account id it is spending; a key sends its token and nothing
  else. A credential is refused against the other kind's endpoint before
  anything leaves, in a message naming both halves, rather than being answered
  upstream with something about an invalid token. `[upstream.key]` is where a
  key is spent; it has no socket, because that protocol belongs to the
  subscription backend, and a key account has no quota to report, because that
  figure is a subscription entitlement.

  A key request is never compressed: zstd on a request body is measured against
  the subscription backend and nowhere else, and the key endpoint parses the
  compressed bytes as JSON and rejects them.

  A login through the CLI tells a running daemon to hand over, so what a switch
  carries with it — the conversations bound to the previous account, its quota,
  its model list — moves too rather than leaving a live conversation dialing an
  endpoint that now refuses it.

  Proven end to end against the replay server, which is what this suite can
  hold. Nothing about a real key endpoint has been confirmed — `docs/roadmap.md`
  §L carries what only a live one can settle.

- **`doctor --live` refuses when it cannot authenticate, instead of reporting
  the backend as broken.** With no credential — or with a key selected, whose
  probe path held a grant's token source — it answered with the whole matrix,
  every row failed, under a header saying the backend answered and was billed.
  Nothing had been sent. It now resolves the credential first and says only
  that, and probes the endpoint the account's kind belongs to.

- **`accounts --rename FROM TO` changes what an account is called.** A login
  carrying no `--as` names the account by the id the backend knows it by, which
  is a UUID nobody wants to type at `--use`. Renaming moves that name and
  nothing else: the grant, the account id, and which account serves turns all
  stay where they were, and a name another account already holds is refused.

- **`accounts --forget NAME` drops an account from the CLI.** `disconnect` had
  been on the control socket since v0.1 with nothing in the CLI that called it,
  so the only way to undo a login was to delete the credential file by hand. The
  name is required and cannot be combined with `--use`: an account is gone once
  the command returns. The answer says which one went and which one serves turns
  afterwards.

- **A credential write that lost a race is redone rather than lost.** Every
  write rewrites the whole file, so the CLI's `login` landing while the daemon
  persisted a refresh could discard an entire account. A write that finds the
  file changed since it read starts over. The window is narrowed rather than
  closed — the comparison and the replacement are two operations — and what
  would close it is a filesystem lock.

- **`login` states the label actually in force.** A second call while a flow is
  running joins it, and now says which name that flow will give the account
  rather than echoing the one it was handed.

## [0.2.1]

### Fixed

- **A client interrupt no longer bricks the conversation.** An abandoned turn
  drops its WebSocket connection instead of parking it, but the session still
  remembered the last response id — so the next turn opened a fresh connection
  and sent a delta naming a response that connection had never seen. The
  backend refuses that with `400 Invalid previous_response_id`, and because the
  refusal ends the turn cleanly, the refusing connection was parked and every
  following delta repeated it: the session never healed on its own. A delta is
  now planned only for the connection that produced the response it continues;
  every other case is a full send.

## [0.2.0]

A daemon a front-end can drive. Every capability below was reachable only by
restarting with a hand-edited configuration file, or not reachable at all.

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
