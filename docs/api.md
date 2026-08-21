# API contract

What proxenos exposes, and what callers may rely on.
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

A request to `POST /v1/messages` whose model id belongs to an account on the
second provider is **relayed** rather than translated: the body is forwarded
byte for byte and the reply is streamed back byte for byte, with the bearer
replaced by that account's credential. `proxy-behavior.md` §9 states the rule
and the header delta. Nothing about the endpoint changes — the same URL serves
both paths, and which one a turn takes is decided from the model id it carries.
This path is proven against fixtures and not yet confirmed against the second
provider's live endpoint (`proxy-behavior.md` §9, `roadmap.md` §L).

`run` fails immediately if the port is already bound, naming the conflict, rather
than retrying or selecting another port. A second daemon on a different port
would be silently unused by a client already configured for the first.

The ingress imposes **no size limit** on a request body. A real turn — a full
system prompt and a large tool set — runs past the extractor's 2 MB default, and
the backend's own limit is the real one. A 413 from the door is worse than a
large body: it is not an Anthropic error shape, so the client reads it as
retryable and loops on it, the turn never reaching the backend. The daemon is
loopback-only, so the only sender is the user's own client.

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
proxenos run        start the daemon (--detach: in the background)
proxenos login      authenticate (--as NAME labels it, --key reads one from stdin,
                    --provider names which provider that key is for,
                    --setup-token stores a subscription token, guided)
