# API contract

What codex-cc-proxy exposes, and what callers may rely on.
[`proxy-behavior.md`](proxy-behavior.md) is the companion spec for how it
behaves internally.

Four surfaces: the HTTP ingress Claude Code talks to, the command line, the
control socket, and the configuration file. The ingress shape is fixed by the
Anthropic Messages API and is not ours to change. The other three are ours, and
the stability rules in §6 apply to them.

---

## 1. Ingress

The daemon binds `127.0.0.1` and refuses any other address. It performs no
authentication: every caller reaching the socket is already a local process
running as the user.

`ANTHROPIC_AUTH_TOKEN` must be set for Claude Code's own sake. Its value is
ignored.

| Endpoint | Purpose |
|---|---|
| `POST /v1/messages` | The only endpoint carrying real load. Streams SSE in both directions. |
| `POST /v1/messages/count_tokens` | Pre-flight sizing. Returns an estimate. |
| `GET /v1/models` | The mapped models, in the Anthropic list shape — `{"data": [{"id", "display_name", "type": "model"}]}`. Ids are the upstream model ids the tiers map to. |

`run` fails immediately if the port is already bound, naming the conflict, rather
than retrying or selecting another port. A second daemon on a different port
would be silently unused by a client already configured for the first.

### 1.1 Errors

Every failure — including transport and credential failures — returns an
Anthropic-shaped body:

```json
{ "type": "error", "error": { "type": "...", "message": "..." } }
```

| Condition | Type | Status |
|---|---|---|
| Quota exhausted | `rate_limit_error` | 429 |
| Upstream overload or 5xx | `overloaded_error` | 529 |
| Upstream judged the request invalid | `invalid_request_error` | 400 |
| Upstream rejection, otherwise | `api_error` | upstream status |
| Credentials invalid or absent | `authentication_error` | 401 |
| Credentials transiently unavailable | `overloaded_error` | 529 |
| Request exceeds the model's window | `invalid_request_error` | 400 |
| Malformed request body | `invalid_request_error` | 400 |
| Unknown endpoint | `not_found_error` | 404 |

`retry-after` is forwarded when upstream supplies it.

Transient conditions surface as retryable so Claude Code's own backoff drives
them. Terminal conditions surface as terminal. The proxy does not build a second
retry loop on top of the client's.

An error arising mid-stream is emitted as an SSE `error` frame rather than
changing an already-sent status.

---

## 2. Command line

```
codex-cc-proxy run        start the daemon
codex-cc-proxy login      authenticate
codex-cc-proxy status     connection, tier mapping, model catalog
codex-cc-proxy models     available models
codex-cc-proxy env        environment for Claude Code
codex-cc-proxy doctor     probe live backend capabilities
codex-cc-proxy usage      what quota is left
codex-cc-proxy statusline wrap a status-line script, adding that quota
codex-cc-proxy record     capture exchanges as fixtures
```

Every verb except `run` and `login` operates through the control socket (§3)
against a running daemon.

`login` runs in the CLI **and** in the daemon (§3), and the two are alternatives
rather than a duplication. The CLI's exists because the daemon need not be
running to authenticate: requiring it would mean starting a daemon that cannot
serve a request in order to obtain the credentials it needs to serve one. The
daemon's exists because a front-end that is not a terminal has no other way to
start the flow. Both bind the same fixed callback port, so only one can be in
flight; the second to ask is told which is holding it.

**The authorization URL is printed, never opened.** Opening it would hand the
authorization to whichever account the default browser happens to be signed
into, and that is a choice this command has no basis for making: the grant it
produces is the one every later request spends. The printed URL says so, and
names a private window as the way to pick a different account. It also means an
environment with no browser at all is not a special case.

`doctor` runs the capability probes and prints a matrix. Against the fixture
corpus — the default — it contacts nothing and costs nothing. `--live` answers
the same probes from the real backend instead, one turn each, and spends real
inference quota; it maps the corpus's model ids through the configured tiers,
so what it reports is the mapping in the configuration file rather than a
notional one.

A live run applies every check except the ones that only mean something against
a recording. The corpus can assert the exact URL a search returned because the
corpus wrote it; a backend answers with whatever it answers, and failing a
working capability on that basis teaches whoever reads the matrix to discount
it. Those checks are marked in the probe table and skipped live.

