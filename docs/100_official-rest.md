# 100 — Official Realtime REST compatibility

Work-phase: `wp2-official-rest`. Depends on committed `090`.

## Scope and activation boundary

Add all nine OpenAPI Realtime HTTP paths from `083` plus the guide-derived
translation call path. API-key profiles are opaque official relays. ChatGPT and
recognized private dialects use this pre-body dispatch matrix:

| Profile/dialect | Dispatch |
|---|---|
| `ApiKeyManaged` or `ApiKeyClient` + `OfficialGa` | official bounded relay |
| `ChatGptBackend` + `OfficialGa` | stable unsupported-capability error; zero upstream contact |
| managed profile + recognized `QuicksilverV1`/`Frameless` on call create | legacy `live::handle_call_create` until `120` |
| `ApiKeyClient` + private dialect | contract error; zero upstream contact |
| any non-call-create official operation + ChatGPT/private dialect | unsupported-capability error; zero contact |
| `/v1/live` | unchanged legacy owner |

The dispatch happens before body read. Official URL, body, credential, status,
and response-header activation moves atomically; no request gets a mixture of
official auth with a private AVAS URL.

## One route and path authority

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

pub fn parse_rest_path(raw_path: &str) -> Result<RestOperation, PathError>;
pub fn validate_call_id(decoded: &str) -> Result<(), PathError>;

pub enum PathError { UnknownRoute, InvalidCallId }
```

`parse_rest_path` is the only raw REST path table. It percent-decodes the one
call-ID segment exactly once and rejects malformed escapes, decoded slash,
empty/non-ASCII/punctuation IDs, and IDs over 128 bytes. The fixed actions are
not decoded into dynamic dispatch.

### MODIFY `src/realtime/contract.rs`

```rust
pub struct ClassifiedRest {
    pub operation: RestOperation,
    pub selection: ProtocolSelection,
}

