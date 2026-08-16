# codex-cc-proxy

Claude Code, running on OpenAI models served through a ChatGPT subscription,
without modifying Claude Code.

An Anthropic Messages API on the front, the OpenAI Responses API on the back,
and a translation layer between them whose real job is keeping Claude Code's
built-in tools working.

---

## Why this rather than a generic translator

A dozen proxies will map Messages onto some other chat API. Claude Code is not
an ordinary Messages client: several of its built-in tools depend on behaviour
the *server* provides, and each of them fails **silently** when a translator
handles only messages and function calls. Every request still returns 200.

| Path | What it needs from the server | What happens without it |
|---|---|---|
| `Read` (image, PDF) | attachment blocks inside a tool result | the bytes never arrive, and the model describes the file from its name |
| `WebSearch` | a server-side search tool, and its results as structured blocks | the search returns nothing, reported as "no results" |
| `WebFetch` | a model call on the haiku tier | breaks in a way that looks unrelated to tier mapping |
| tool search | deferred tool stubs and discovery | discovered tools stay uncallable, or every stub inflates context |
| context meter | input tokens in the first frame | the meter collapses to zero at the start of every turn |

Preserving those is the product. [`docs/proxy-behavior.md`](docs/proxy-behavior.md)
is the normative specification, and most of its rules exist because the obvious
implementation is wrong in a way that does not fail loudly.

---

## Status

**v0.1.0, and honest about what that means.** Everything here is verified
against a local replay server built from the upstream protocol definitions. No
part of it has been confirmed against a live backend, because the subscription
it was written on is out of quota.

That is a real distinction and the project keeps it visible: `doctor` states
what its capability matrix was run against, and
[`docs/roadmap.md`](docs/roadmap.md) §L lists every question only a working
subscription can answer. Treat a green matrix as evidence the proxy does its
half.

---

## Getting started

```sh
just setup          # toolchain and test runners
just check          # formatting, lints, and the whole suite
```

The suite runs offline. Nothing in it reaches the network, which is a design
constraint rather than a convenience: a project whose correctness can only be
demonstrated by spending money stops being demonstrable at an arbitrary moment.

To try it end to end without credentials, run the capability probes against the
fixture corpus:

```sh
cargo run -- doctor
```

To use it for real, write a configuration first. It lives at
`~/.config/codex-cc-proxy/config.toml` (or `$XDG_CONFIG_HOME`), and `run` prints
this example if it is missing:

```toml
port = 8787

[tiers]
opus   = "gpt-5-codex"
sonnet = "gpt-5-codex"
haiku  = "gpt-5-codex-mini"
fable  = "gpt-5-codex-mini"
```

Both the model and the reasoning effort are chosen per request, not baked in.
Claude Code sends its effort with every request and the proxy honours it; it
also sends a model id, and any id the backend knows passes straight through. So
`ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-5.6-terra` takes effect on the next request
with no restart, and the `effort` key above is a *ceiling* on what the client
asks for rather than a fixed value.

All four tiers are required and the daemon refuses to start without them.
`WebFetch` runs on the haiku tier, so an unmapped haiku breaks it in a way that
looks like something else entirely — refusing to start is the only failure that
points at the cause. Credentials never go in this file.

Then:

```sh
cargo run -- login                       # authenticate
cargo run -- run                         # start the daemon on loopback
eval "$(cargo run -q -- env)"            # point Claude Code at it
claude
```

`status`, `models`, `env`, and `doctor` all talk to the running daemon over a
control socket; the CLI holds no state of its own. See
[`docs/api.md`](docs/api.md).

---

## How it fits together

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
clock, no filesystem. That boundary is what makes every translation rule
testable as a pure function over recorded data.

WebSocket is primary and HTTP is its fallback, but neither is a degraded version
of the other — the backend closes WebSocket connections under policy conditions
often enough that HTTP is a normal operating mode. A session that falls back
stays fallen back rather than retrying the socket every turn.

Set `websocket = false` under `[transport]` to use HTTP only.

---

## Security and privacy

The daemon binds `127.0.0.1` and refuses anything else. It performs no
authentication, which is safe precisely because every caller reaching the socket
is already a local process running as the user. `ANTHROPIC_AUTH_TOKEN` must be
set for Claude Code's sake and its value is ignored.

Credentials live in a file created `0600`, never in the configuration file,
never in process arguments, and never in logs. Nothing is collected and nothing
is transmitted anywhere but the backend.

See [`SECURITY.md`](SECURITY.md).

---

## Contributing

Development is test-first, and the specification comes first: read the relevant
section of [`docs/proxy-behavior.md`](docs/proxy-behavior.md) before touching
translation, transport, sessions, or token accounting. If implementation shows a
rule is wrong, change the spec in the same commit as the code that proved it.

[`CONTRIBUTING.md`](CONTRIBUTING.md) has the rest.

---

## Posture

The upstream endpoint is not a published or supported API. It may change or be
withdrawn without notice, and using a subscription this way is a decision each
operator makes for themselves. There is no version of this project that avoids
that, so it is stated rather than omitted.

Not affiliated with, endorsed by, or sponsored by Anthropic or OpenAI. All
trademarks belong to their owners.

Licensed under [Apache-2.0](LICENSE).