The matrix always states which it was. One built from replayed fixtures that
reads like one built from a live backend is exactly the plausible-looking
output the probes exist to prevent. A probe that could not run is reported as
skipped and never counted as a pass: a probe that established nothing while
reporting success is the same failure in miniature.

`--probe <name>` runs one at a time, and naming an unknown one lists the
known ones.

The corpus resolves in one of three ways, and the matrix names which. `--fixtures
<dir>` is answered from that directory and never from anywhere else — a
recording just captured by `record` must be what a run against it sees, not a
copy compiled in months earlier, so a named directory missing a fixture skips
the probe rather than falling back. With no `--fixtures`, a `fixtures/`
directory in the working directory wins if there is one, and otherwise the
corpus compiled into the binary answers. That last case is the one an installed
binary is in: `doctor` has to establish something on a first run, and a run that
skipped all eight probes for want of a checkout would establish nothing at the
moment it is most likely to be run.

`usage` reports the account's quota as of the last turn. It costs nothing to
ask: the backend opens every stream with a snapshot, before it says anything
about the response, so the figure rides along with a turn already being made and
is never polled. Before any turn has been made it says so rather than answering
with zeroes. `--json` emits the snapshot as it stands, for a status line.

The same snapshot is also put on the response as `anthropic-ratelimit-unified-*`
headers, which are the names this client's own code parses a quota from.
**Measured: that is not enough to make it appear in the status line.** A stub
endpoint setting those headers, with nothing else changed, left `rate_limits`
absent from the status-line payload.

The reason is now known rather than inferred. The client does parse those header
names, but the status-line payload is gated on a separate flag, which its own
schema describes as false "when plan rate limits do not apply (API key, Bedrock,
Vertex, or missing profile scope)". Pointing the client at a proxy means setting
`ANTHROPIC_AUTH_TOKEN`, which is the API-key path by definition, so `rate_limits`
is null there no matter what any header says. §2.1 is the only route, and the
headers are emitted because they are the accurate wire form of a figure the
response really carries — they do still feed the client's retry banner on a
quota 429.

### 2.1 `statusline`

The status line is a script the user supplies, and the client hands it a JSON
payload on stdin. `statusline` wraps that script: it reads the payload, merges
in the quota, and passes it on. A script written against the client's own shape
keeps working unchanged and gains a figure it could not otherwise have.

```json
{ "statusLine": { "type": "command",
                  "command": "codex-cc-proxy statusline -- ~/.claude/my-statusline.sh" } }
```

The merged payload gains `rate_limits.five_hour` and `rate_limits.seven_day`
where a window genuinely is one of those, in the fields a script already reads —
plus `rate_limits.windows`, which carries every window the backend reported with
its real length. A script wanting a window the client has no name for reads that.

Omit the command to print the merged payload instead, for a script that would
rather pipe it. The wrapped command's exit status becomes this command's.

**It never breaks the status line.** A daemon that is not running, a socket that
does not answer, a payload that will not parse: each passes through unchanged. A
status line renders constantly, and one that breaks is worse than one missing a
figure.

**And it never merges another session's quota.** A status line is configured
once and renders for every session the client runs, including sessions pointed
at their own provider rather than at this proxy — and the daemon answers `usage`
whenever it is up. So the merge is conditional on the model: `usage` reports
the ids this daemon serves, and a payload naming something else is passed
through untouched. That is what makes the wrapper safe to leave configured
permanently while switching back and forth.

The ids are the configured tiers plus every id a turn has actually been made
against, because a client that names a model itself passes that id straight
through and no tier would recognize it. **An unanswerable question merges**: a
snapshot that names no models, or a payload that names none, leaves nothing to
compare, and withholding the figure there would take it from every session that
has it today to prevent a case that may not be happening.

Where headers do apply, only a window that genuinely matches one gets one. Those
headers name two fixed windows, five hours and seven days, and the backend's
windows are not fixed: it has reported a five-hour window in the past, does not
currently, and may again. Windows are matched to header slots by duration, and
one matching neither is reported by `usage` — where it can state its real length
— rather than announced as a window it is not.

`record` has two modes, and the distinction matters because only one of them
costs anything:

- **ingress** captures what Claude Code sends to the proxy. It needs a working
  client and no upstream credentials at all, since the exchange is recorded
  before translation.
