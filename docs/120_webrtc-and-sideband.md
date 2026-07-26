# 120 — WebRTC call-create and sideband compatibility

Work-phase: `wp4-webrtc-sideband`. Depends on `090`, `100`, and `110`.

## Runtime matrix

| Profile/dialect | Inbound route | Upstream | Query | Body |
|---|---|---|---|---|
| official API key GA | `/v1/realtime/calls` | `/v1/realtime/calls` | preserve only inbound official query | multipart or raw SDP unchanged |
| API key V1 | private V1 selection | `/v1/realtime/calls` | `intent=quicksilver&architecture=avas` | multipart |
| API key Frameless | `/v1/live` + private header | `/v1/live` | none | multipart |
| ChatGPT V1/Frameless | private selection | backend `/realtime/calls` | AVAS where source requires | multipart rewritten to JSON |
| ChatGPT GA V2 | `/v1/realtime/calls` + GA session | unsupported | none | reject before contact |

The current Codex snapshot rejects RealtimeV2 only on its private AVAS/ChatGPT
path. The current public OpenAI API supports `gpt-realtime-2.1` WebRTC; API-key
GA mode must not inherit that private rejection.

## File changes

### VERIFY `src/realtime/http.rs`

`100` already owns official create-call, including raw-SDP ephemeral and
multipart managed/client credential policy. This phase does not defer or rewrite
that route; its end-to-end WebRTC test proves the `100` result composes with the
sideband implementation.

### MODIFY `src/live/call_create.rs`, `src/live/url.rs`, `src/live/headers.rs`

Consume `ProtocolSelection` for private routes. Remove the `keyed` shortcut that
currently sends every API-key profile to AVAS. Keep multipart→backend JSON and
top-level ID removal only for the body the proxy rebuilds.

### MODIFY `src/realtime/websocket.rs` and private sideband adapter

Official GA sideband is exactly `/v1/realtime?call_id=...`; V1 alone adds
Quicksilver intent; Frameless uses `/v1/live/{id}`. Returned `Location` remains
opaque downstream, while tests extract its call ID and prove a following join.

### MODIFY `tests/call_create.rs`, `tests/sideband.rs`

Correct false-confidence expectations: API-key Frameless direct URL and GA
sideband query must assert source-authoritative paths. Retain private regression
coverage.

### NEW `tests/official_webrtc.rs`

End-to-end mock flow: multipart/raw offer → exact upstream request → `201` SDP +
`Location` → extract call ID → sideband join → GA event echo. Matrix-test every
row above and prove unsupported ChatGPT GA rejects before contact.

## Verification

```bash
cargo test --test official_webrtc
cargo test --test call_create --test sideband
cargo test --all-features
```