proxenos accounts   stored accounts (--use switches, --rename, --forget drops)
proxenos status     connection, tier mapping, model catalog
proxenos models     available models
proxenos env        environment for Claude Code, as shell exports
proxenos settings   the same configuration, as one settings document
proxenos exec       run a command with that configuration applied
proxenos stop       ask the running daemon to stop
proxenos doctor     probe backend capabilities (--live answers from the real one)
proxenos usage      what quota is left
proxenos statusline wrap a status-line script, adding that quota
proxenos record     capture exchanges as fixtures
```

Every verb except `run`, `login`, and `doctor` operates through the control
socket (§3) against a running daemon.

`login` **adds** an account; it never replaces the one already stored
(`proxy-behavior.md` §8.1). It selects the account it stored **only where
nothing is already serving turns** — a first login has nothing to displace, and
every login after it stores a credential and leaves the selection alone,
printing the account still serving and the `accounts --use NAME` that would
switch. Storing a credential and choosing what serves turns are two decisions,
and a login that made both moved every turn onto a newly added account without
saying so. The rule is the same on all three paths — an authorization, `--key`,
and `--setup-token` — and in the daemon's `login` (§3) as well as the CLI's:
which flag stored the credential is not a reason for it to mean something
different. `--key` stores a key read from **stdin**
instead of starting an authorization: no browser, and the secret never appears
in a command line. `--as NAME` is required with it, because a key carries no id
to be named by. `--provider` states which provider's endpoints the key is spent
against — `codex` by default, `anthropic` for a key that serves turns through
the relay (`proxy-behavior.md` §9). It is only meaningful with `--key`: an
authorization runs against one provider's server, so a grant has nothing to
choose, and naming a provider without `--key` is refused rather than ignored. `--as NAME` is what to call it locally, for
an operator holding more than one; without it the account id the grant carries
names it. `accounts` lists what is stored, marking the one serving turns, and
`--use NAME` switches to another. `--rename FROM TO` changes what an account is
called here, leaving its grant and the id the backend knows it by alone — a
login carrying no `--as` names the account by that id, and correcting it should
not cost an authorization. `--forget NAME` drops one, leaving the rest usable;
the name is required, because an account is gone once it returns. All of
them go through the socket, because the daemon holds the selection: a CLI that
edited the file directly would leave a running daemon serving the account it
read at startup.

`--setup-token` is a guided front door over the same stored credential `--key
--provider anthropic` produces — no new credential kind, and nothing about how
it is stored changes. What it adds is the part a person needs and a pipe does
not: it says to run `claude setup-token` in another terminal, reads the token
from a **hidden prompt** where stdin is a terminal, and refuses anything that
does not begin with `sk-ant-oat` before the store is touched — the stem, because the version digit belongs to the issuer and a real minted token begins `sk-ant-oat01-`. That refusal
exists because a credential of the wrong kind stores cleanly and fails later
naming the account rather than the paste that was wrong; there is no override
flag. `--as NAME` names the account, and without one the flow asks. Where stdin
is **not** a terminal the token is read from the pipe exactly as `--key` reads
it, so `proxenos login --setup-token --as NAME < token` keeps working and
scripted use does not regress — in that case `--as` is required, because there
is nobody to ask. It conflicts with `--key` and `--provider`, which describe a
credential it has already decided.

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

**The rendered `status` says what the next turn does, not only what is
configured.** Where the account serving turns is on the second provider, every
model id it authenticates relays verbatim and the tier mapping decides nothing
(`proxy-behavior.md` §9.1). The four tier rows printed unqualified read as
"your turns go to these models", which is the one thing they do not mean in
that state, so each unpinned row is marked inert and a `routing` line names the
provider the ids relay to. A pinned tier (§7.1) names its own account and stays
live either way, marked or not by the provider the selection happens to be on —
a split mapping renders accurately row by row. This is a rendering rule: the
`status` payload already carries `auth.provider` and the pins, and no field
changed.

A live run **resolves its credential before it probes anything**, and answers
with that refusal alone when it cannot. A matrix reporting seven capabilities
as broken because there is no credential — under a header saying the backend
answered and was billed, when nothing was sent — is the same failure the probes
exist to prevent, printed the other way round. It probes the endpoint the
account's kind belongs to (`proxy-behavior.md` §8.2), so a key is answered for
rather than reported as a subscription that failed everything.

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

A failed row prints the probe's rationale beneath it — what breaks silently
without that probe. Passing rows stay one line, because a rationale on every
row is a page of prose over a matrix nobody would then read.

Under `--live` the `count-tokens` row is marked as answered by the proxy. The
live header says the backend answered and was billed, and that is true of every
other row and false of this one: pre-flight sizing never leaves the proxy by
design. The row is marked rather than dropped, because a list whose job is to
be complete cannot quietly omit the surface it cannot vouch for.

One line under the matrix names what the run exercised and what it did not: the
account whose credential was spent, and — always — that the WebSocket transport
was not among it. A live run is HTTP only, since a probe is one turn with no
continuation and the socket's value is entirely in the incremental path. The
same line says whether the relay path (§9) was exercised. Green rows say
nothing about a path no probe drove, and a reader with no line to tell them
otherwise reads green as coverage of the whole proxy.

The relay path has a probe of its own, and it is replay-only. It drives the §9
branch against a recording whose marker sits inside a field the proxy does not
model, so a body round-tripped through the proxy's own types fails it. `--live`
skips that row: driving the relay against the real backend needs the account
serving turns switched to the second provider for the length of the run, and
that is not wired.

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
skipped every probe for want of a checkout would establish nothing at the
moment it is most likely to be run.

`usage` reports the serving account's quota as of its last turn. It costs
nothing to ask: the backend opens every stream with a snapshot, before it says
anything about the response, so the figure rides along with a turn already being
made and is never polled. Before any turn has been made it says so rather than
answering with zeroes. `--json` emits the snapshot as it stands, for a status
line.

**A figure per account, not per daemon.** A pinned tier's turns spend the
account it names (`proxy-behavior.md` §7.1), so a daemon can hold two live
figures at once. Each is held under the account that earned it and reported
beside the serving one, with how it was come by — riding a turn, or asked for
over the socket — and the moment it was taken. An account with no figure says
so, and says why: no turn made as it yet, a key holding no subscription
entitlement, or a provider that does not report a quota to this proxy at all.

**A second-provider account earns its figure the same way**, from the
`anthropic-ratelimit-unified-*` headers on the response to a relayed turn
(`proxy-behavior.md` §9.4) — the only place that provider states a quota for a
subscription credential. It is reported as having ridden a turn, because it did.
No plan name appears beside it: no header states one, and one is not deducible
from headroom.
Where there is one account, the block above is the whole answer and nothing is
repeated under its own name.

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
                  "command": "proxenos statusline -- ~/.claude/my-statusline.sh" } }
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
  before translation. The capture carries the request headers as they arrived —
  they are half of any question about what a client actually sends — with
  credential-bearing values (`authorization`, `x-api-key`, `cookie`,
  `proxy-authorization`) redacted by name: the header's presence is the datum,
  its value is a secret in a file that is not the credential store. A turn that
  is relayed rather than translated (`proxy-behavior.md` §9) is captured too,
  and its request is held as the exact bytes that were relayed — that path
  forwards the body verbatim, and a capture re-encoded through this proxy's own
  types would silently drop every field they do not model.
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

Either mode runs a daemon, so both take the daemon's port control: `--port`, or
`PROXENOS_PORT`, overriding the configured value — the same pair `run`
documents.

Captures are written beside the configuration, `0600`, and the most recent
twenty are kept. They hold conversation content — the system prompt, the
messages, and whatever the tools read.

Logging is controlled by `RUST_LOG`. Credentials never appear at any level.

### 2.2 `env` and `settings`

The configuration Claude Code needs, in two renderings. Neither is a degraded
version of the other; they carry different amounts because the client has two
configuration surfaces and only one of them is the environment.

`env` emits shell exports, for a shell:

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

**A mapping with any tier on the second provider states no window at all**, and
one served entirely by that provider omits `CLAUDE_CODE_DISABLE_1M_CONTEXT` too
— the client recognizes those ids by itself, and both variables would replace
what it knows with a figure this catalog cannot supply (`proxy-behavior.md`
§7.2). The tier variables are unchanged: they still carry the final ids.

**Every launch adds `ENABLE_TOOL_SEARCH=true`.** The client disables deferred
tool loading the moment its base URL is not a first-party host — it cannot
know what stands behind the proxy — and that variable is the client's own
documented override. Both paths carry the contract it needs: the relay
forwards `defer_loading` and `tool_reference` verbatim to a backend that runs
the search itself, and the translating path carries client-driven discovery
(`proxy-behavior.md` §2.5). Measured on both, live: an MCP set costing ~101k
tokens loaded up front defers to zero and the turns succeed.

All four tier variables are always emitted. `WebFetch` runs on the haiku tier, so
an unmapped haiku breaks it in a way that looks unrelated to tier mapping.

`CLAUDE_CODE_DISABLE_1M_CONTEXT` is not inert: without it this client appends
`[1m]` to an unrecognized id and assumes a million tokens, and it also strips
`context-1m-2025-08-07` from the beta list the client sends — see
`proxy-behavior.md` §7.2.

**Shell exports carry routing, plus the connector switch.** When
`client.disable_connectors` is on, the exports include
`ENABLE_CLAUDEAI_MCP_SERVERS=false` — the client's documented opt-out for the
claude.ai-hosted servers, and the one piece of client policy (§7.3 of
`proxy-behavior.md`) that has an environment variable. The rest — the denied
skill, the connector notice — lives in the client's settings file and has no
environment variable of any kind, so this rendering cannot deliver it. It says
so in a comment, which `eval` steps over, and the comment appears only when
there is a policy being left out.

`settings` emits one complete client settings document. `env --json` is the same
verb under the older name and prints the same bytes by running it, rather than
rendering the document a second time — two renderings of one thing is how the
older name kept a behaviour the newer one had dropped.

```json
{
  "env": { "ANTHROPIC_BASE_URL": "http://127.0.0.1:8787", "...": "..." },
  "permissions": { "deny": ["Skill(claude-api)"] },
  "disableClaudeAiConnectors": true
}
```

**This document is complete on its own.** Measured: a client started with no
`ANTHROPIC_*` in its environment, reading only a settings file holding this
document's `env` block, still reached the proxy. It needs no `eval`.

The `permissions` and `disableClaudeAiConnectors` keys are absent from the
*document* when nothing is configured, rather than present and empty. An empty
deny list merged over a real one is how a rule disappears.

**The payload behind it is the other way round.** The `env` method's `settings`
field is always present, an empty object when there is no policy, because
absence there is reserved for one thing only: a daemon that predates client
policy. One file is both the daemon and the CLI, and replacing it on disk does
not restart what is already running, so a newer CLI against an older daemon is
what an ordinary upgrade leaves behind. If "no policy" and "cannot answer" looked
alike, nothing could tell the operator which one they had.

`settings` and `exec` **refuse** against such a daemon rather than producing a
document that looks complete and lacks a permission rule. `env` continues,
because routing is all it ever carried and an older daemon has all of it — with
a comment naming the daemon it is talking to. `status` (§3) names the version
actually running.

**Redirecting this into a settings file overwrites that file.** `>` truncates;
it does not merge. `.claude/settings.local.json` in particular is where the
client itself records the permissions a user has accepted, so an existing file
with real content in it is the common case, not the corner case. Merge, or write
somewhere nothing else owns. Deep-merging with `jq -s '.[0] * .[1]'` is the
obvious one-liner and is wrong: it recurses into objects but takes arrays from
the right-hand side, so the existing `permissions.deny` is replaced rather than
extended.

The proxy publishes this document and never installs it. Applying it is the job
of whoever starts the client.

### 2.3 `exec`

Runs a command with the configuration of §2.2 applied, so starting a client is
one step rather than two.

```
proxenos exec claude --resume abc
proxenos exec -- claude --help
```

The environment half is set on the child. The policy half rides on the client's
own settings flag, passed inline: **nothing is written to disk**, so there is no
file to go stale and none to clean up. The document holds no secret — the auth
token's value is ignored by design — so a command line is a fine place for it.

Everything from the program name onward is opaque and forwarded in order, so the
client's own flags keep working unchanged. `--` is accepted for a command whose
first argument would otherwise be read as this verb's. On Unix the child is
`exec`d, so signals, job control, the terminal, and the exit status pass through
untouched.

**One argument is rewritten, and only for a relay-serving daemon**: a plain
`--model` id whose `[1m]` variant the curated list offers (§3) is upgraded to
that variant, and the rewrite is named on stderr. The suffix is the client's
own long-context selector, so the session starts on the million-token window
instead of silently assuming the standard one. An id already carrying the
marker, an alias the list does not name, and another program's `--model` are
forwarded as typed — and a daemon translating to the first provider rewrites
nothing, because there the marker makes the client assume a window it does not
have.

**It refuses, before starting anything, in three cases.**

When the daemon is not answering: launching anyway hands the operator a
connection refused from a client that cannot explain it.

When the daemon predates client policy (§2.2): the session would start with a
permission rule missing and nothing about it would ever say so.

When the forwarded arguments already carry `--settings`. Measured: given two
settings flags on one argument list, the client keeps the last, drops the first,
exits 0, and writes nothing to stderr. So leading with this proxy's document
loses the policy and trailing loses the caller's, both without a word. The
refusal names the collision and the way out; `proxenos settings` prints
this proxy's half to merge. A program that does not read the flag is never given
one, so its own `--settings` is not a collision — and because that launch drops
a rule the operator configured, it is named on stderr rather than left silent:
the launch carries the environment only.

**The policy half does not reach a grandchild.** A session started this way
inherits the environment into anything it spawns, but not the argument list, so
a client started from inside it carries the routing and not the policy. Anything
that spawns a client composes its own `--settings`.

### 2.4 `stop`

Asks the running daemon to stop, then reports what it observed afterwards.

```
$ proxenos stop
stopped 0.2.0; something started it again as 0.3.0
```

The observation is the useful half. Under a supervisor a stop is how a running
daemon is replaced by the build on disk, which is the answer to "the binary is
new and nothing changed" — one file is both the daemon and the CLI, and
replacing it does not restart what is already running (§2.2). Whether anything
restarts it belongs to the supervisor, so this reports what it saw rather than
claiming to have done it.

**It watches the `instance`, not the silence.** A socket falling quiet is a
statement about timing rather than about the daemon: a supervisor quick enough
leaves no gap to observe, and one that throttles a respawn leaves a gap longer
than any sensible wait. `status` therefore carries an id minted when the process
started, and a different id is a different process however the two overlapped.

The windows are three seconds for the daemon to go and twelve for anything to
bring it back, and it returns as soon as it sees the answer. Twelve because
launchd holds a respawn for ten seconds after the last start, and a shorter
window would report "nothing started it again" moments before something did,
sending the reader to `run` straight into the port the supervisor is about to
take.

**The answer arrives before the process goes.** A caller reading a closed
connection with no reply cannot tell a clean stop from a crash, and learning what
happened is the reason to ask over the socket rather than send a signal. The run
loop is released only once the response has been written.

**An in-flight turn is cut.** Someone typing `stop` means it, and a dropped
connection is something the client's own retry already handles.

**It cannot stop a daemon older than itself.** The verb exists to replace a
running daemon with the build on disk, and a daemon that predates the verb has
no method to ask — so the first upgrade past this version still has to be ended
by whatever supervises it. Nothing here can fix that; what it does is say which
situation it is rather than surface `unknown method` and leave the reader to
work out that a protocol error is really an upgrade problem.

### 2.5 `run --detach`

Starts the daemon in the background and returns once it answers.

```
$ proxenos run --detach
daemon running (pid 4711), logging to ~/.config/proxenos/daemon.log
stop it with `proxenos stop`
```

The child is a plain `run` of the same binary in its own process group, with
stdout and stderr appended to `daemon.log` in the configuration directory —
a detached process's terminal is gone the moment the command returns, so its
output needs somewhere durable to go. `stop` (§2.4) is the counterpart.

**Success is observed, not assumed.** The command exits 0 only once the daemon
answers the control socket. A child that dies first — a held port, a broken
configuration — is reported with the tail of what it wrote this start quoted,
and the command exits nonzero. Ten seconds without either is reported the same
way, and the child is ended rather than left to finish coming up after the
command has already called it a failure.

**A second detach is refused while the first still answers.** The control
socket is one per socket path, and a second daemon would take over the socket
file of the first, leaving the CLI answering for one daemon while another holds
the port. The refusal names `stop` as the way forward.

---

## 3. Control socket

A Unix domain socket, or a named pipe on Windows, carrying JSON-RPC:

| Method | Returns | v0.1 |
|---|---|---|
| `status` | connection state, whether the grant has been **refused**, plan and which source reported it, the tier mapping and the effort ceiling, any mapped model the catalog withholds, whether the catalog was authoritative, the client policy in effect, and the build and `instance` serving the socket | yes |
| `accounts.forget` | forgets one account — the selected one, or `{"account": name}` — and answers with the name it cleared and the one serving turns afterwards; the rest stay usable, and an idle account's removal leaves the serving grant's quota alone | no — was `disconnect` |
| `accounts` | every stored account, what kind of credential each holds, and which one serves turns; no tokens | no — v0.3 |
| `accounts.select` | `{"account": name}`, the account every following turn is made as, whether the catalog was refetched for it, and the tier mapping now in force; refuses, and moves nothing, where that account's mapping names a model its catalog does not have | no — v0.3 |
| `accounts.rename` | `{"account": from, "name": to}`, the name this daemon calls an account by, and whether an account section moved with it; the grant and the account id are untouched | no — v0.3 |
| `models` | catalog, whether it is the fallback list, and whether it was fetched for an account other than the one serving turns | yes |
| `tiers` | tier mapping | no — was `tiers.get` |
| `usage` | the serving account's quota as of its last turn, or that no turn has been made, plus `models` — the ids this daemon serves — and `accounts`, one entry per stored account with its own figure, its freshness, and `unavailable` where it has none | yes |
| `usage.refresh` | asks the backend for a figure now, for a front-end with nothing to show on a daemon that has served no turn | yes |
| `env` | the §2.2 block: `variables`, and `settings` always present | yes |
| `shutdown` | `{"stopping": true, "version": ...}`, then the process goes once the answer is written | yes |
| `record.start` / `record.stop` | fixture capture | yes — `{"mode": "ingress"}` by default, `"upstream"` must be named because it bills every turn that follows |
| `login` | authorization URL, then completion in the background; `status` reports when it landed. `{"label": name}` names the account it produces, and the answer states the label actually in force | yes |
| `login.cancel` | abandons a flow and releases the callback port | yes |
| `tiers.set` | tier mapping, validated against the catalog and in effect until the daemon stops; `{"account": name}` writes that account's section instead of the shared table. A tier's value takes the same two forms the file does — a model id, or `{"account": …, "model": …}` pinning the tier to another account. The pinned form needs `cross_account_tiers = true` and is refused by name without it, and its model is excluded from catalog validation: the catalog is the serving account's menu and cannot speak for the pinned one | yes |
| `effort.set` | the effort ceiling, or `null` to remove it; in effect until the daemon stops; `{"account": name}` as for `tiers.set` | yes |
| `cross_account_tiers.set` | `{"enabled": bool}` — consent for pinned tiers. **Always persisted**, unlike the setters above: consent is the operator changing what the daemon is, and a grant that evaporated at restart would leave the file refusing a mapping the operator permitted. Granting applies to the next call, not the next restart; revoking is refused by name while any tier still pins an account, because the write would produce a file the daemon refuses to start from | yes |
| `doctor` | probe results | no — `doctor` runs in the CLI, which is where `--live` can be given credentials without a daemon already holding them |

**`tiers.get` is gone and `tiers` replaces it.** Every other read on this
socket is a bare noun — `status`, `models`, `accounts`, `usage`, `env` — and each
of them coexists with namespaced writers under the same noun, `accounts.select`
and `usage.refresh` among them. A lone `.get` was one name a caller had to
remember separately for no capability it bought. Renamed rather than aliased,
for the reason below.

**`disconnect` is gone and `accounts.forget` replaces it**, and the answer's
`disconnected` field is `forgotten`. The old name shipped in v0.1, when there
was one account and disconnecting from it was the whole idea; with a store of
several, forgetting one is an account operation and every other account
operation is `accounts.<verb>`. Keeping it would have left one method outside
the pattern, and adding the new name beside it would have left two methods
doing one thing for as long as the other had to stay. Renamed rather than
either, because nothing but this project's own CLI has ever called the socket
— see §6 on what that permits and when it stops.

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

**The account tables are re-read from disk when they are needed.** They are the
one part of the configuration this daemon writes — `tiers.set` and `effort.set`
persist into them, and a rename moves them — so resolving them from the snapshot
taken at startup means a daemon that cannot see its own writes. Everything else
is still read once at startup, because nothing else here writes it. A file that
no longer parses keeps the snapshot: the daemon is already running on it.

**A persisted change is written where the value is read from.** An account
section shadows the shared table for the tiers it names and for the ceiling it
states (§4), so a change written to the shared table while such a section exists
would be in force on this daemon and gone at the next start — written, and left
looking applied. With no `account` named, each tier goes to the serving
account's section if that section already names it and to the shared table
otherwise; the ceiling follows the same rule. `{"account": name}` writes that
account's section regardless.

A change aimed at an account that is not the one serving turns is **written and
not applied**: the mapping in force belongs to the account being served, and it
is not validated against that account's catalog either, since a list fetched for
one account makes no claim about another. Without `persist` such a call would
change nothing anywhere, and is refused rather than answered as though it had
done something. Both answers carry `account` — null for the shared table — and a
`detail` that distinguishes written-and-applied from written-only.

**`effort.set` with `null` removes an override, not every ceiling.** Under an
account it clears that account's line, and the shared ceiling applies again; the
answer and the running daemon both report the ceiling that results rather than
the `null` that was asked for, because reporting no ceiling would be a figure
that lasted until the next start.

**A rename onto a name whose section is still in the file is refused.**
Forgetting an account leaves its section behind, so a name can be free in the
store and taken in the file; moving onto it would define one table twice, which
TOML refuses, and the daemon would fail to start on a file the operator never
edited. The store is renamed first and the file second, because the store is the
half that can refuse — and a write that fails puts the name back rather than
leaving an account and its section apart.

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

**A rename takes the account's configuration with it.** An account section is
keyed by the name (§4), so a rename that left it behind would detach a mapping
from the account it was written for — and a section naming nobody is not an
error, so nothing would say so. Only the table headers change; every key and
comment under them survives byte for byte. The file is written before the store,
because the other order can leave an account with no mapping, and this one can
only leave an orphan section. An account with no section is renamed without the
file being touched at all.

**A selection re-resolves the mapping, and can be refused.** The account's own
tiers and ceiling (§4) are resolved and validated against the catalog fetched
for it, before anything else moves; a mapping naming a model that account's
catalog does not have refuses the switch and leaves the daemon serving what it
was, catalog included. The answer carries the mapping now in force, because
after a switch it is not necessarily the one that was routing turns a moment
ago. Validation is skipped where the catalog cannot speak for this account — a
fallback list, or a refetch that failed and left the previous account's list in
force — for the reason startup skips it: a fetch that did not answer is not
evidence that a model went away. There `catalog_stale` says the list is not
this account's.

**A selection moves what routes turns.** `accounts.select` writes to the store
the ingress authenticates through, so the next turn is made as the account
named rather than the one this socket merely reports. Quota does not move with
it and does not need to: every figure is held under the account that earned it
(`proxy-behavior.md` §8.3), so what `usage` reports at the top follows the
selection by itself, and each account's own figure stays valid. `accounts.forget`
drops the figure of the account it forgets, serving or idle, because that
entitlement belongs to an account this daemon can no longer spend.

Live conversations are dropped with it. A conduit fixes its account on the
connection when it dials and reuses that connection for the conversation's life
(`proxy-behavior.md` §4.1), so a session left alone would go on being billed to
the account the operator has just moved off. Each dropped session pays a full
upload on its next turn, which is the direction §4.3 resolves every ambiguity
toward anyway.

`auth.dead` needs nothing of the sort: a refusal is held as the refresh token
that was refused, so it ends when the stored grant is no longer that token and
returns if it comes back.

**An account on the second provider answers from a curated list.** The fetched
catalog was never these models' menu (`proxy-behavior.md` §9.1), and the second
provider's own list endpoint names ids but states no windows — so `models` for
a relay-serving daemon answers from a list built into the binary, windows
included, and says `curated: true` rather than presenting it as a fetch. It is
a menu for reading, never a list to refuse by: no mapping is validated against
it, and `status` reports the catalog as curated instead of unvalidated.

**A catalog belongs to the account it was fetched for** (`proxy-behavior.md`
§7.0). `accounts.select` and an `accounts.forget` that hands over to another account
fetch it again as whoever serves now, and their answers carry
`catalog_refreshed` — a fetch that failed keeps the previous list in force, and
everything downstream of it still describes that account. A CLI `login` calls
`accounts.select` when it lands **and only when it selected** — a login that
left the selection where it was moved nothing for the catalog to follow. Where nothing
refetched — a `login` started here and completed in the background, or a CLI
one made while no daemon was running — `status.catalog_stale` and `models.stale`
say the list is not this account's and `status.catalog_account` names whose it
is.

**`status` names the account.** `auth.account` is what this daemon calls the one
serving turns and is what selects it; `auth.account_id` is what the backend
calls it and is what appears on a request; `auth.kind` is `grant` or `key`,
which decides which endpoint it is spent against and what it can be asked for;
`auth.provider` is the other half of that decision — which provider's endpoint,
`codex` unless the account says otherwise, and each entry of `auth.accounts`
carries the same field. Stored entries without one are the first provider's:
every credential file written before the field existed reads unchanged, and the
CLI renderings name a provider only where it is not `codex`.
`auth.connected` means there is a credential to spend, of either kind — a key
has no grant behind it, and reading only the grant reported a daemon that could
serve every turn as not connected. `auth.accounts` lists every stored
account — present and empty rather than absent — carrying names, ids, addresses,
plans as of each login, and expiries. It carries no tokens: this is the one
credential-shaped answer that leaves the process.

**`login` runs in the daemon.** It answers with the authorization URL and
returns; the flow completes in the background and `status` is what reports that
it did. A control call that blocked until the operator finished in a browser
would hold the socket for minutes and give a front-end nothing to render. There
is one fixed callback port, so a second caller **joins** the first and is told so
rather than being handed a URL whose callback would then be rejected as not
matching. A join takes the running flow's label along with its URL, and the
answer states the label in force rather than echoing the one asked for: a
caller that could not tell would go looking for an account that was never going
to exist. An abandoned flow releases the port after ten minutes.

**`usage.refresh` is not the primary path and does not replace it.** The backend
volunteers a snapshot at the head of every stream; that one is free, rides a turn
already being made, and is what `usage` reports. This exists for the case that
path cannot cover — a front-end with a figure to show on a daemon that has served
no turn yet — and its answer is recorded where the stream path records its own,
under the serving account, so everything reading a quota reads one value. It is
recorded as asked for rather than as volunteered: both are true figures and they
go stale differently, so `usage` states which one each account's figure is.

**`status` reports the version of the build serving the socket.** It is not
necessarily the build that asked: one file is both, and replacing it does not
restart a running daemon. The CLI says so only when the two differ, because a
line printed on every run is one nobody reads on the run that matters.

**`env` keeps its name although its payload now carries more than an
environment.** The two halves are named inside it — `variables` and `settings` —
and a caller reading only the first is untouched by the second. Renaming the
method would cost a shim in a caller that already speaks it and buy no
capability, so the honesty went into the field names and the CLI verb: `settings`
is the name for the document, and `env` stays the name for the exports.

The names are reserved whether or not v0.1 answers them: they are semver-bound
(§6), and a method that appears later must mean what its name said all along. A
reserved method reports that it is unimplemented rather than failing as though
it were unknown.

The daemon holds authoritative state and any front-end is a client of this
interface. The CLI has no privileged path of its own; a second front-end needs no
new daemon work.

---

## 4. Configuration

TOML in the platform configuration directory — `$PROXENOS_HOME`, else
`$XDG_CONFIG_HOME/proxenos`, else `~/.config/proxenos`. Credentials
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

# Optional, one per account, keyed by the name `accounts` lists it under.
[accounts.spare]
effort = "low"

[accounts.spare.tiers]
opus = "..."

[transport]
websocket   = true
compression = true

[instructions]
identity       = true
working_budget = true
append         = "..."

[client]
deny_skills            = ["claude-api"]
disable_connectors     = true
disable_remote_control = true

[upstream]
client_version           = "2.0.0"
effective_window_percent = 95.0
endpoint                 = "https://..."
websocket                = "wss://..."
catalog                  = "https://..."

[upstream.key]
endpoint = "https://..."
catalog  = "https://..."

[upstream.anthropic]
endpoint = "https://..."
```

