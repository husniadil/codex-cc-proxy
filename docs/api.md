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
codex-cc-proxy record     capture exchanges as fixtures
```

Every verb except `run` and `login` operates through the control socket (§3)
against a running daemon.

`login` runs in the CLI. It needs a browser and a fixed callback port, and the
daemon need not be running to authenticate — requiring it would mean starting a
daemon that cannot serve a request in order to obtain the credentials it needs
to serve one. The authorization URL is printed as well as opened, so an
environment with no browser still has a way through.

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

### 2.1 `env`

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
| `status` | connection state, tier mapping, whether the catalog was authoritative | yes |
| `disconnect` | clears credentials | yes |
| `models` | catalog, and whether it is the fallback list | yes |
| `tiers.get` | tier mapping | yes |
| `usage` | quota snapshot, or that none has been seen | yes |
| `env` | the §2.1 block | yes |
| `record.start` / `record.stop` | fixture capture | yes — `{"mode": "ingress"}` by default, `"upstream"` must be named because it bills every turn that follows |
| `login` | authorization URL, then completion | no — `login` runs in the CLI, which owns the browser and the callback port |
| `tiers.set` | tier mapping | no — edit the configuration file |
| `doctor` | probe results | no — `doctor` runs in the CLI, which is where `--live` can be given credentials without a daemon already holding them |

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

[tiers]
opus   = "..."
sonnet = "..."
haiku  = "..."
fable  = "..."

[transport]
websocket   = true
compression = true
```

All four tiers are required. The daemon refuses to start without them, and
validates each against the live catalog when one is reachable.

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

- **Compression is unavailable over WebSocket.** The endpoint offers
  `permessage-deflate`, negotiated during the upgrade, and no published Rust
  WebSocket library implements the extension. HTTP bodies are zstd-compressed;
  WebSocket frames are plain text JSON. It costs bytes, not tokens.

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
