# gpt-live-proxy

A standalone Rust proxy for GPT-Live and the OpenAI Realtime voice protocol.

> The API-key profiles now implement the official Realtime REST, WebSocket, and
> WebRTC relay surfaces. Official-SDK base-URL-only
> conformance and final CI hardening remain tracked by [`docs/140`](docs/140_conformance-and-hardening.md),
> so this is not yet a completed drop-in-compatibility claim. The ChatGPT
> profile remains limited to the source-proven private V1 and Frameless flows.

It sits between a Realtime client and OpenAI, forwarding HTTP, WebSocket, and
WebRTC signaling surfaces. In managed profiles the proxy owns
authentication and otherwise stays out of the way: sideband **data frames** are
relayed verbatim, text as text and binary as binary, with no parsing or
re-encoding.

It is not a pure pipe, and the exceptions are deliberate:

- A ChatGPT `backend-api` call-create rewrites the client multipart body into
  the JSON shape that host expects. The API-key path forwards multipart
  untouched.
- A session `id` is stripped from a call-create body the proxy *builds* — the
  ChatGPT rewrite path. The API-key path forwards multipart untouched, so an id
  the client put there survives.
- A client close code is normalized to `1000` before it reaches the upstream.
- Ping and pong are answered per leg rather than forwarded.
- On private legacy call-create only, the response keeps `content-type` and
  `location`; official responses use the audited metadata allowlist.

