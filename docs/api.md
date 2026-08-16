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
| Upstream rejection | `api_error` | upstream status |
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
codex-cc-proxy status     connection, tier mapping, quota
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
corpus — the default — it contacts nothing and costs nothing; against a live
backend it spends real inference quota.

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
- **upstream** captures what the backend sends back. It needs credentials and
  spends quota.

Both write to the same fixture format, so a test replays either without knowing
which mode produced it.

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
CLAUDE_CODE_DISABLE_1M_CONTEXT=1
```

All four tier variables are always emitted. `WebFetch` runs on the haiku tier, so
an unmapped haiku breaks it in a way that looks unrelated to tier mapping.

`CLAUDE_CODE_DISABLE_1M_CONTEXT` is inert for ordinary model ids and is set as a
one-sided floor — see `proxy-behavior.md` §7.2.

---

## 3. Control socket

A Unix domain socket, or a named pipe on Windows, carrying JSON-RPC:

| Method | Returns |
|---|---|
| `status` | connection state, tier mapping, quota windows |
| `login` | authorization URL, then completion |
| `disconnect` | clears credentials |
| `models` | live catalog |
| `tiers.get` / `tiers.set` | tier mapping |
| `usage` | quota snapshot |
| `env` | the §2.1 block |
| `doctor` | probe results |
| `record.start` / `record.stop` | fixture capture, either mode |

The daemon holds authoritative state and any front-end is a client of this
interface. The CLI has no privileged path of its own; a second front-end needs no
new daemon work.

---

## 4. Configuration

TOML in the platform configuration directory, watched and applied without
restart. Environment variables override file values. Credentials are never stored
here.

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

[estimator]
backend = "calibrated"   # or "tokenizer"
```

All four tiers are required. The daemon refuses to start without them, and
validates each against the live catalog when one is reachable.

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

- **Three request shapes have no upstream precedent** and are derived from the
  public API rather than observed in use: `input_file` for documents, a
  `tool_choice` other than `auto`, and `url_citation` annotations as the source
  of web-search results. Each fails loudly rather than silently if this backend
  rejects it, and each is a question in `roadmap.md` §L.

- **A web search that produced no citations reports the pages the model opened**,
  which carry a URL but no title. That is worse than a real citation and better
  than an empty result, which the client reads as "nothing found".

- **Nothing about the live backend is confirmed.** Every capability claim in this
  project is derived from the upstream protocol definitions and verified against
  replayed fixtures. `doctor` says so on the face of its output, and
  `roadmap.md` §L lists what only a subscription can settle.

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