`[upstream.key]` is where an API key is spent, which is not where a grant is
(`proxy-behavior.md` §8.2). There is no socket in it: the WebSocket protocol
belongs to the subscription backend, so a key account uses HTTP. Sending either
credential to the other's endpoint is refused before anything leaves.

`[upstream.anthropic]` is where a relayed turn goes (`proxy-behavior.md` §9).
One entry, because the relay does one thing: it speaks the surface this proxy
already exposes, so there is no catalog to translate and no socket protocol to
speak.

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

**An account section states what differs for one account.** A catalog is one
account's menu (`proxy-behavior.md` §7.0), so a mapping is only ever right for
the models every account has: two subscriptions on different plans are offered
different models, and a key account beside a subscription need not overlap at
all. `[accounts.<name>.tiers]` replaces the tiers it names and no others, and
`effort` under `[accounts.<name>]` replaces the shared ceiling rather than being
capped by it — an operator who writes a different one for an account means that
one. The key is the name `accounts` lists, because that is the string every
account verb takes and a key account has no id to be named by. An account with
no section takes the shared tables, which is also what a daemon with nothing
selected uses.

The four tiers default to the mapping above. An omitted tier takes its default; a
tier written blank is refused, because an omission accepts the shipped answer
while a blank is a mistake. Each mapped model is validated against the live
catalog when one is reachable. That validation happens once, at startup: the
catalog is not refetched, so a mapping cannot go stale while the daemon runs.