- **upstream** captures the whole exchange: the client's request, untranslated,
  paired with the stream the backend answered it with. It needs credentials and
  spends quota, because the turn it records is a real one. Every turn through a
  daemon started this way is captured, not only the failing ones — a fixture is
  made from an exchange that worked.

Both halves are needed to replay one. The request cannot be inferred from the
stream, which is why the capture holds the client's request rather than the
translated one: a capture of the translated request could not be replayed
through the translation it had already been through.

Both write to the same fixture format, so a test replays either without knowing
which mode produced it.

Captures are written beside the configuration, `0600`, and the most recent
twenty are kept. They hold conversation content — the system prompt, the
messages, and whatever the tools read.

Logging is controlled by `RUST_LOG`. Credentials never appear at any level.

### 2.2 `env`

Emits the configuration Claude Code needs, as shell exports or as a settings
fragment:

```
ANTHROPIC_BASE_URL=http://127.0.0.1:8787
ANTHROPIC_AUTH_TOKEN=unused
ANTHROPIC_DEFAULT_OPUS_MODEL=<mapped>
ANTHROPIC_DEFAULT_SONNET_MODEL=<mapped>
ANTHROPIC_DEFAULT_HAIKU_MODEL=<mapped>
ANTHROPIC_DEFAULT_FABLE_MODEL=<mapped>
CLAUDE_CODE_MAX_CONTEXT_TOKENS=<effective window>
CLAUDE_CODE_AUTO_COMPACT_WINDOW=<effective window>
CLAUDE_CODE_DISABLE_1M_CONTEXT=1
```

The two window variables appear only when the catalog knows the window, and
carry the smallest across the mapped tiers. The client will warn that its
200,000 limit is not enforced; that is expected, because the real window is
larger and using it is the point.

All four tier variables are always emitted. `WebFetch` runs on the haiku tier, so
an unmapped haiku breaks it in a way that looks unrelated to tier mapping.

`CLAUDE_CODE_DISABLE_1M_CONTEXT` is not inert: without it this client appends
`[1m]` to an unrecognized id and assumes a million tokens — see
`proxy-behavior.md` §7.2.

---

## 3. Control socket

A Unix domain socket, or a named pipe on Windows, carrying JSON-RPC:

| Method | Returns | v0.1 |
|---|---|---|
| `status` | connection state, whether the grant has been **refused**, plan and which source reported it, the tier mapping and the effort ceiling, any mapped model the catalog withholds, whether the catalog was authoritative | yes |
| `disconnect` | clears credentials | yes |
| `models` | catalog, and whether it is the fallback list | yes |
| `tiers.get` | tier mapping | yes |
| `usage` | quota snapshot as of the last turn, or that no turn has been made, plus `models` — the ids this daemon serves | yes |
| `usage.refresh` | asks the backend for a figure now, for a front-end with nothing to show on a daemon that has served no turn | yes |
| `env` | the §2.2 block | yes |
| `record.start` / `record.stop` | fixture capture | yes — `{"mode": "ingress"}` by default, `"upstream"` must be named because it bills every turn that follows |
| `login` | authorization URL, then completion in the background; `status` reports when it landed | yes |
| `login.cancel` | abandons a flow and releases the callback port | yes |
| `tiers.set` | tier mapping, validated against the catalog and in effect until the daemon stops | yes |
| `effort.set` | the effort ceiling, or `null` to remove it; in effect until the daemon stops | yes |
| `doctor` | probe results | no — `doctor` runs in the CLI, which is where `--live` can be given credentials without a daemon already holding them |

`auth.dead` is the one that is easy to miss: a refused grant leaves `connected`
true, because the credential file is still there and still readable, while every
turn after it fails with an authentication error. Without that field a front-end
shows a healthy provider and no reason to look.

**A persisted change is written before it is applied.** A write that fails
leaves the daemon exactly as it was, so the error the caller receives is the
whole story — applying first would leave it running a policy nobody chose,
reported as a failure, and gone at the next restart.

**`tiers.set` and `effort.set` write the configuration file only when asked** —
`{"persist": true}`. A front-end changing a mapping to try something is not the
same as an operator changing what this daemon is, and only the caller knows
which it is doing. Without it the change lasts until the daemon stops, and every
answer says which it was rather than leaving it to be discovered.

