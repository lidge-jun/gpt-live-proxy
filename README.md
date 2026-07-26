# gpt-live-proxy

A standalone Rust proxy for GPT-Live and the OpenAI Realtime voice protocol.

> Compatibility expansion in progress: commit `de1240b` implements the
> OpenCodex-derived WebRTC call-create and sideband subset. It is **not yet** a
> drop-in proxy for the full official Realtime GA API. The audited contract and
> dependency-ordered implementation roadmap begin at [`docs/080`](docs/080_official-realtime-ga-contract.md).

It sits between a Codex-style voice client and OpenAI, forwarding call-create
over HTTP and the sideband control channel over WebSockets. The proxy owns
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
- Only `content-type` and `location` come back from the upstream response.

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

| Mode | Base | Call-create body | Sideband host |
|---|---|---|---|
| `chatgpt` | `https://chatgpt.com/backend-api/codex` | JSON `{sdp, session?}` | `api.openai.com` |
| `apikey` | `https://api.openai.com/v1` | multipart, forwarded verbatim | the configured base |

The body shape follows the base URL rather than the mode, matching upstream: a
base containing `/backend-api` gets the JSON shape wherever it points.

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
| `GPT_LIVE_BASE_URL` | per mode | upstream base; no query, fragment, or userinfo |
| `GPT_LIVE_TOKEN` | required | upstream bearer |
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

A clean frame records no payload at all. A corrupted one adds a `context` field:
a bounded excerpt around the first replacement character.

Records are written by a background thread and are **dropped** rather than
queued indefinitely if that thread falls behind, or if the write fails. Stalling
a voice call to guarantee a diagnostic line would be the wrong trade, so the log
is best-effort by design.

**Bounded is not empty.** The excerpt spans at most 24 scalars on each side, and
a frame shorter than that window is captured whole. If a secret sits next to the
corruption, it is in the excerpt. That is a deliberate trade — an excerpt
without its surroundings could not attribute anything — so treat the file as
sensitive and write it outside the working tree.

## Quickstart

```bash
cargo build --release

GPT_LIVE_TOKEN=… \
GPT_LIVE_UPSTREAM_MODE=chatgpt \
GPT_LIVE_ACCOUNT_ID=… \
  ./target/release/gpt-live-proxy
```

For the currently implemented OpenCodex Live subset, point the client API base
at `http://127.0.0.1:10110/v1`. Full official base-URL compatibility is tracked
by `docs/080` through `docs/150` and must not be assumed from the current binary.

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
