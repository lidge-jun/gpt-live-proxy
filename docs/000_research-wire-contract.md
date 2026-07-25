# 000 — GPT-Live / OpenAI Realtime wire contract (research)

Research notes. Two sources, cited as `S1` and `S2`:

- `S1` — an architecture record of the upstream `codex-rs` realtime stack: the
  three wire adapters, the call-create contract, session shapes, and the
  sideband WebSocket. Reconstructed from `openai/codex` at the tag noted in that
  record.
- `S2` — the 2026-07-24 OpenCodex GPT-Live repair record, covering the two
  defects fixed by commits `75344b09` and `3b766d91`.

Both are private engineering notes, so the `S1:line` / `S2:line` citations below
are provenance markers rather than links a reader can follow. What they point at
is reproducible from public sources: `openai/codex` for the protocol and
`lidge-jun/opencodex` for the relay behavior. Literals are quoted verbatim so
the claims stand on their own.

## 1. Two planes

The WebRTC path is not one connection. It is two planes (`S1:12-19`, `S1:48-70`):

1. The browser/WebView owns `RTCPeerConnection`, the microphone track, the remote audio track, and the `oai-events` data channel.
2. The host performs an HTTP **call-create** and receives an answer SDP plus a call id.
3. The host joins a separate **sideband WebSocket** keyed by that call id for session/control, context, and delegation events.

Offer construction order is `getUserMedia({ audio: true })` → `addTrack()` → `createDataChannel("oai-events")` → `createOffer()` → `setLocalDescription()` (`S1:73-77`). With an SDP present, call-create runs and the sideband task starts; without an SDP, the host connects a standalone Realtime WebSocket and skips call-create entirely (`S1:79-107`).

## 2. call-create HTTP contract

### 2.1 Endpoint selection

The default logical path is `realtime/calls` (`S1:265-269`). The body shape and the path both switch on whether the provider base URL contains the literal substring `/backend-api` (`S1:271-284`):

| Base kind | Adapter | Path | Body |
|---|---|---|---|
| non-backend | `FramelessBidi` | `live` | multipart `sdp`, `session` |
| non-backend | `V1` | `realtime/calls` | multipart `sdp`, `session` |
| non-backend | `RealtimeV2` | (logically `realtime/calls`) | rejected before the request |
| contains `/backend-api` | `V1` or `FramelessBidi` | `realtime/calls` | JSON `{ "sdp": ..., "session": ... }` |

(`S1:286-293`)

Concrete endpoints:

```text
ChatGPT backend (V1 and Frameless alike):
https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas

OpenAI API key (Realtime/V1):
https://api.openai.com/v1/realtime/calls?intent=quicksilver&architecture=avas

Direct API Frameless:
https://api.openai.com/v1/live        (no intent/architecture query)
```

(`S2:1040-1058`, `S1:354-359`)

### 2.2 AVAS query rule

The query pair is appended exactly when the parser is `V1`, **or** when the backend request shape is in use and the parser is `FramelessBidi` (`S1:340-352`). Order is `intent=quicksilver` then `architecture=avas` (`S1:354-359`). `RealtimeV2` is rejected before the request with `AVAS realtime calls require realtime v1 or v3` (`S1:361-363`).

### 2.3 Non-backend multipart body

Boundary is the fixed literal `codex-realtime-call-boundary` (`S1:295-299`), so the request content type is exactly:

```text
multipart/form-data; boundary=codex-realtime-call-boundary
```

(`S1:867-873`)

Part order is fixed — `sdp` first with `Content-Type: application/sdp`, then `session` with `Content-Type: application/json`, terminated by `--codex-realtime-call-boundary--\r\n` (`S1:301-315`).

### 2.4 Backend JSON body

```json
{ "sdp": "<SDP string>", "session": <session JSON object> }
```

(`S1:320-338`). The OpenCodex relay additionally permits an SDP-only body `{ "sdp": "v=0..." }` when the client sent no session part (`S2:1082-1084`).

### 2.5 Response

The response body is the answer SDP, not JSON. The call id lives in the `Location` header, never in the body (`S1:733-738`). A relay must therefore preserve `content-type` and `location` (`S2:1113-1125`). Observed shape:

```text
Location: /v1/realtime/calls/rtc_u0_E52xSxAjvyO0yAcpamyDl
```

(`S2:521-530`)

### 2.6 Location parsing

Split on `?`, keep the path, scan segments from the right, take the first valid call-id segment (`S1:740-754`). A segment is valid when it starts with `rtc_` and has a non-empty suffix, or when it is a 36-character `8-4-4-4-12` hex UUID (`S1:756-762`). A missing header is the error `realtime call response missing Location` (`S1:733-746`).

## 3. The three wire adapters