This is a reimplementation of the relay in
[OpenCodex](https://github.com/lidge-jun/opencodex). The contract was
reconstructed from that project behavior and from the upstream architecture
record, then written fresh in Rust; the [`docs/`](docs) notes cite what each
rule came from. The two defects that record describes are
encoded structurally here, so they cannot recur by accident.

## Why a relay is subtle

Two failures from the original are worth stating up front, because they explain
most of the design.

**A dropped header is not a body problem.** A Frameless session has no top-level
`type`. When the proxy forwarded the body but discarded `openai-alpha`, the
backend fell back to v1 validation and rejected the session for *lacking a
`type`*. The tempting fix — add the field — would have converted a Frameless
request into a V1 request. The real fix was to stop dropping the header. Here,
six protocol headers are forwarded explicitly and `FramelessSession` has no
`type` field to serialize.

**A local `101` proves nothing.** After call-create succeeded against
`chatgpt.com/backend-api`, the sideband join to that same host failed before it
opened, while the downstream socket had already reported `101 Switching
Protocols`. The sideband lives on `api.openai.com` even for a ChatGPT-authenticated
call. That rule is a named constant with the commit in its comment.

## Routes

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/v1/live` | call-create (GPT-Live / Frameless) |
| `POST` | `/v1/realtime/calls` | call-create (public Realtime alias) |
| WS | `/v1/live/{callId}` | sideband join, path style |
| WS | `/v1/realtime/calls/{callId}` | sideband join, path style |
| WS | `/v1/realtime?call_id=` | sideband join, query style |
| `GET` | `/healthz` | liveness; the only route outside the trust boundary |

Every route except `/healthz` sits behind a middleware layer that enforces
draining, admission, and origin policy, so a route added later is guarded
whether or not its author remembers to do anything.

## Upstream profiles

| Profile | Configuration | Credential owner | Default base |
|---|---|---|---|
| API-key managed | `GPT_LIVE_UPSTREAM_MODE=apikey` | proxy (`GPT_LIVE_TOKEN`) | `https://api.openai.com/v1` |
| API-key client | `GPT_LIVE_UPSTREAM_MODE=apikey`, `GPT_LIVE_CREDENTIAL_MODE=client` | each request or WebSocket subprotocol | `https://api.openai.com/v1` |
| ChatGPT managed | `GPT_LIVE_UPSTREAM_MODE=chatgpt` | proxy token and optional account id | `https://chatgpt.com/backend-api/codex` |

The body shape follows the base URL rather than the mode, matching upstream: a
base containing `/backend-api` gets the JSON shape wherever it points.

## Realtime surface matrix

`Native` relays the official contract without a private protocol adaptation.
`Adapted` is a source-proven private V1 or Frameless mapping. `Unsupported`
fails before upstream contact with `unsupported_realtime_capability`.

| Surface | API-key managed | API-key client | ChatGPT | Required profile when unsupported |
|---|---|---|---|---|
| Official GA REST: voice call-create, call control, client secret, legacy session | Native | Native | Unsupported | `apikey_managed` or `apikey_client` |
| Official transcription: session token and standalone semantics | Native | Native | Unsupported | `apikey_managed` or `apikey_client` |
| Official translation: client secret, WebRTC call-create, WebSocket | Native | Native | Unsupported | `apikey_managed` or `apikey_client` |
| Official standalone voice WebSocket | Native | Native | Unsupported | `apikey_managed` or `apikey_client` |
| Official existing-call/SIP sideband WebSocket | Native | Native | Unsupported | `apikey_managed` or `apikey_client` |
| Private V1 call-create | Adapted | Unsupported | Adapted | managed: `apikey_managed` or `chatgpt` |
| Private V1 sideband, query or historical alias | Adapted | Unsupported | Adapted | managed: `apikey_managed` or `chatgpt` |
| Private V1 standalone WebSocket | Adapted | Unsupported | Unsupported | `apikey_managed` |
| Private Frameless call-create | Adapted | Unsupported | Adapted | managed: `apikey_managed` or `chatgpt` |
| Private Frameless sideband, query or historical alias | Adapted | Unsupported | Adapted | managed: `apikey_managed` or `chatgpt` |
| Private Frameless standalone WebSocket | Adapted | Unsupported | Unsupported | `apikey_managed` |

Three similarly named concepts are intentionally separate: public Realtime
GA/V2 is the official API surface; `quicksilver=v2` is the private Frameless
negotiation token; Codex app-server RPC v2 is not an HTTP/WebSocket route owned
by this proxy. SIP call control and existing-call sideband are covered above,
but SIP trunk configuration and incoming webhook delivery cannot be selected by
changing an API base URL and are out of scope.

## Header ownership

Exactly six client headers are forwarded upstream:

```
openai-alpha  x-session-id  session-id  thread-id  originator  x-oai-attestation
```

The upstream header map is **built**, never copied from the request. Everything
else — cookies, the admission credential, `x-openai-fedramp`, anything a caller
invents — stops here. `authorization` and `chatgpt-account-id` are proxy-owned
and always win. An absent `openai-alpha` stays absent: inventing one would
negotiate a protocol the client never asked for.

Admission and upstream credentials are different domains. An
`Authorization: Bearer` that *is* the admission secret is refused rather than
forwarded, since it means nothing to OpenAI and forwarding it would leak this
proxy's own secret. Send the proxy credential in `X-GPT-Live-API-Key` and the
upstream bearer in `Authorization`.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `GPT_LIVE_BIND` | `127.0.0.1:10110` | listen address |
| `GPT_LIVE_UPSTREAM_MODE` | `chatgpt` | `chatgpt` or `apikey` |
| `GPT_LIVE_CREDENTIAL_MODE` | `managed` | `managed` or client-supplied `client`; client mode requires `apikey` |
| `GPT_LIVE_BASE_URL` | per mode | upstream base; no query, fragment, or userinfo |
| `GPT_LIVE_TOKEN` | required in managed mode | proxy-owned upstream bearer; omitted in client mode |
| `GPT_LIVE_ACCOUNT_ID` | — | ChatGPT account id |
| `GPT_LIVE_API_KEY` | — | admission credential; required unless bound to loopback |
| `GPT_LIVE_CORS_ORIGINS` | — | comma-separated extra origins |
| `GPT_LIVE_FRAME_LOG` | — | frame-forensics path; disabled when unset |
| `GPT_LIVE_LOG` | `info` | tracing filter |

A loopback bind exempts callers from admission auth. A non-loopback bind without
a configured credential fails closed rather than serving the relay to the
network unauthenticated.

## Frame forensics

Setting `GPT_LIVE_FRAME_LOG` appends a JSONL record for each relayed **data**
frame - text and binary only. Control frames are deliberately outside this
diagnostic: a close reason is peer-controlled text and could carry transcript
content, so it is never written here or to a log line.

```json
{"ts":"…","dir":"c2u","kind":"text","bytes":123,"fffd":false}
```

It exists to answer one question: when a transcript shows U+FFFD (the Unicode
replacement character) - was it already in the upstream frame, or did the relay
introduce it?

A clean frame records no payload at all. A corrupted frame adds only
`fault_byte_offset`, the first replacement/invalid-UTF-8 byte position. It never
records an excerpt, payload bytes, a reversible digest, a protocol value, or a
close reason.

Records are written by a background thread and are **dropped** rather than
queued indefinitely if that thread falls behind, or if the write fails. Stalling
a voice call to guarantee a diagnostic line would be the wrong trade, so the log
is best-effort by design.

Even a secret immediately adjacent to U+FFFD is absent from the JSONL record.
The file still contains timing, direction, frame type, and byte-count metadata,
so write it outside the working tree and enable it only for diagnostics.

## Quickstart

```bash
cargo build --release

GPT_LIVE_TOKEN=… \
GPT_LIVE_UPSTREAM_MODE=chatgpt \
GPT_LIVE_ACCOUNT_ID=… \
  ./target/release/gpt-live-proxy
```

For official Realtime routes, run the `apikey` profile and point the client API
base at `http://127.0.0.1:10110/v1`:

```bash
GPT_LIVE_UPSTREAM_MODE=apikey \
GPT_LIVE_TOKEN=sk-… \
  ./target/release/gpt-live-proxy
```

The REST, WebSocket, and WebRTC relay paths are implemented. The official Node
SDK base-URL-only conformance gate is still the `docs/140` work phase, so use
the route matrix above rather than treating the current binary as a fully
certified drop-in release. With `GPT_LIVE_UPSTREAM_MODE=chatgpt`, only private
V1/Frameless call-create and existing-call sideband are supported; official GA,
transcription, translation, and private standalone WebSockets are not.

## Development

```bash
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

CI runs those three on Linux and macOS, with every action pinned to an immutable
commit SHA.

The design notes in [`docs/`](docs) are the working record: the wire contract
with line-level citations, the relay's observable behavior, and one document per
implementation phase.

## License

MIT.
