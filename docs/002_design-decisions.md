# 002 — Design decisions for the Rust port

## D1. This is a standalone service, not an OpenCodex fork

OpenCodex resolves upstream credentials through its account pool, thread affinity, cooldown tracking, and JWT-claim validation. Those subsystems are not part of the wire contract; they are OpenCodex product policy. `gpt-live-proxy` reimplements the **protocol** faithfully and models upstream selection as a small, explicit `UpstreamProfile` configuration instead.

Consequence: the pool-derived rows of `001` §10 have no equivalent here — `429` cooldown, `409` thread affinity, and the two pool `401`s (selected-account reauthentication and pool authentication failure). Everything else in that inventory is reproduced.

## D1b. Scope: relay only, not a Realtime client

`000` documents the full upstream client behavior, including standalone WebSocket sessions, `session.update` emission, and the per-adapter inbound/outbound event vocabulary. **Those are explicitly out of scope.** This service is a relay: it never originates `session.update`, never normalizes events, and never opens a standalone session of its own.

What the wire-adapter layer (`040`) exists for is the *relay's* decisions — which `openai-alpha` value a request carries, which call-create URL shape and body shape to use, which sideband join style a call id belongs to — plus a serialization library that pins the session shapes so a test can prove a Frameless body never grows a top-level `type`. `RealtimeV2` is modeled to the extent that it must be *rejected* for WebRTC, not implemented as a transport.

Anything in `000` §3.4 (RealtimeV2 session internals), §6.3 (standalone sequences), and §7 (event literals, in the linked research) is reference material for a future client, not a build target here.

## D2. Crate stack

Resolved live from crates.io on 2026-07-26:

| Crate | Version | Role |
|---|---|---|
| `axum` | 0.8 | routing, extractors, WebSocket upgrade |
| `tokio` | 1 | runtime |
| `tokio-util` | 0.7 | `CancellationToken` for the call-create cancellation regime (`020`) |
| `tokio-tungstenite` | **0.29** | upstream WebSocket client |
| `reqwest` | 0.13 | upstream HTTP client |
| `multer` | 3.1 | multipart parse for the rewrite path |
| `subtle` | 2 | constant-time admission-credential comparison (`015`) |
| `serde` / `serde_json` | 1 | session JSON modeling |
| `http` / `bytes` | 1 | header and byte primitives |
| `tracing` / `tracing-subscriber` | 0.3 | structured logging |
| `thiserror` | 2 | error taxonomy |

**Version pin rationale.** `axum` 0.8.9 depends on `tokio-tungstenite` 0.29 (verified: `cargo tree -i tungstenite` on a probe crate shows a single `tungstenite v0.29.0`). Adding `tokio-tungstenite` 0.30 would link **two** tungstenite versions with incompatible `Message` types. The workspace therefore pins 0.29 to keep exactly one tungstenite in the graph.

Even at a matching version the two sides do **not** share a message type: `axum::extract::ws::Message` and `tungstenite::protocol::Message` are distinct enums, and only the latter has a `Frame` variant. An explicit conversion module is required in both directions, covering text, binary, ping, pong, and close (code plus reason). That conversion is a named deliverable of `030`, not an assumption.

`multer` 3.1 parses from a stream plus an explicitly extracted boundary. Because the body is already buffered under the 16 MiB cap before the rewrite, the port feeds it a single-chunk stream — buffered, not streaming. That is fine at this cap, but the plan states it rather than implying a streaming rewrite.

`tokio-util` and `subtle` are **direct** dependencies with entries in `Cargo.toml`. Being reachable transitively through another crate does not make a type importable, and a hand-rolled constant-time comparison is not offered as an interchangeable fallback on a security-sensitive path.

## D3. Faithful, not "improved"

Deliberate quirks preserved from the TypeScript implementation, because a relay that "fixes" them stops being wire-compatible:

- Only `content-type` and `location` come back from the upstream response; every other header is dropped.
- Close propagation is asymmetric: upstream → client preserves code and reason, client → upstream is normalized to `1000` / `client closed`.
- The pre-open queue is bounded by **frame count** (32), not bytes.
- There is no post-open backpressure accounting and no upstream connect timeout in the Live path.
- The `499` status is emitted for client cancellation even though it is not an IANA status.

Each of these has a regression test so a future refactor cannot silently drift.

## D4. Session modeling is typed, and the type-lessness is enforced

The single most expensive defect in the source record was a Frameless session being validated as V1 because a header was dropped. The Rust port encodes this structurally:

- `WireAdapter::{V1, FramelessBidi, RealtimeV2}` owns the `openai-alpha` value, the call-create path shape, the sideband join style, and whether a `session.update` follows a join.
- `FramelessSession` has **no** `type` field to serialize, so it cannot accidentally acquire one.
- `V1Session` always serializes `"type": "quicksilver"`.
- `strip_session_id` runs on every call-create body regardless of adapter.

## D5. Credentials never touch disk or logs

Three independent mechanisms, because a newtype alone is not enough:

1. `BearerToken`'s `Debug` prints `Bearer <redacted>`, protecting config-level `{:?}`.
2. Once a token becomes an `authorization` value inside a `HeaderMap`, the newtype no longer protects it — so every credential-bearing header value is constructed with `HeaderValue::set_sensitive(true)`, and **logging a whole `HeaderMap` is prohibited**. A helper `redacted_headers(&HeaderMap)` is the only sanctioned way to render headers, and it replaces the value of any sensitive or known-credential header with `<redacted>`.
3. The frame-forensics writer records only direction, kind, byte length, a U+FFFD flag, and — solely when a replacement character is present — a bounded excerpt.

Config accepts secrets from the environment only; no file in the repository contains a real token shape.

**Scope of this guarantee.** It covers *credentials the relay handles*: bearer tokens, account identifiers, and the admission secret. It does not extend to arbitrary text a client chooses to put inside a WebSocket frame. If a caller writes a token into frame content and that content is corrupted, the opt-in frame log can capture it in the bounded excerpt around the corruption — see `050` for the precise statement. The three layers above make it impossible for the relay to leak a credential it manages; they cannot make it impossible for a client to leak its own.

## D6. Environment variable naming

OpenCodex uses `OCX_LIVE_FRAME_LOG`. The standalone service uses `GPT_LIVE_FRAME_LOG`, with `OCX_LIVE_FRAME_LOG` accepted as a compatibility alias so an existing diagnostic workflow keeps working.