| Aspect | V1 / Quicksilver | FramelessBidi / v3 / GPT-Live | RealtimeV2 |
|---|---|---|---|
| App-server version | `v1` | `v3` | `v2` |
| Default model | `gpt-realtime-1.5` | `gpt-live-1-boulder-alpha` | `gpt-realtime-1.5` |
| Negotiation header | `openai-alpha: quicksilver=v1` | `openai-alpha: quicksilver=v2` | none |
| Session top-level `type` | `"quicksilver"` | absent | `"realtime"` or `"transcription"` |
| Standalone WS path | `/v1/realtime` | `/v1/live` | `/v1/realtime` |
| Sideband join | query `call_id=` | path `/live/{call_id}` | query `call_id=` |
| Input audio event | `input_audio_buffer.append` | `input_audio.append` | `input_audio_buffer.append` |
| Text injection | `conversation.item.create` | `session.context.append` | `conversation.item.create` |
| Delegation | `conversation.handoff.*` | `delegation.created`, `delegation.context.append` | `background_agent` function call |
| Initial history | separate create message | `initial_items` in call-create session | separate create message |
| `session.update` after WebRTC join | sent | **not sent** | WebRTC itself rejected |

(`S1:170-188`). Version mapping is `v1 → V1`, `v2 → RealtimeV2`, `v3 → FramelessBidi` (`S1:28-33`). WebRTC allows only `V1` and `FramelessBidi` (`S1:190-196`).

### 3.1 V1 session

A typed session serialized with snake_case, so the wire JSON carries `"type": "quicksilver"` (`S1:437-465`).

### 3.2 Frameless session

A separate builder, not a modified Realtime session (`S1:369-420`):

```json
{
  "instructions": "<instructions>",
  "audio": { "output": { "voice": "<voice>" } },
  "delegation": { "type": "client" }
}
```

`model` is added only when present; `initial_items` only when non-empty (`S1:401-420`). There is **no** top-level `type`; the `type` inside `delegation` is not a session type, and adding `"type": "quicksilver"` to a Frameless body corrupts the Frameless contract into the V1 contract (`S1:398-400`, `S2:259-284`). The exact JSON pinned by the upstream test is:

```json
{"audio":{"output":{"voice":"cove"}},"delegation":{"type":"client"},"instructions":"backend prompt\n\nstartup context","model":"gpt-live-1-boulder-alpha"}
```

(`S1:846-853`)

`initial_items` entries are `{ "type": "message", "role": <role>, "content": [...] }` where `user`/`developer` map to `input_text` and `assistant` maps to `output_text` (`S1:422-430`). Limits: 128 items, 8,192 estimated tokens per item and in total (`S1:432-435`).

### 3.3 RealtimeV2 session

Conversational form carries `"type": "realtime"`, 24 kHz PCM, near-field noise reduction, `gpt-4o-mini-transcribe` input transcription, server VAD, and the `background_agent` / `remain_silent` function tools (`S1:467-472`). Transcription form carries `"type": "transcription"` and drops output and tools (`S1:473-475`).

### 3.4 `session.remove("id")`

Regardless of adapter, the call-create body builder removes the top-level `id` from the session object (`S1:482-493`). The identity headers stay on the request; only the session JSON loses its `id` (`S1:495-497`).

## 4. Header layer

| Header | Meaning |
|---|---|
| `openai-alpha` | wire protocol negotiation |
| `x-session-id` | upstream Realtime session id |
| `session-id` | Codex session / conversation id |
| `thread-id` | Codex thread id |
| `originator` | thread-scoped originator override (process default `codex_cli_rs`) |
| `authorization` | API key or ChatGPT bearer |
| `chatgpt-account-id` | ChatGPT account / workspace routing |
| `x-openai-fedramp` | FedRAMP account routing |
| `x-oai-attestation` | host-generated thread attestation |

(`S1:501-519`, `S1:578-580`)

The three identifiers are distinct and must never be collapsed into one (`S1:1045-1050`). Attestation is optional and only generated when both `include_attestation` and a provider are present (`S1:591-594`).

`openai-alpha` values are exactly `quicksilver=v1` for V1, `quicksilver=v2` for FramelessBidi, and absent for RealtimeV2 (`S1:521-535`). Public version `v3` and the negotiated `quicksilver=v2` are two different version axes and are not a contradiction (`S1:1011-1016`). Live proof: Frameless session plus `openai-alpha: quicksilver=v2` returned `201 Created`; removing only that header returned `403 Voice session access denied` (`S2:506-543`).

### 4.1 The six-header whitelist (OpenCodex `75344b09`)

```text
openai-alpha
x-session-id
session-id
thread-id
originator
x-oai-attestation
```

(`S2:346-359`). Only non-empty values are forwarded; no blanket `x-*` copying; the proxy never invents a missing protocol header (`S2:361-365`, `S2:448-463`).

Merge order is client protocol headers → provider static headers → selected account/API-key auth, so auth always wins on conflict (`S2:367-383`). `authorization` and `chatgpt-account-id` are proxy-owned and never copied from the client (`S2:378-389`). Client-supplied `x-openai-fedramp` is never forwarded, because it is an account-derived compliance claim and the pool may have selected a different account than the caller assumed (`S2:422-444`). The same policy applies to both call-create and the sideband upgrade (`S2:1086-1096`).

CORS `Access-Control-Allow-Headers` must include `OpenAI-Alpha`, `X-Session-Id`, `Session-Id`, `Thread-Id`, `Originator`, `X-OAI-Attestation` alongside the pre-existing `ChatGPT-Account-Id` (`S2:392-413`).

