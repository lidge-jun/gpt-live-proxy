# 100 — Official Realtime REST compatibility

Work-phase: `wp2-official-rest`. Depends on `090`.

## Scope

Add every official Realtime REST operation from `080`, plus the documented
translation call path. The API-key profile is transparent. The ChatGPT profile
is gated by `130` capabilities and must not mint fake credentials or SIP state.

## File changes

### NEW `src/realtime/http.rs`

One bounded opaque HTTP relay:

```rust
pub async fn handle(
    State<AppState>, Method, OriginalUri, HeaderMap, Body,
) -> Response;
```

Flow: classify route → enforce method and profile capability → install the
existing outside-handler cancellation ownership → read under request-read
deadline/cap → build exact upstream URL with ordered query preservation → build
headers from `realtime::headers` → send using the inbound method → buffer the
bounded non-streaming response → relay status, body, and safe response headers.

No endpoint-specific JSON deserialization occurs in API-key mode. OpenAI remains
the schema validator and its error body/status are preserved.

### NEW `src/realtime/path.rs`

```rust
pub enum RestOperation {
    CreateCall,
    AcceptCall { call_id: String },
    RejectCall { call_id: String },
    ReferCall { call_id: String },
    HangupCall { call_id: String },
    CreateClientSecret,
    CreateLegacySession,
    CreateTranscriptionSession,
    CreateTranslationClientSecret,
    CreateTranslationCall,
}
```

Call IDs reuse the one canonical validator moved out of `live/sideband.rs`.
Path decoding rejects malformed escapes, slash injection, empty/overlength IDs,
and extra segments before any upstream contact.

### MODIFY `src/app.rs`

Add exact protected routes. Control paths use explicit route registration; no
catch-all `/v1/realtime/*` proxy is allowed.

```text
POST /v1/realtime/client_secrets
POST /v1/realtime/sessions
POST /v1/realtime/transcription_sessions
POST /v1/realtime/translations/client_secrets
POST /v1/realtime/translations/calls
POST /v1/realtime/calls
POST /v1/realtime/calls/{call_id}/{accept|reject|refer|hangup}
```

`POST /v1/realtime/calls` moves from the private baseline handler to this
official handler in the same phase. API-key multipart uses the exact public URL
without AVAS query injection; raw SDP preserves the client ephemeral bearer.
Private GPT-Live aliases remain on `live::call_create` until `120` aligns their
explicit protocol selection.

### MODIFY `src/error.rs`

Add only proxy-originated errors: invalid route/method, capability unsupported,
request-read timeout, active-request limit, and invalid credential policy. Every
upstream error response is opaque and is not rewritten into `RelayError`.

### MODIFY `tests/support/mod.rs`

Capture ordered request path/query, method, exact headers and body; allow
arbitrary status/body/headers and deterministic stalled/read-reset behaviors.

### NEW `tests/official_rest.rs`

Table-driven real-socket matrix for all ten paths. Each row asserts method,
exact URL, exact upstream header map, body bytes, downstream status, safe
headers, and body. Negative cases cover wrong method, malformed call ID,
limit/+1, read timeout, upstream timeout/reset, 429 + `Retry-After`, cookie
stripping, and capability rejection before contact.

For `application/sdp`, the test proves an ephemeral client bearer survives in
client/ephemeral policy; multipart managed mode proves configured auth wins.

## Verification

```bash
cargo test --test official_rest
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```
