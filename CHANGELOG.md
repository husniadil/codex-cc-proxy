# Changelog

All notable changes to this project are recorded here. This project follows
[semantic versioning](https://semver.org). The semver-bound surfaces are listed
in [`docs/api.md`](docs/api.md) §6.

## [Unreleased]

## [0.1.0]

First release.

### Added

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

Compression over WebSocket is the one thing still unavailable — the extension
the endpoint offers is `permessage-deflate`, which no published Rust WebSocket
library implements.
