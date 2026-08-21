# Changelog

All notable changes to this project are recorded here. This project follows
[semantic versioning](https://semver.org). The semver-bound surfaces are listed
in [`docs/api.md`](docs/api.md) §6.

## [Unreleased]

### Changed

- **Every `accounts` row names its provider.** The listing named one only where
  it was not `codex`, so a store holding both providers printed three rows whose
  provider had to be inferred from the one row that had it. It is a column every
  row fills now, whatever the credential kind, alongside the address or `key`
  that already told two rows apart. A payload that carries no provider at all —
  a daemon older than providers — still prints none, because filling that in
  would be inventing it.

### Added

- **The launch settings disable the client's commit attribution.** A session
  started through `proxenos exec` or configured from `proxenos env` now receives
  `attribution.commit: ""`, the client's own empty-template form for appending
  no trailer to a commit it makes. Which model served a turn is not a fact a
  commit message is the place to record, so it ships on both the translating and
  the all-relay path. `client.disable_commit_attribution = false` puts the
  client's own default back, and then the `attribution` object is absent from
  the document rather than present and empty.

- **A ninth `doctor` probe, `env-contract`, holds the launch surface to its
  contract.** `ENABLE_TOOL_SEARCH` and `CLAUDE_CODE_DISABLE_1M_CONTEXT` are both
  load-bearing and both fail silently — the first hands the deferred tool set
  back into the context, the second lets the client invent a million-token
  window for an id it cannot recognize — and nothing asserted that `env` and
  `exec` still emitted them. The probe renders the environment for a translating
  mapping and an all-relay one and asserts on what was rendered, so a launch that
  emits nothing cannot pass it. It replays nothing, contacts no backend on either
  mode, and its row is marked as proxy-answered under `--live` alongside
  `count-tokens`.

### Changed

- **A key re-store that would change an account's provider is refused.** A key
  stored over a key was a silent replace, including across providers, so
  `login --setup-token --as api` turned a first-provider key account into a
  second-provider one and the old key was gone with nothing said. The refusal
  names the account, the provider it holds, and the `accounts --forget NAME`
  that clears the way. Same-provider rotation is unchanged, and so is the
  existing refusal to store a key over a grant.

- **The control socket lives inside `PROXENOS_HOME` when one is set.** *Path
  change.* It was `$TMPDIR/proxenos.sock` regardless of the home, so a CLI or a
  daemon isolated into a temporary home still reached the operator's real
  daemon whenever the two shared a `TMPDIR` — and every login path ends in
  `accounts.select` over that socket, so an isolated login could switch the
  account the real daemon serves. With no home named the path is unchanged. One
  derivation answers for the daemon's bind and every CLI call, and a derived
  path over the platform's `sun_path` cap is now refused by name at both ends
  rather than leaving a daemon that serves turns and answers no verb.

- **A login no longer takes over what serves turns.** *Behaviour change to a
  v0.6.0 surface.* `login` stores a credential and selects it only where
  nothing is already serving; every login after the first leaves the selection
  alone, names the account still serving, and prints the `accounts --use NAME`
  that would switch. Storing a credential and choosing what serves turns are
  two decisions, and making them one moved every turn onto a newly added
  account with nothing said about it. The rule holds on all three paths — an
  authorization, `--key`, and `--setup-token` — and in the daemon's `login` as
  well as the CLI's; `accounts --use` is unchanged and is the verb that
  switches. An operator who relied on a second login switching for them needs
  that verb now.

- **`status` says when the tier mapping is inert.** A serving account on the
  second provider relays every id it authenticates verbatim (§9.1), so the four
  tier rows decided nothing while reading as though they decided everything.
  Unpinned rows are now marked inert and a `routing` line names the provider
  the ids relay to; a pinned tier names its own account and stays live, so a
  split mapping stays accurate row by row. A rendering change only — no field
  of the `status` payload moved.

- **The control socket's `tiers.get` is now `tiers`.** Every other read on the
  socket is a bare noun, and each coexists with namespaced writers under the
  same noun; a lone `.get` bought no capability. Renamed with no alias while
  nothing outside this repository's own CLI speaks the socket — `docs/api.md`
  §6 now names the nineteen methods that are bound from v0.7.0 on, which is
  when that freedom ends.

### Added

- **`doctor` now says what a run did not touch.** A failed row prints the
  probe's rationale — what breaks silently without it — while passing rows stay
  one line. Under `--live` the `count-tokens` row is marked as answered by the
  proxy: the live header says the backend answered and was billed, which is true
  of every other row and false of that one. And one line under the matrix names
  the account the run spent and the paths it left alone, so eight green rows
  cannot be read as coverage of the WebSocket transport or the relay when
  neither ran.

- **A probe for the relay path (§9).** `doctor` built its own state with no
  relay at all, so nothing in the suite drove the branch that forwards a turn
  instead of translating it — the one path whose entire claim is that the bytes
  are not touched. The new probe's marker sits inside a field the proxy has no
  type for, so a body round-tripped through its own types fails it. Replay-only:
  driving it live needs the account serving turns switched to the second
  provider for the length of the run, which is not wired.

- **`proxenos usage` now reports the second provider's accounts too.** That
  provider states rate-limit headroom in `anthropic-ratelimit-unified-*` headers
  on the response to every turn, and for a subscription credential it is the
  only place one is stated — its usage endpoint refuses that credential for want
  of a scope, so there is nothing for `usage.refresh` to ask. The headers are
  read off each relayed response and filed under the account that made the turn,
  which costs nothing: the figure rides a turn already being made, and no path
  here polls. A plan name is absent rather than guessed, and an account that has
  relayed no turn yet says a turn supplies its figure instead of claiming the
  provider reports none.

- **`proxenos login --setup-token`: a guided way to store a subscription
  token.** The same stored credential `--key --provider anthropic` produces,
  reached without the pipe-and-ctrl-d workflow it required. It says where the
  token comes from (`claude setup-token`), reads it from a hidden prompt where
  stdin is a terminal, asks what to call the account when `--as` did not, and
  refuses anything not beginning with `sk-ant-oat` before the store is
  touched — with no override flag, because a credential of the wrong kind
  stores cleanly and fails later naming the account rather than the paste. A
  non-terminal stdin still reads the token from the pipe, so scripted use does
  not regress.

## [0.6.0]

The second provider, end to end — and confirmed live: relayed turns round-trip
against the real endpoint with a substituted subscription bearer, plain and
streaming both. `docs/roadmap.md` §L records what settling that falsified.

### Added

- **A tier may pin another account, behind explicit consent.** `haiku =
  { account = "spare", model = "..." }` routes that tier's turns to another
  stored account — and they are *served as* that account: its credential
  authenticates every upstream request, a token refresh is written back to the
  entry it was read from rather than to whichever account is selected, and a
  pooled socket opened as one account is never reused for another. A pin naming
  an account the store does not hold refuses the turn by name; there is no
  fallback to the serving account, because that spends the wrong subscription's
  quota invisibly. Absent the `cross_account_tiers` consent key, a pinned entry
  refuses the daemon at startup and `tiers.set` at write time, naming the key;
  the `cross_account_tiers.set` socket method grants or withdraws it, always
  persisted, with withdrawal refused while a pin is in force.
  `docs/proxy-behavior.md` §7.1.
- **An account states its provider.** Each stored account carries which
  provider's endpoints its credential is spent against; `accounts` and `status`
  name it where it is not the default. Files written before there was a second
  provider keep their exact shape.
- **A relay-bound launch is handed no window it cannot know.** For a mapping
  served entirely by the relay, `env` and `exec` omit
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, and
  `CLAUDE_CODE_DISABLE_1M_CONTEXT`: the client recognizes those ids natively,
  an override could only replace a real window with an invented one, and the
  flag would strip the `context-1m` beta from the wire — an entitlement the
  account may actually hold. A mixed mapping states no window and keeps the
  flag; `docs/proxy-behavior.md` §7.2 states both costs.
- **`record ingress` keeps the request headers**, in arrival order, with
  credential-bearing values (`authorization`, `x-api-key`, `cookie`,
  `proxy-authorization`) redacted by name. A header's presence is the datum;
  its value never lands in a capture.
- **The connector opt-out reaches the environment.**
  `client.disable_connectors` now also exports
  `ENABLE_CLAUDEAI_MCP_SERVERS=false` — the client's own documented opt-out for
  the claude.ai-hosted servers — so a launch configured by exports alone honours
  it.
- **A quota figure per account, not per daemon.** A pinned tier's turns spend
  the account it names, so two accounts can serve one session and a single
  latest snapshot reported whichever made the most recent turn. Each figure is
  now held under the account that earned it, and `usage` keeps its shape — the
  serving account's figure stays where a reader already looks — while gaining
  `accounts`: one entry per stored account with its own windows, whether it is
  serving, how the figure was come by (riding a turn, or asked for over the
  socket), and the moment it was taken. `docs/proxy-behavior.md` §8.3.
- **An account with no figure says so, and says why.** No turn made as it yet, a
  key holding no subscription entitlement, or a provider whose quota endpoint
  is an open question. Never a zero, and never another account's figure.

- **A turn for the second provider is relayed verbatim.** A key stored with
  `login --key --provider anthropic` and pinned to a tier serves that tier's
  turns through a passthrough: the body is forwarded byte for byte, the reply
  is streamed back byte for byte, and the only thing that changes on the way is
  the header set — the bearer becomes the account's credential, any `x-api-key`
  the caller brought is dropped, and `anthropic-version` and `anthropic-beta`
  pass through as the client sent them. `docs/proxy-behavior.md` §9.
- **`login --key --provider`** states which provider's endpoints a stored key is
  spent against. `codex` by default. Only meaningful with `--key`, and refused
  rather than ignored without it.
- **`[upstream.anthropic]`** in the configuration, with an `endpoint` key.
- **A relayed turn is observable like a translating one.** `record ingress`
  captures it, holding its request as the exact bytes that were relayed rather
  than a re-encoding that would drop every field this proxy's own types do not
  model, with header values redacted by name as everywhere else. Its model id
  joins the served list `usage` states, so a status line still recognizes a
  session whose tier was remapped while it was running.
  `docs/proxy-behavior.md` §9.4.
- **`exec` upgrades a plain `--model` id to its long-context variant.** For a
  relay-serving daemon, an id whose `[1m]` variant the curated list offers is
  rewritten to it — the client's own long-context selector — and the rewrite is
  named on stderr. An id already carrying the marker, an alias, another
  program's `--model`, and every translating launch are forwarded as typed.
- **A relay-serving daemon answers `models` from a curated list**, windows
  included: the second provider's own list endpoint names ids but states no
  windows, so the figures are curated into the binary and every answer says
  `curated` rather than presenting the list as a fetch. It is a menu for
  reading, never a list to refuse by — no mapping is validated against it, and
  `status` reports the catalog as curated instead of unvalidated.
- **Launches carry `remoteControlAtStartup: false`.** A session started through
  a local proxy is a local decision. On by default beside the other client
  policy, switchable with `[client] disable_remote_control = false`.
- **Every launch forces the client's tool search on.** The client disables
  deferred tool loading the moment its base URL is not a first-party host, and
  an MCP set measured at ~101k tokens loaded up front defers to zero with
  `ENABLE_TOOL_SEARCH=true` — verified live on both paths: the relay forwards
  `defer_loading` and `tool_reference` verbatim to a backend that runs the
  search itself, and the translating path carries client-driven discovery
  (`docs/proxy-behavior.md` §2.5).

### Changed

- **A select no longer discards a quota figure, and a removal now does.** With
  every figure held under the account that earned it, a select changes which
  account `usage` is about and invalidates nothing; forgetting an account drops
  its figure whether or not it was the one serving, because that entitlement
  belongs to a subscription this daemon can no longer spend.
- **Routing on the relay path is by model id, and one id may be claimed by at
  most one account.** A tier that pins nobody names the account serving turns,
  so selecting a key for this provider relays every turn rather than sending
  them to the other provider's endpoint. Two mappings naming one id and two different accounts
  refuse the turn, naming both. Picking one would spend a subscription nobody
  pointed at that turn and say nothing.
- **`AccountStore::add_key` takes the provider.** A required parameter rather
  than a default: the two providers' endpoints refuse each other's credentials,
  and a key that silently claimed the wrong one fails as an authentication
  error naming the credential rather than the destination.
- **An id no mapping names follows the account that would authenticate it**:
  relayed when that account is on the second provider — the credential travels
  only to its own provider's endpoint, and that provider judges the id — and
  translated with the id passed through when it is on the first. A launch-time
  model override rides on this symmetrically, with no mapping edit. Crossing
  providers still takes a pointer (a pinned tier, or a changed selection),
  because serving an id from an account nobody named would spend a
  subscription nobody pointed at the turn. `docs/proxy-behavior.md` §9.1.
- **The `claude-api` skill-deny default is resolved per launch.** Unset,
  `deny_skills` denies the skill only where a tier translates — the skill
  documents the second provider's API, the wrong reference for a translated
  session and the right one for a relayed session. A written list stays the
  operator's rule on either path, and `status` reports the list a launch would
  actually apply.
- **`docs/proxy-behavior.md` §4** no longer claims transports are
  interchangeable below the session. The choice of transport belongs to the
  provider: the relay is HTTP with SSE and nothing else. §9 is the relay; what
  was §9 (Testing) is §10.

### Fixed

- **A pinned mapping no longer refuses the daemon at startup, nor a switch.**
  `tiers.set` already excluded pinned entries from catalog validation, but the
  daemon's start and `accounts.select` still measured them against the serving
  account's menu — so a pinned mapping accepted over the socket was refused at
  the next start, silently until then. One function now holds the exclusions
  (pinned and relayed alike) at all three doors.
- **`record ingress` and `record upstream` honour `--port` and
  `PROXENOS_PORT`.** The variable was read by `run` and silently dropped by
  `record`, which assembled its daemon's arguments by hand.
- **The ingress imposes no size limit on a request body.** The body extractor's
  2 MB default refused a real client turn — a full system prompt and a large
  tool set — with a plain-text 413 the client read as retryable and looped on.
  The backend's own limit is the real one, and the daemon is loopback-only.
- **The request path's query string is relayed as sent.** Observed live: the
  client posts `/v1/messages?beta=true`, and the ingress never read the URI, so
  the relay posted to the bare endpoint. The query now follows the body's rule
  — forwarded exactly as sent, never invented. `docs/proxy-behavior.md` §9.2.

## [0.5.0]

The project is named for what it does. `codex-cc-proxy` named one provider on
one side of a daemon that is about to serve more than one; **proxenos** — the
ancient Greek office the word "proxy" descends from, a citizen representing a
foreign guest's interests in his own city — names the role and no provider.

### Changed

- **Everything the old name reached is renamed, with no aliases kept.** The
  repository, both crates (`proxenos`, `proxenos-core`), the binary, and the
  environment prefix: `CODEX_CC_PROXY_PORT` and `CODEX_CC_PROXY_HOME` are
  `PROXENOS_PORT` and `PROXENOS_HOME`. One rename on a minor bump under the
  pre-1.0 exception recorded in `docs/api.md` §6. Wire identity is untouched:
  the originator and user agent the upstream sees never carried the project
  name.
- **A store under the old name refuses loudly instead of vanishing.** A binary
  finding `CODEX_CC_PROXY_HOME` exported, or a default home written by the old
  binary with nothing at the new path, refuses to run and says exactly what to
  move where. Starting fresh with an empty store would have read as every
  credential having disappeared. Nothing is migrated automatically — an old
  daemon may still be running against that directory.

## [0.4.0]

The tier mapping stopped belonging to the daemon and started belonging to an
account, and the one socket method whose name did not match its neighbours was
renamed while nothing but this project's own CLI was there to notice.

### Added

- **A tier mapping belongs to an account.** A catalog is one account's menu, so
  one mapping is only ever right for the models every stored account has — and
  that intersection shrinks with each account added. Two subscriptions on
  different plans are offered different models; a key account beside a
  subscription need not overlap at all. `[accounts.<name>.tiers]` replaces the
  tiers it names and no others, and `effort` under `[accounts.<name>]` replaces
  the shared ceiling rather than being capped by it. The key is the name the
  store files the account under, because a key account has no id to be named by
  and the name is what every account verb takes.

  Three things follow, and each was a way for the mapping to be quietly wrong.
  A **switch re-resolves it**, and is refused by a mapping the target account's
  catalog cannot serve — the daemon stays where it was, catalog included, rather
  than serving an account whose every turn dies upstream saying nothing about
  tier mapping. A **rename takes the section with it**, headers only, every key
  and comment under them untouched. And a **persisted change is written where
  the value is read from**: a change written to the shared table while the
  serving account shadows it would be in force now and gone at the next start.
  `tiers.set` and `effort.set` also take `{"account": name}`, which writes that
  account's section; aimed at an account that is not serving, the change is
  written and not applied, not validated against the serving account's catalog,
  and without `persist` refused rather than answered as though it had done
  something. `effort.set` with `null` under an account removes that account's
  override and reports the shared ceiling that applies again, rather than
  reporting no ceiling and letting one come back at the next start.

### Changed

- **`disconnect` is now `accounts.forget` on the control socket, and its answer
  says `forgotten` rather than `disconnected`.** The old name shipped in v0.1,
  when there was one account and disconnecting from it was the whole idea. With
  a store of several, forgetting one is an account operation, and every other
  account operation is `accounts.<verb>`. This is a breaking change to a
  semver-bound surface, made rather than deferred because nothing outside this
  project's own CLI has ever spoken the socket, and the CLI and the daemon are
  one binary — the rename lands on both at once. `api.md` §6 now states when
  that stops being true. Nothing in the CLI changes: `accounts --forget NAME`
  is what it always was.

- **A credential directory that cannot lock says what to do about it.** Every
  write takes a lock beside the credential file, and a filesystem that cannot
  take one — a home on a network mount being the case that exists — now fails
  with the file named and `CODEX_CC_PROXY_HOME` named beside it, rather than
  with an error that reads as a bug in this program. Proceeding without the lock
  is deliberately not offered: it would report success while doing the thing the
  lock exists to stop.

## [0.3.1]

### Fixed

- **Two writers of the credential file no longer overlap at all.** A write
  reads the file, changes it, and replaces the whole thing, and v0.3.0 answered
  an overlap by starting over when the file had changed since it was read. That
  check cannot cover the gap between itself and the replacement: a writer
  landing there was copied over, silently, and what it lost was a whole account
  rather than a stale token. Every write now takes a lock the filesystem
  enforces and holds it across all three steps. The lock is a file of its own
  beside the credentials — the credential file itself is replaced by rename, so
  a lock on it would be a lock on an inode the next writer never opens — and
  the kernel drops it when the process goes, so a crash mid-write leaves
  nothing for the next run to wait on. The comparison stays, because the lock
  reaches only writers that take it and an older binary or a hand edit takes
  none.

## [0.3.0]

One credential file held one account and one kind of credential, and starting a
client meant evaluating a shell expression first. None of those three is true
now.

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

- **The model catalog follows the account.** It is fetched at startup for the
  account selected then, and asked for again when `accounts.select` or a
  `disconnect` that hands over changes who serves turns. That fetch is best
  effort: a failure keeps the list already in force, because a fetch that did
  not answer is not evidence that a model went away, and replacing a real list
  with the fallback would withdraw models the account has. So the list can
  still belong to somebody else, and `status.catalog_stale`,
  `status.catalog_account`, and `models.stale` say when it does rather than
  presenting another account's plan as this one's.

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
  hold. A real key endpoint has answered three times and settled less than that:
  it took the key at the turn endpoint, refused a compressed body there, and
  refused the same key at the model list. `docs/roadmap.md` §L carries what that
  leaves open.

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