A persisted change is a **text edit**, not a re-serialization. The file is a
document whose comments explain why each key is what it is, and most of them
exist because the obvious value is wrong in a way that does not fail loudly;
rewriting it from the parsed configuration would discard all of that, and the
loss would be invisible — the file would still parse, still work, and never again
explain itself. One value on one line changes; everything else survives byte for
byte. The file is read fresh at write time, so an edit the operator made since
startup is not overwritten to persist an unrelated one.

`tiers.set` is **partial**: naming one tier changes that tier. Treating the
argument as the whole mapping would let a caller that knows about one tier
silently unset the three it did not mention. Every set is validated against the
catalog exactly as startup validates it — that check is why this daemon owns the
mapping rather than a front-end, since it is the side holding the catalog.

**`login` runs in the daemon.** It answers with the authorization URL and
returns; the flow completes in the background and `status` is what reports that
it did. A control call that blocked until the operator finished in a browser
would hold the socket for minutes and give a front-end nothing to render. There
is one fixed callback port, so a second caller **joins** the first and is told so
rather than being handed a URL whose callback would then be rejected as not
matching. An abandoned flow releases the port after ten minutes.

**`usage.refresh` is not the primary path and does not replace it.** The backend
volunteers a snapshot at the head of every stream; that one is free, rides a turn
already being made, and is what `usage` reports. This exists for the case that
path cannot cover — a front-end with a figure to show on a daemon that has served
no turn yet — and its answer is recorded where the stream path records its own,
so everything reading a quota reads one value.

The names are reserved whether or not v0.1 answers them: they are semver-bound
(§6), and a method that appears later must mean what its name said all along. A
reserved method reports that it is unimplemented rather than failing as though
it were unknown.

The daemon holds authoritative state and any front-end is a client of this
interface. The CLI has no privileged path of its own; a second front-end needs no
new daemon work.

---

## 4. Configuration

TOML in the platform configuration directory — `$CODEX_CC_PROXY_HOME`, else
`$XDG_CONFIG_HOME/codex-cc-proxy`, else `~/.config/codex-cc-proxy`. Credentials
are never stored here.

```toml
port = 8787

# Optional. A ceiling on reasoning effort, whatever the client asks for.
effort = "low"

[tiers]
opus   = "..."
sonnet = "..."
haiku  = "..."
fable  = "..."

[transport]
websocket   = true
compression = true

[instructions]
identity       = true
working_budget = true
append         = "..."

[upstream]
client_version           = "2.0.0"
effective_window_percent = 95.0
endpoint                 = "https://..."
websocket                = "wss://..."
catalog                  = "https://..."
```

`[upstream]` is entirely optional; every key defaults to what ships. It exists so
a pinned binary can be repointed rather than rebuilt, and because two of the keys
fail in ways nothing else can diagnose.

`client_version` is what the proxy reports when asking for the model list — not
this crate's version. The backend filters the list by it, and a version below
every model's minimum returns an **empty list rather than an error**, which reads
exactly like an account with no models. Startup says so by name when the catalog
comes back empty.

`effective_window_percent` is the share of a context window left usable once
instructions, tool overhead, and output are accounted for, applied where the
catalog states no share of its own. It is the figure the client is told, so it
decides when compaction fires: lower compacts sooner and wastes window, higher
risks a turn refused for length. A value outside `(0, 100]` is refused at
startup rather than clamped.

**Every key has a default, and the file itself is optional.** A missing
configuration is a first run, not a failure: the daemon logs where the file would
go and starts on the defaults. A file that is present but unparseable is still an
error — falling back there would run a daemon that ignores what the operator
wrote.

The four tiers default to the mapping above. An omitted tier takes its default; a
tier written blank is refused, because an omission accepts the shipped answer
while a blank is a mistake. Each mapped model is validated against the live
catalog when one is reachable. That validation happens once, at startup: the
catalog is not refetched, so a mapping cannot go stale while the daemon runs.