A tier entry is a model id, or a table pinning one to another account:
`haiku = { account = "spare", model = "..." }` serves that tier's turns as
`spare` whatever account serves the rest of the session. The table form is
gated by the top-level `cross_account_tiers = true` — it routes one client's
traffic across accounts' quotas, which is a decision the operator owns, so its
absence refuses the daemon at startup rather than falling back to the serving
account. Falling back would spend the wrong account's quota invisibly. The
bare-string form is ungated and keeps the meaning it has always had.

The pin decides which credential authenticates: every upstream request that tier
produces goes up as the pinned account, and unpinned tiers are unchanged. A pin
naming an account the store does not hold refuses the turn with
`invalid_request_error`, naming the account and listing what is stored, and
nothing reaches the backend as somebody else. A pinned account holding a
credential the endpoint does not take is refused the same way, naming the pinned
account. `proxy-behavior.md` §7.1 carries the rest, including what a refresh on
a pinned grant does.

`[client]` is policy the client applies to itself, which settings mostly carry
and environment variables mostly cannot — see `proxy-behavior.md` §7.3 for why
each default is what it is. `deny_skills` names skills refused for a session
served here; the proxy writes the `Skill(...)` rule the client understands,
because a rule built by hand and built wrong denies nothing and reports nothing.
An empty list allows everything. **Left unset, the default is resolved per
launch**: `claude-api` is denied for a launch whose turns translate, and
nothing is denied for one whose turns are all relayed — the skill documents
the second provider's API, the wrong reference for a translated session and
the right one for a relayed session. A written list is the operator's rule and
applies on either path. `status` reports the list a launch would actually
apply. `disable_connectors` does two things through
one intent: the settings key (`disableClaudeAiConnectors`) suppresses the
connector notice the client prints whenever an auth token is set, which here is
always, and the export (`ENABLE_CLAUDEAI_MCP_SERVERS=false`) is the client's
documented opt-out for the claude.ai-hosted servers themselves — the half that
still reaches a client launched from `proxenos env` alone.
`disable_remote_control` writes `remoteControlAtStartup: false`, keeping the
client from starting its remote-control session at launch: a session started
through a local proxy is a local decision.

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
- **The credential directory has to be on a filesystem that locks.** Every write
  of the credential file takes a lock beside it, and a filesystem that cannot
  take one — a network mount is the case that exists — fails the write rather
  than proceeding without it. The error names `PROXENOS_HOME`, which
  points the whole directory somewhere else.