## 5. Sideband WebSocket

### 5.1 Join URL

Frameless appends the call id as a path segment; V1 and RealtimeV2 append it as a `call_id` query pair (`S1:634-648`). Canonical API-host forms (`S1:650-655`):

```text
wss://api.openai.com/v1/realtime?intent=quicksilver&call_id=rtc_...
wss://api.openai.com/v1/live/rtc_...
```

OpenCodex local-to-upstream mapping (`S2:668-685`):

| Local | Upstream |
|---|---|
| `/v1/live/{id}` | `wss://api.openai.com/v1/live/{id}` |
| `/v1/realtime/calls/{id}` | `wss://api.openai.com/v1/realtime/calls/{id}` |
| `/v1/realtime?call_id={id}` | `wss://api.openai.com/v1/realtime?intent=quicksilver&call_id={id}` |

### 5.2 Why the join host is the API host

Upstream selects the WebSocket base with `provider.to_api_provider(Some(AuthMode::ApiKey))`, whose default base is `https://api.openai.com/v1` (`S1:661-686`). `AuthMode::ApiKey` here is a **URL base selection rule**, not a credential switch: the sideband reuses the same `api_auth`, so ChatGPT bearer plus account id are sent to the API host (`S1:688-700`).

OpenCodex defect 2 was exactly this confusion: it converted the ChatGPT provider base into `wss://chatgpt.com/backend-api/codex/{callId}`, which is not a sideband endpoint. All four backend-host candidates failed to OPEN and closed `1002`; the same auth headers against `wss://api.openai.com/v1/live/{callId}` opened successfully (`S2:599-647`). Fix `3b766d91` introduced `LIVE_SIDEBAND_API_ROOT = "https://api.openai.com/v1"` and changed only sideband URL construction (`S2:668-695`, `S2:720-737`).

### 5.3 No `session.update` after a Frameless join

Standalone connects with `initialize_session = true`; a WebRTC sideband connects with `false` (`S1:702-706`). The update is sent when `initialize_session || parser != FramelessBidi` (`S1:708-719`), which yields:

- standalone V1 / Frameless / RealtimeV2 → send `session.update`;
- WebRTC V1 sideband → still sends it (not Frameless);
- WebRTC Frameless sideband → **never** sends it, because the session was already created and started by the call-create body (`S1:721-731`).

## 6. End-to-end sequences

### 6.1 Frameless v3 WebRTC

Offer → `thread/realtime/start {version: v3, sdp}` → Frameless session JSON (no top-level `type`, `delegation.type = "client"`, optional `model` / `initial_items`) → strip `id` → call-create (direct `/v1/live` multipart, or backend `realtime/calls?intent=quicksilver&architecture=avas` JSON) with `openai-alpha: quicksilver=v2` → answer SDP + `Location` → parse call id → `wss://api.openai.com/v1/live/{call_id}` with the same header set → no `session.update` → `session.started` → answer SDP back to the browser (`S1:939-978`).

The upstream V3 end-to-end test pins: public version `V3`, call-create `/v1/live`, the fixed boundary, both parts, absence of a top-level `type`, sideband `/v1/live/rtc_e2e`, `openai-alpha: quicksilver=v2`, and an empty outbound frame buffer right after join (`S1:823-844`).

### 6.2 V1 WebRTC

`/v1/realtime/calls?intent=quicksilver&architecture=avas` with `openai-alpha: quicksilver=v1` and multipart body → answer SDP + `Location` → `wss://api.openai.com/v1/realtime?intent=quicksilver&call_id={id}` → first outbound frame is a V1 `session.update` (`S1:980-995`, `S1:855-865`).

### 6.3 Standalone WebSocket

No SDP → no call-create → direct connect per adapter (`/v1/realtime` for V1 and RealtimeV2, `/v1/live` for Frameless) with `initialize_session = true`, so every adapter sends `session.update`; Frameless then waits for `session.started` (`S1:997-1005`, `S1:628-632`).

## 7. Error discrimination table

| Observation | Interpretation |
|---|---|
| `/v1/live` JSON `404` | the server route does not accept call-create |
| `400 Field session must be an object` | the backend is validating against an unexpected contract |
| `400` demanding `session.type` | fell back to V1 quicksilver validation — restore `openai-alpha` |
| `403` with no `openai-alpha` | Frameless protocol/access contract unmet |
| `session.model not allowed` | a model allow-list issue, not a header issue |
| local `http_101` then `1011` | downstream upgraded, upstream connect failed |
| backend candidate close `1002` | wrong WebSocket endpoint or protocol shape |
| API host `OPEN` | sideband host choice is correct |
| `session.started` | upstream control channel actually started |
| close `1000` | normal termination |

(`S2:1356-1369`)

A downstream `101` alone never proves end-to-end success. The minimum success chain is downstream upgrade → upstream OPEN → a valid first frame (`session.started`) → bidirectional relay → the intended close code (`S2:1150-1161`, `S2:1230-1245`).