`effort` caps reasoning effort on every request, whatever the client asks for —
one of `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`
(`ultracode` is the client's name for `ultra` and is accepted as one). It is a
ceiling, not a fixed value, and is capped again by what the model accepts. Omit
it for no ceiling; omitting it does not mean zero effort, it means the backend's
own default. An unrecognized value is refused at startup rather than ignored.

It is a top-level key and must sit above the tables — see the note on
misplacement below.

`[instructions]` is what the proxy puts around the client's system prompt
(`proxy-behavior.md` §2.1). `identity` leads with one line naming the model that
is actually answering, and is **on by default** — a model told it is a different
product is being given a false premise on every turn, which is not a neutral
default to pick on an operator's behalf. `append` is operator text placed after
the system prompt, where an instruction has to be to take precedence over it.

`working_budget` is a short block asking the model to read the smallest slice
that answers the question rather than whole files, and to act once a read is
enough. **On by default**, deliberately: the conversation is replayed upstream on
every turn and echoed back three times, so broad reading spends the window fast.
It sits after the client's prompt, which it exists to overrule on this point, and
before `append`, which an operator wrote on purpose and which therefore outranks
it.

All three must be constant for a conversation. Text that changes between turns
changes `instructions`, and that costs every delta and every cache hit.

The file is read once, at startup: **v0.1 does not watch it**, and a change
takes effect on the next `run`. `--port` is the only override outside the file.
The estimator backend is likewise not a configuration key in v0.1 — the
tokenizer is a compile-time feature (`--features tokenizer`), because which
estimator wins is a measurement rather than an operator's choice
(`proxy-behavior.md` §6.3).

**An unrecognized key is refused, not ignored.** Tolerating one looks forgiving
and is not: in TOML a bare key written after a table header belongs to that
table, so `effort` placed below `[tiers]` is `tiers.effort`. Ignored quietly,
the operator believes they capped their spending while every request runs at
the backend's default. Top-level keys therefore sit above the tables, and the
error says so when they do not.

---

## 5. Limitations

Stated because each is permanent under the current design, not because they are
pending work.

- **The context percentage Claude Code displays is wrong.** It is computed
  client-side against an assumed window and cannot be corrected from the proxy.
  Token counts are exact; the percentage is not.
- **Sessions compact earlier than necessary**, for the same reason. The assumed
  window sits below the real one, which is the safe direction.
- **`cache_creation_input_tokens` is always zero.** No upstream write event
  exists to report.
- **`count_tokens` is an estimate.** It is answered by the conversation's own
  estimator, so it is uncalibrated before that conversation's first completed
  request and improves after it. It is never exact: there is no upstream
  token-counting endpoint to be exact against.
- **`cache_control` and `thinking` blocks are dropped** on the request path.
  Reasoning is reconstructed on responses from summary events.
- **Image URLs are not prefetched** and resolve only if the backend can reach
  them.
- **The catalog fallback list is fixed** and needs updating if models are renamed
  or retired while the live fetch is unavailable. Its entries carry no context
  window, so the window guard does not fire for a model the fallback named.

- **Claude Code never reaches the `input_file` path.** It rasterises PDFs into
  image blocks, so documents from that client reach the model as images. The
  `document` translation is for a client that sends one, and the backend does
  accept it — measured by posting a `document` block directly, which returned a
  code that existed only inside the PDF.

- **Compression saves bytes and no tokens.** HTTP bodies are zstd-compressed;
  WebSocket frames use `permessage-deflate`, negotiated during the upgrade.
  Roughly two thirds off in both directions, and the inbound half is the larger
  one — the backend echoes the whole request back three times per turn. Quota is
  unaffected.

- **A web search that produced no citations reports the pages the model opened**,
  which carry a URL but no title. That is worse than a real citation and better
  than an empty result, which the client reads as "nothing found".

- **What a matrix proves depends on what answered it.** A replayed run
  establishes that the proxy does its half; only `--live` establishes that the
  backend does its own. `doctor` states which on the face of its output, and
  `roadmap.md` §L records what has been settled against a live backend and what
  has not.

---

## 6. Stability

The CLI verb set, the control-socket method names, the configuration keys, and
the error-type vocabulary are semver-bound. A shipped name is never repurposed or
removed within a major version; only new ones are added.

The ingress shape is not ours — it tracks the Anthropic Messages API, and
changes there are not breaking changes in this project's versioning.

---

## 7. Posture

The upstream endpoint is not a published or supported API. It may change or be
withdrawn without notice, and using a subscription this way is a decision each
operator makes for themselves. There is no version of this project that avoids
that, so it is stated rather than omitted.

This project is not affiliated with, endorsed by, or sponsored by Anthropic or
OpenAI. All trademarks belong to their owners.

No telemetry is collected or transmitted. Credentials never appear in process
arguments or logs. Configuration and credential files are created with
restrictive permissions.
