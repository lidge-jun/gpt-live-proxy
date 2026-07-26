# 130 — ChatGPT/GPT-Live capability profile

Work-phase: `wp5-chatgpt-compat`. Depends on the public transport foundation.

## Source-proven boundary

The 2026-07-23 Codex snapshot proves:

- ChatGPT auth supports V1/Frameless WebRTC call-create plus API-host sideband;
- standalone Realtime WebSocket requires an OpenAI API key;
- private RealtimeV2 WebRTC is rejected by the AVAS path;
- Frameless and public GA event/session vocabularies are not one-to-one.

Therefore this phase does not market ChatGPT auth as a full source of public
`gpt-realtime-2.1` semantics. Public routes remain available in the API-key
profile. ChatGPT adds a bounded compatibility profile for source-proven voice
flows.

## File changes

### NEW `src/realtime/capability.rs`

```rust
pub enum Capability {
    VoiceWebRtc,
    VoiceStandaloneWebSocket,
    Sideband,
    ClientSecret,
    Transcription,
    Translation,
    SipControl,
}

pub fn support(profile: &UpstreamProfile, selection: &ProtocolSelection)
    -> Support;
```

`Support` is `Native`, `Adapted`, or `Unsupported { code }`. Table tests cover
every profile/capability pair and fail if a new capability lacks a row.

### NEW `src/realtime/chatgpt.rs`

Adapt only call-create request shape and private protocol routing already proven
in `live`. Official GA session JSON is minimally discriminated at the boundary:

- type-less + `delegation.type=client` + private negotiation → Frameless;
- `type=quicksilver` + V1 negotiation → V1;
- `type=realtime|transcription` without a lossless backend capability → stable
  unsupported error before contact.

Do not translate arbitrary client/server events. A future semantic adapter would
need the Codex parser/normalizer plus inverse generation and its own roadmap; a
partial rename table would silently corrupt tool, reasoning, transcript, and
turn semantics.

### MODIFY `src/error.rs`

Add a stable profile-capability error whose body is clearly proxy-originated and
names the required `apikey` profile. It must not mimic an upstream 200/201 or
claim model access.

### MODIFY `README.md` and `docs/002_design-decisions.md`

Publish a per-profile matrix. Keep the distinction between app-server RPC v2,
public Realtime V2/GA, and `quicksilver=v2` Frameless.

### NEW/UPDATE tests

`tests/chatgpt_capabilities.rs` activates every table row, proves no unsupported
request contacts upstream, and retains the real private call-create/sideband
fixtures. Event fixtures prove opacity; there is no fake GA↔Frameless converter.

## Verification

```bash
cargo test --test chatgpt_capabilities
cargo test --test call_create --test sideband
cargo test --all-features
```