- **A key account's catalog carries no windows or efforts.** The list is real
  and is the account's own, and the endpoint states neither for any entry. The
  window guard therefore never fires for a key account and the model half of the
  effort cap has nothing to cap against. The ceiling set in configuration still
  applies.

- **Claude Code never reaches the `input_file` path.** It rasterises PDFs into
  image blocks, so documents from that client reach the model as images. The
  `document` translation is for a client that sends one, and the backend does
  accept it — measured by posting a `document` block directly, which returned a
  code that existed only inside the PDF.

- **Compression saves bytes and no tokens.** Subscription HTTP bodies are
  zstd-compressed and WebSocket frames use `permessage-deflate`, negotiated
  during the upgrade. Roughly two thirds off in both directions, and the inbound
  half is the larger one — the backend echoes the whole request back three times
  per turn. Quota is unaffected. A key request is neither: it is never
  compressed, and there is no socket for it to compress.

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

**Before 1.0 that rule has one deliberate exception, and it closes on its own.**
Semantic versioning does not bind a zero major, and nothing outside this
project's own CLI has ever spoken the socket — the CLI and the daemon are one
binary, so a rename lands on both at once. A name that turns out wrong is
therefore renamed on a minor bump, said in the changelog, and gone rather than
left beside its replacement. `accounts.forget` arrived that way, and so did the
project's own name: everything `codex-cc-proxy` and `CODEX_CC_PROXY_*` named is
`proxenos` and `PROXENOS_*` from v0.5.0, one rename with no aliases kept. The exception
ends when a second caller exists — the graphical front-end is the one planned,
and any other program that speaks this socket ends it just as well — and it ends
whether or not 1.0 has been reached: the moment something else has to be
upgraded in step, only additions are safe. It is a statement about callers, not
about a version number.