pub fn classify_rest(facts: &RouteFacts<'_>)
    -> Result<ClassifiedRest, RestContractError>;

pub enum RestContractError {
    UnknownRoute,
    MethodNotAllowed,
    InvalidCallId,
    UnsupportedContentType,
    PrivateDialectRequiresManaged,
    PrivateDialectNotSupported,
}
```

`classify_rest` calls `parse_rest_path` once, losslessly maps `PathError` into
`RestContractError`, applies method/content/dialect/credential rules, and
returns operation plus selection. Existing `classify` delegates REST facts to
it and maps the result/error into the broader `ContractError`; add
`ContractError::InvalidCallId`. Delete
`rest_session_kind`/`control_session_kind` so no second path table remains.

### MODIFY `src/live/sideband.rs`

Move the canonical call-ID character/length check to `realtime::path`.
Sideband keeps transport-specific target parsing and percent decoding but calls
the shared validator. Parity tests prove HTTP controls and WS joins accept and
reject the same decoded IDs.

### MODIFY `src/realtime/mod.rs`

Declare `http` and `path`. No future `capability` module is imported.

## Protocol-neutral HTTP exchange

### NEW `src/relay/http.rs` and MODIFY `src/relay/mod.rs`

```rust
pub struct OpaqueResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub enum ExchangeTerminal { InFlight, Completed, Failed, TimedOut, Canceled }
#[derive(Clone)]
pub struct ExchangeLifecycle { /* first-writer slot + cancellation token */ }
pub struct ExchangeGuard { /* cancel-on-drop while slot is InFlight */ }

pub fn begin_exchange() -> (ExchangeLifecycle, ExchangeGuard);

pub fn spawn_execute(
    client: reqwest::Client,
    request: reqwest::Request,
    response_cap: usize,
    upstream_timeout: Duration,
    lifecycle: ExchangeLifecycle,
    permit: OwnedSemaphorePermit,
) -> JoinHandle<Result<OpaqueResponse, RelayError>>;
```

`ExchangeLifecycle::finish` is first-writer-wins. Each handler calls
`begin_exchange` before body reading and reports body/read failures through the
same lifecycle. `spawn_execute` receives a lifecycle clone and an
`OwnedSemaphorePermit`, then spawns
the entire send/read lifecycle, accepts the request's arbitrary method, applies
the upstream deadline, and buffers response chunks only up to the cap. The task
owns the permit until it actually exits, even if the handler future is dropped.
It returns raw upstream headers; legacy and official handlers apply their own
allowlists.

The spawned task records `Completed`, `Failed`, or `TimedOut` before returning
its result. `ExchangeGuard::drop` records `Canceled` and cancels the token only
if the slot is still `InFlight`. A task-completed/handler-drop race therefore
cannot be relabeled as client cancellation.

### MODIFY `src/live/call_create.rs`

Delete private `CallOutcome`/`OutcomeGuard`/`spawn_upstream`. Call
`begin_exchange` before body read, acquire the shared HTTP permit after cheap
dispatch validation, pass the lifecycle and owned permit to
`relay::http::spawn_execute`, and map the shared terminal enum into Live span
labels. Retain only Live-specific body rewrite, URL/header policy, and
two-header response filtering. Cancellation, terminal outcome, timeout, cap,
and spawned-task behavior have one owner.

## Official handler and URL

### NEW `src/realtime/http.rs`

```rust
pub async fn handle(
    State<AppState>,
    Method,
    OriginalUri,
    HeaderMap,
    Body,
) -> Response;

pub fn official_url(base: &str, original: &Uri)
    -> Result<String, RelayError>;
```

Handler order:

1. `classify_rest` once and apply the dispatch matrix;
2. profile capability and credential/header validation;
3. call `begin_exchange` for the shared lifecycle/guard;
4. acquire an HTTP permit with `try_acquire_owned`;
5. read body under request-read timeout and byte cap;
6. build raw-preserving official URL and request headers;
7. pass lifecycle plus owned permit to `relay::http::spawn_execute`;
8. let the spawned task record terminal outcome before it returns;
9. relay exact status/body and `realtime::headers::response_headers`.

API-key JSON, multipart, SDP, and empty bodies are opaque. OpenAI remains the
schema validator; all upstream 4xx/5xx statuses and binary body bytes survive.

`official_url` uses the validated configured base and raw `OriginalUri`
path/query. It trims only trailing base slashes; if the base ends in `/v1`, it
removes exactly one inbound `/v1` prefix, otherwise it appends the full inbound
path. It appends `path_and_query().as_str()` without parsing/rebuilding query
pairs. Tests pin duplicate keys, blank values, `+`, `%2B`, encoded UTF-8,
ordering, no-query routes, and bases at root/`/v1`/custom prefix. No private
AVAS or intent query can enter this builder.

## Exact protected route map

### MODIFY `src/app.rs`

Use `any(crate::realtime::http::handle)` so the proxy, not Axum, owns wrong
method errors:

```text
/v1/realtime/calls
/v1/realtime/calls/{call_id}/accept
/v1/realtime/calls/{call_id}/reject
/v1/realtime/calls/{call_id}/refer
/v1/realtime/calls/{call_id}/hangup
/v1/realtime/client_secrets
/v1/realtime/sessions
/v1/realtime/transcription_sessions
/v1/realtime/translations/client_secrets
/v1/realtime/translations/calls
```

There is no wildcard. Wrong method on a registered path is the existing
`404 Unknown endpoint: {method} {path}`. Trailing slash, unknown action, extra
segment, and empty ID fall through to the same 404. A matched control route with
malformed percent escape, decoded slash, invalid decoded characters, or
overlength ID returns the exact 400 below before upstream contact.

## Limits and permits

### MODIFY `src/config.rs`, `src/app.rs`

Add `Limits.active_requests`, environment
`GPT_LIVE_MAX_REQUESTS`, default 128. Keep `active_connections` for later
WebSockets. `AppState` owns `Arc<Semaphore>` initialized from
`active_requests`. Configuration rejects request or connection permit counts
above `tokio::sync::Semaphore::MAX_PERMITS`, so an oversized environment value
cannot defer a panic into `AppState` startup.

Cheap route/method/path/profile/header validation precedes permit acquisition.
The shared exchange guard also precedes body read. The permit precedes body read
and moves into the spawned exchange. A rejected max+1 request returns 429 with
zero body read/upstream contact; client disconnect during body read releases
the handler-owned permit immediately, while disconnect after spawn cancels the
task and the task releases its permit only when it exits. Body error/timeout,
upstream error/timeout, over-cap response, and normal completion all return the
count to baseline. Both official and legacy HTTP handlers use this order. WP6
verifies/soaks these controls; it does not reimplement them.

## Exact proxy-originated errors

### MODIFY `src/error.rs`

Every new variant enters `contract_rows`, discriminant coverage, and rendered
response tests:

| Condition | Status | Message | type | code | Extra header |
|---|---:|---|---|---|---|
| wrong method/unmatched shape | 404 | existing dynamic `Unknown endpoint: {method} {path}` | `invalid_request_error` | `invalid_request_error` | — |
| invalid decoded call ID | 400 | `invalid Realtime call_id` | `invalid_request_error` | `invalid_call_id` | — |
| unsupported content type | 400 | `unsupported Realtime content type` | `invalid_request_error` | `invalid_request_error` | — |
| unsupported profile/capability | 400 | `Realtime operation is not supported by the configured upstream profile` | `invalid_request_error` | `unsupported_realtime_capability` | — |
| request-read timeout | 408 | `Realtime request body timed out` | `invalid_request_error` | `request_timeout` | — |
| HTTP permit exhausted | 429 | `too many active Realtime requests` | `rate_limit_error` | `rate_limit_exceeded` | `Retry-After: 1` |
| missing/malformed required bearer | 401 | existing `NoCredential` row | `authentication_error` | `invalid_api_key` | — |
| repeated/local invalid header | 400 | existing `AmbiguousAuthorization`/`InvalidRealtimeHeader` | `invalid_request_error` | existing | — |
| request/response cap, cancel, upstream timeout/transport | existing rows | existing exact messages | existing | existing | existing |

Upstream responses are never rewritten into these variants.

The one exhaustive `RestContractError` conversion is:

| REST contract error | Wire error |
|---|---|
| `UnknownRoute`, `MethodNotAllowed` | dynamic existing `UnknownEndpoint` |
| `InvalidCallId` | `InvalidRealtimeCallId` |
| `UnsupportedContentType` | `UnsupportedRealtimeContentType` |
| `PrivateDialectRequiresManaged`, `PrivateDialectNotSupported` | `UnsupportedRealtimeCapability` |

Direct mapping tests and rendered-response tests prove no handler-side second
path parse is needed.

## Credential/body activation matrix

| Operation/body | Managed API key | Client API key |
|---|---|---|
| create call multipart | configured bearer | caller bearer |
| create call raw `application/sdp` | caller ephemeral bearer | caller ephemeral bearer |
| translation call raw SDP | caller ephemeral bearer | caller ephemeral bearer |
| bootstrap/control JSON | configured bearer | caller bearer |
| hangup empty body | configured bearer | caller bearer |
| any official operation on ChatGPT | unsupported, zero contact | unrepresentable profile |
| private call-create dialect | legacy managed handler | contract error, zero contact |

Tests assert the winning bearer and canary absence, plus zero contact for
missing/malformed/repeated bearer and admission-secret crossover.

## Test changes

### MODIFY `tests/call_create.rs`

Replace WP1's client-mode 401 canary with official success; update API-key URL
to no AVAS; move private regression rows to explicit alpha/`/v1/live`; make
the redirect canary use valid multipart or raw SDP. Preserve ChatGPT rewrite.

### MODIFY `tests/support/mod.rs`

Capture raw URI/method/exact headers/body; support arbitrary binary
status/body/header responses, response stall/reset, request-start/drop signals,
and deterministic barriers.

### NEW `tests/official_rest.rs`

Real-socket table covers every operation and credential/body row. It asserts:

- exact raw upstream URL/query, header map, body bytes, status, safe response
  headers, and opaque/binary body;
- wrong method and every route/call-ID boundary with zero contacts;
- request body empty, exact cap, cap+1; response exact cap and cap+1;
- slow partial raw-TCP body read → 408;
- disconnect during partial inbound body → guard fires and permit releases;
- downstream disconnect during upstream response → body dropped, task exits,
  then spawned-task permit releases;
- deterministic exchange-completes/handler-drops race → first terminal outcome
  stays `Completed`, never `Canceled`;
- upstream 400/401/429/500 binary bodies, reset, and stall;
- max permits plus max+1 → 429/`Retry-After: 1`, then recovery;
- all missing/malformed/repeated/crossover credential negatives.

The translation-call row is tagged `GuideDerivedTranslationCall`; its status
and safe-header oracle is guide-derived, not claimed as OpenAPI-derived, until
a later `083` snapshot includes it.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --test official_rest
cargo test --test call_create
cargo test --all-features
cargo +1.86 check --locked --all-targets --all-features
```
