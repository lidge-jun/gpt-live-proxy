# 040 — Wire-adapter session modeling

Work-phase `wp5-session`. Deliverable: the three adapters as types, so a Frameless session cannot become a V1 session by accident.

**Ordering note.** This phase lands *before* `020` and `030`, because `WireAdapter` owns the AVAS-query rule and the sideband join style that those phases would otherwise hardcode. Build order is `010 → 015 → 040 → 020 → 030 → 050 → 060 → 070`; the decade numbers preserve document identity, not build sequence.

**Scope.** Per `002` D1b this is relay-side modeling plus a serialization library, not a Realtime client. Nothing here emits `session.update` or normalizes events. The full RealtimeV2 conversational contract (24 kHz PCM, near-field noise reduction, `gpt-4o-mini-transcribe`, server VAD, the `background_agent` / `remain_silent` tools) stays reference material in `000` §3.3.

## New files

```text
src/wire/mod.rs        WireAdapter and its behavior table
src/wire/session.rs    V1Session, FramelessSession, RealtimeV2Session, strip_session_id
src/wire/call_body.rs  multipart builder + backend JSON builder
```

## `WireAdapter`

```rust
/// Wire constants live here because `wire` is the single authority for protocol policy.
/// `020` and `030` import these rather than redeclaring them.
pub const AVAS_QUERY: &str = "intent=quicksilver&architecture=avas";
pub const SIDEBAND_API_ROOT: &str = "https://api.openai.com/v1";
pub const MULTIPART_BOUNDARY: &str = "codex-realtime-call-boundary";

pub enum WireAdapter { V1, FramelessBidi, RealtimeV2 }

impl WireAdapter {
    pub fn openai_alpha(self) -> Option<&'static str>;   // v1 => "quicksilver=v1", frameless => "quicksilver=v2", v2 => None
    pub fn default_model(self) -> &'static str;          // "gpt-realtime-1.5" / "gpt-live-1-boulder-alpha" / "gpt-realtime-1.5"
    pub fn standalone_ws_path(self) -> &'static str;     // "/v1/realtime" / "/v1/live" / "/v1/realtime"
    pub fn sideband_join(self) -> SidebandJoinStyle;     // Query / Path / Query
    pub fn wants_avas_query(self, backend_shape: bool) -> bool;  // V1 always; Frameless only when backend_shape
    pub fn sends_session_update_after_join(self, initialize: bool) -> bool;  // initialize || self != FramelessBidi
    pub fn allows_webrtc(self) -> bool;                  // RealtimeV2 => false
    pub fn from_app_server_version(v: &str) -> Option<Self>;  // v1/v2/v3
}
```

The `wants_avas_query` and `sends_session_update_after_join` signatures deliberately mirror the upstream boolean expressions so the mapping is auditable against `000` §2.2 and §5.3.

## Sessions

```rust
#[derive(Serialize)]
pub struct FramelessSession {
    // NO `type` field exists on this struct — the absence is structural.
    pub instructions: String,
    pub audio: FramelessAudio,          // { output: { voice } }
    pub delegation: Delegation,          // { type: "client" }
    #[serde(skip_serializing_if = "Option::is_none")] pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub initial_items: Option<Vec<InitialItem>>,
}

#[derive(Serialize)]
pub struct V1Session {
    #[serde(rename = "type")] pub session_type: V1SessionType,  // always Quicksilver => "quicksilver"
    #[serde(skip_serializing_if = "Option::is_none")] pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub output_modalities: Option<Vec<String>>,
    pub audio: V1Audio,
    #[serde(skip_serializing_if = "Option::is_none")] pub tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub tool_choice: Option<Value>,
}

#[derive(Serialize)]
pub struct V1Audio {
    pub input: V1AudioInput,   // { format: { type: "audio_pcm", rate } }
    #[serde(skip_serializing_if = "Option::is_none")] pub output: Option<V1AudioOutput>,  // { format?, voice }
}

#[derive(Serialize)]
pub struct RealtimeV2Session {
    // Modeled only to the depth the relay needs: a `type` discriminant plus passthrough.
    #[serde(rename = "type")] pub session_type: RealtimeV2Type,  // "realtime" | "transcription"
    #[serde(flatten)] pub rest: serde_json::Map<String, Value>,
}
```

`InitialItem::from_message(role, text)` maps `user`/`developer` → `input_text` and `assistant` → `output_text`, with `type: "message"`. Limits enforced: 128 items, 8 192 estimated tokens per item and in total, returning a typed error above either bound.

```rust
pub fn strip_session_id(session: &mut serde_json::Value);  // removes top-level "id" if the value is an object
```

## Caller integration

Each policy has exactly one authority, and `060` asserts it:

| Consumer | Uses |
|---|---|
| `020` call-create URL builder | `WireAdapter::wants_avas_query(backend_shape)` |
| `020` header merge | `WireAdapter::openai_alpha()` when the adapter is known |
| `020` `Location` handling | `parse_call_id` (tests and any future client; the relay passes the header through untouched) |
| `030` join-style derivation | `WireAdapter::sideband_join()`, cross-checked against the parsed `SidebandTarget` |
| `060` truth tables | every method on `WireAdapter` |

A function with no caller and no test is dead code and does not ship.

## Call bodies

```rust
pub fn multipart_call_body(sdp: &str, session: &Value) -> Vec<u8>;
pub fn backend_json_call_body(sdp: &str, session: Option<&Value>) -> Vec<u8>;
```

The multipart builder emits the fixed boundary, `sdp` first with `Content-Type: application/sdp`, `session` second with `Content-Type: application/json`, CRLF placement exactly as in `000` §2.3, terminated by `--codex-realtime-call-boundary--\r\n`.

## Exit criteria

Tests asserting: Frameless serialization contains no top-level `type` and matches the pinned upstream JSON string byte for byte; V1 serialization contains `"type":"quicksilver"`; `strip_session_id` removes `id` and leaves non-objects alone; `initial_items` role mapping and both limits; `openai-alpha` values per adapter; the AVAS-query truth table across `(adapter, backend_shape)`; the `session.update` truth table across `(adapter, initialize_session)`; RealtimeV2 WebRTC rejection.