**The bound method set is the whole of §3's table, named here so the freeze is
a contract rather than folklore.** From v0.7.0 — the release that ships the
graphical front-end, and with it the second caller the exception above is a
statement about — these nineteen names are fixed: `status`, `shutdown`, `login`,
`login.cancel`, `accounts`, `accounts.select`, `accounts.rename`,
`accounts.forget`, `models`, `tiers`, `tiers.set`, `effort.set`,
`cross_account_tiers.set`, `usage`, `usage.refresh`, `env`, `doctor`,
`record.start`, `record.stop`. `doctor` is bound although it is not implemented:
a reserved name that appears later must mean what its name said all along. The
same list is a constant in the daemon, so removing or renaming one is a visible
change to the code and not only to this document.

**An unknown method reaches the caller as an unknown method.** The error code
survives the round trip rather than being flattened into one kind, because
"this daemon does not have that method" and "that method refused what you asked"
are different situations and only the first is answered by replacing the daemon.

**A field added to a response is a capability, and a caller that needs it checks
for it.** Adding one is not a breaking change: an older caller ignores what it
does not know, and must not be "fixed" into a strict check, because that would
make every upgrade have to be simultaneous. The obligation runs the other way. A
newer caller that requires a field has to establish it is there rather than infer
it from a version string — comparing versions forces a policy about which
differences matter and gets it wrong for a patched build or a forgotten bump.
Where a field's absence would otherwise be ambiguous, it is emitted empty rather
than omitted, so that absence keeps meaning "this daemon predates it" and nothing
else.

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
