# 110 — Official standalone, existing-call, and translation WebSockets

Work-phase: `wp3-standalone-websocket`. Depends on `090` and `100`.

This phase makes the public WebSocket surface base-URL compatible. It does not
translate public GA events into private GPT-Live events. Public frames are
opaque and byte-preserving; explicitly negotiated private V1/Frameless routes
retain their source-proven URL and downstream-first behavior.

## Authorities and corrected selector rule

Official authorities are the 2026-07-26 WebSocket, server-side controls, SIP,
translation, client-event, and server-event references inventoried by `080`.
The private authority is the local Codex snapshot recorded by `000` and `001`.

The official server-side controls contract says `model` is ignored when a
`call_id` joins an existing call. Therefore a single valid `call_id` wins even
when one or more `model` values are also present. The earlier `090` row that
rejected both selectors is corrected in this phase.

| Inbound request | Classification | Public upstream |
|---|---|---|
| `GET /v1/realtime?model=X` | standalone | exact raw path/query; no invented `intent` |
| `GET /v1/realtime?call_id=X` | existing call | exact raw path/query; no invented `intent` |
| `GET /v1/realtime?call_id=X&model=Y` | existing call | exact raw path/query; upstream ignores model |
| `GET /v1/realtime/translations?model=X` | translation | exact dedicated path/query |
| no selector | local 400 | zero upstream contact |
| duplicate `call_id` | local 400 | zero upstream contact |
| no `call_id` plus empty/duplicate `model` | local 400 | zero upstream contact |
| translation plus `call_id`, empty/duplicate model, or private alpha | local 400 | zero upstream contact |
| wrong method or non-upgrade | existing 404 policy | zero upstream contact |

The shared `[A-Za-z0-9_-]{1,128}` call-ID validator is a proxy safety policy,
not a claimed OpenAI schema rule. Unknown query keys are accepted and forwarded
in original order for forward compatibility. Malformed percent escapes or
decoded non-UTF-8 selector keys/values are rejected before contact.

Official paths do not accept a trailing slash. Existing private aliases keep
their already-tested optional trailing slash.

## File manifest

```text
MODIFY src/realtime/contract.rs             corrected selector precedence
NEW    src/realtime/query.rs                strict ordered query decoding
NEW    src/realtime/subprotocol.rs          browser protocol/auth parser
NEW    src/realtime/websocket.rs            public handler and upstream-first handshake
MODIFY src/realtime/headers.rs              WebSocket credential/header builder
MODIFY src/realtime/mod.rs                  export new modules
MODIFY src/relay/pump.rs                     separate public/private pumps and limits
MODIFY src/live/call_create.rs               carried audit fix for timeout terminal label
MODIFY src/live/sideband.rs                 explicit private entry point and policies
MODIFY src/config.rs                         checked WebSocket buffer-size bound
MODIFY src/app.rs                            dispatch and active-connection semaphore
MODIFY src/error.rs                          exact query/protocol/limit errors
MODIFY src/observability/mod.rs              hard-disable payload-bearing WS dependency logs
MODIFY src/observability/frame_log.rs        metadata-only frame diagnostics
MODIFY docs/050_observability.md             publish metadata-only record contract
MODIFY tests/sideband.rs                     private regression and byte-limit tests
MODIFY tests/call_create.rs                  activate carried timeout-label correction
MODIFY tests/forensics.rs                    trace and frame-payload canary tests
NEW    tests/official_websocket.rs           real-socket public conformance
NEW    tests/fixtures/official/realtime-events.json source-tagged event inventory
```

## Carried WP2 audit correction

Before WebSocket changes, make `RealtimeRequestBodyTimeout` map to
`ExchangeTerminal::TimedOut` in the legacy call-create `finish_error` path.
Keep its existing 408 and permit-recovery behavior, and add a focused terminal-
mapping assertion so observability cannot regress to `outcome="failed"`. This is
the only carried correction; it was found after the WP2 wire gates closed.

## Strict ordered query owner

### NEW `src/realtime/query.rs`

```rust
pub fn decode_ordered(raw: Option<&str>)
    -> Result<Vec<(String, String)>, QueryDecodeError>;
```

Split on `&`, split each pair on the first `=`, convert `+` to space, validate
every `%` escape, then percent-decode exactly once as UTF-8. Empty keys and
values remain present. The decoded ordered pairs feed `RouteFacts`; the raw
`OriginalUri::path_and_query()` remains the sole URL-construction authority.
No `HashMap` may appear on the official classifier path because it would erase
duplicates and order.

### MODIFY `src/realtime/contract.rs`

Add the single target/selection owner, analogous to `classify_rest`:

```rust
pub enum WebSocketTarget {
    Standalone { model: String },
    ExistingCall { call_id: String },
    Translation { model: String },
}

pub struct ClassifiedWebSocket {
    pub target: WebSocketTarget,
    pub selection: ProtocolSelection,
}

pub enum WebSocketContractError {
    UnknownRoute,
    MethodNotAllowed,
    MissingSelector,
    AmbiguousQuery,
    InvalidCallId,
    PrivateDialectRequiresManaged,
    PrivateDialectNotSupported,
}

pub fn classify_websocket(facts: &RouteFacts<'_>)
    -> Result<ClassifiedWebSocket, WebSocketContractError>;
```

It extracts the owned model/call ID and `ProtocolSelection` from the same
decoded ordered slice. Broad `classify` delegates to it and maps
`WebSocketContractError` exhaustively into the existing broad `ContractError`;
the runtime never parses selectors a second time.

Selector ownership executes in this order:

1. more than one `call_id` -> `AmbiguousQuery`;
2. exactly one `call_id` -> validate it, then `ExistingCallWebSocket`; ignore
   all `model` values for classification;
3. no `call_id` plus exactly one non-empty `model` -> `StandaloneWebSocket`;
4. no selectors -> `MissingSelector`;
5. every other standalone model shape -> `AmbiguousQuery` or `MissingSelector`.

Translation still requires exactly one non-empty model and no call ID. Unit
tables include selector order, encoded selector names, empty values, duplicate
models with and without call ID, malformed escapes, and the call-ID boundary.

## WebSocket target and URL policy

### NEW `src/realtime/websocket.rs`

```rust
pub async fn handle(...) -> Response;
pub fn official_websocket_url(base: &str, original: &Uri)
    -> Result<String, RelayError>;
```

`official_websocket_url` mirrors `realtime::http::official_url`: trim only
trailing base slashes; remove exactly one inbound `/v1` when the base already
ends in `/v1`; otherwise append the entire inbound raw path/query; then convert
`http` to `ws` and `https` to `wss`. Root, `/v1`, and custom-prefix bases are
pinned. Duplicate query keys, blanks, `+`, `%2B`, encoded UTF-8, and ordering
remain byte-identical. It never adds AVAS parameters.

Private target policy stays separate:

| Explicit private dialect | Standalone upstream | Existing-call upstream |
|---|---|---|
| V1, `openai-alpha: quicksilver=v1` | `/v1/realtime?intent=quicksilver&model=...` | `/v1/realtime?intent=quicksilver&call_id=...` |
| Frameless, `openai-alpha: quicksilver=v2` | `/v1/live?model=...` | `/v1/live/{call_id}` |

Private standalone WebSockets require API-key managed mode. Private existing-
call joins continue to support the source-proven API-key-managed and ChatGPT-
managed profiles. ChatGPT standalone remains a capability error because the
inspected source requires an OpenAI API key. Alpha-free `?call_id=` is official
and must not reach the private AVAS builder.

## Browser subprotocol and credential domains

### NEW `src/realtime/subprotocol.rs`

```rust
pub struct ParsedProtocols {
    pub offered: Vec<String>,
    pub upstream_header: Option<HeaderValue>,
    pub browser_credential: Option<HeaderValue>,
}

pub fn parse(headers: &HeaderMap, admission: Option<&BearerToken>)
    -> Result<ParsedProtocols, RelayError>;

pub fn validate_selected(
    upstream: &HeaderMap,
    offered: &ParsedProtocols,
) -> Result<Option<String>, RelayError>;
```

Read every protocol header field in wire order and then comma-tokenize the
official values:

```text
realtime
openai-insecure-api-key.<ephemeral>
openai-organization.<organization-id>
openai-project.<project-id>
```

If any browser credential/organization/project token is present, `realtime` is
mandatory. Reject
duplicate tokens/classes across all fields, unknown tokens, empty suffixes,
invalid RFC token bytes, control bytes, token length over 4096 bytes, or
aggregate length over 8192 bytes. These limits are proxy policy, not claimed
OpenAI limits. Before `WebSocketUpgrade::from_request`, replace the request's
possibly repeated fields with one canonical accepted header so Axum can select
`realtime` even when the client split tokens across fields.

The complete upstream protocol header is sensitive because it may contain a
credential. Credential, organization, and project suffixes are never rendered
in logs/errors. If the ephemeral suffix equals the proxy admission token, return
`AdmissionSecretNotForwardable` before contact.

Credential resolution is explicit:

| Downstream channel | Managed profile | Client profile | Upstream channel |
|---|---|---|---|
| no browser key | configured bearer; caller bearer ignored | exactly one caller bearer required | `Authorization` |
| browser key, no Authorization | browser ephemeral wins | browser ephemeral wins | protocol header only |
| browser key plus Authorization | 400 ambiguous credential | 400 ambiguous credential | zero contact |

Allowing browser ephemeral credentials in managed mode is required for the
official mint-client-secret -> browser-WebSocket flow. It is selected by the
documented protocol channel, not token-shape guessing. The configured bearer is
not also sent.

Browser credential, organization, and project protocol tokens are valid only
when `selection.dialect == OfficialGa`. Any protocol token on explicit V1 or
Frameless standalone/query/path aliases returns
`InvalidRealtimeSubprotocol` (400) before permit acquisition or upstream
contact, in API-key and ChatGPT managed profiles alike.

The only safe downstream selected protocol is the literal `realtime`. With
tungstenite 0.29, zero selection succeeds only when no protocol header was sent
upstream. If any protocol was offered, its exact `NoSubProtocol`,
`InvalidSubProtocol`, and `ServerSentSubProtocolNoneRequested` protocol errors
map to the dedicated 502 protocol error before a stream is returned. Successful
post-connect validation therefore owns only repeated selected fields and an
offered-but-sensitive credential/organization/project selection; either closes
the new upstream socket under the teardown deadline and returns 502 before
downstream 101. Official docs do not promise which token the server selects;
the reflection rule is an explicit proxy security policy.

## WebSocket header builder

### MODIFY `src/realtime/headers.rs`

Add a WebSocket-specific builder rather than forcing browser auth through the
REST `Authorization` path:

```rust
pub struct WebSocketHeaders {
    pub headers: HeaderMap,
    pub protocols: ParsedProtocols,
}

pub fn upstream_websocket_headers(
    inbound: &HeaderMap,
    profile: &UpstreamProfile,
    selection: &ProtocolSelection,
    admission: Option<&BearerToken>,
) -> Result<WebSocketHeaders, RelayError>;
```

It forwards only audited public metadata: `origin`, `openai-organization`,
`openai-project`, `openai-safety-identifier`, and repeated non-empty
`openai-beta`; private alpha/attestation remain private-only. Browser protocol
organization/project tokens and same-named HTTP headers cannot both be present.

Cardinality and failure behavior are exact:

| Header | Cardinality | Empty/non-UTF-8 policy |
|---|---|---|
| `origin` | zero or one | empty dropped; non-UTF-8 rejected by the trust boundary/header builder |
| `openai-organization` | zero or one | empty dropped; non-UTF-8 rejected |
| `openai-project` | zero or one | empty dropped; non-UTF-8 rejected |
| `openai-safety-identifier` | zero or one | empty dropped; non-UTF-8 rejected |
| `openai-alpha` | zero or one | must exactly match selected private dialect; absent for official |
| `x-oai-attestation` | zero or one, private only | empty dropped; non-UTF-8 rejected |
| `authorization` | zero or one | repeated -> `AmbiguousAuthorization`; malformed -> `NoCredential` when required |
| `openai-beta` | zero or more | preserve ordered non-empty UTF-8/header values |

Repeated singleton metadata returns `InvalidRealtimeHeader`. Browser protocol
organization/project plus the same HTTP identity header, or browser credential
plus Authorization, returns `InvalidRealtimeSubprotocol`. Tests put the allowed
value first and second, include empty and non-UTF-8 values, and compare the
entire upstream map.

Never copy `host`, `connection`, `upgrade`, `cookie`, proxy admission headers,
`sec-websocket-key`, `sec-websocket-version`, or extensions. Build the request
with tungstenite `IntoClientRequest`, then append only the audited map and the
sanitized protocol header. Generated security headers remain library-owned.

The successful 101 response copies only safe non-handshake metadata:
`x-request-id`, `openai-processing-ms`, `openai-version`, and `x-ratelimit-*`.
Axum owns Upgrade/Connection/Accept; `WebSocketUpgrade::protocols` owns the
validated `realtime` selection.

## Handshake and frame-log privacy

### MODIFY `src/observability/mod.rs`, `tests/forensics.rs`

Tungstenite 0.29 logs the fully serialized client handshake request, complete
text messages, binary frame bytes, and close frames at trace/debug levels;
`HeaderValue::set_sensitive` cannot redact those raw renderings. The tracing
layer therefore applies a hard metadata filter, independent of
`GPT_LIVE_LOG`, that disables every `tungstenite` and `tokio_tungstenite`
target. This filter is ANDed with the user EnvFilter, so even explicit hostile
target directives cannot override it. Real managed, client-bearer, and browser-
protocol handshakes plus text/binary/close canaries run under global trace and
assert all canaries are absent.

### MODIFY `src/observability/frame_log.rs`, `docs/050_observability.md`

Move the metadata-only privacy boundary forward from WP6. Remove the optional
payload `context` field entirely. The record contains timestamp, direction,
text/binary kind, byte count, replacement/UTF-8 fault boolean, and optional
first fault byte offset only. No text, binary bytes, protocol value, or close
reason is retained. Existing U+FFFD tests become negative canary tests including
a secret adjacent to the fault. WP6 verifies/soaks this contract and does not
reintroduce excerpts or a reversible digest.

## Public upstream-first handshake

Handler order is exact:

1. verify GET, literal path, and WebSocket-upgrade shape; every non-upgrade or
   wrong-method request returns the existing 404 before query decoding;
2. strict raw-query decode and one `classify_websocket` call;
3. reject unsupported official-GA profile capability;
4. validate headers, browser protocols, and credential channel;
5. reject remaining private standalone profile capability after the
   official-only browser-channel guard;
6. acquire `active_connections` with `try_acquire_owned`;
7. build the raw-preserving upstream request with `IntoClientRequest`;
8. connect with `connect_async_with_config` under
   `websocket_connect_timeout` and configured frame limits;
9. validate the upstream selected protocol;
10. only now return Axum's downstream 101;
11. move upstream stream plus owned permit into `on_upgrade` and run the public
    pump through bounded teardown.

There is no downstream frame stream and no pending-frame queue before public
upstream success. A client disconnect during connect drops the connect future
and handler-owned permit. A disconnect between upstream success and
`on_upgrade` drops the captured upstream socket and permit.

The production handler delegates to an internal generic
`handle_with_after_upstream_ready` and supplies a no-op future. A module test
routes the same function with a channel barrier after protocol validation and
before response construction; aborting that held handler proves the captured
socket and permit drop. The connect-side test uses a held raw-TCP handshake.
Neither race uses sleeps.

Tokio-tungstenite does not follow redirects on this async path. Non-101
`Error::Http` bodies are not claimed byte-complete: tungstenite may expose only
the handshake tail. Preserve upstream status and safe response headers, but
render a fixed bounded local JSON error. This avoids fake body transparency.

### Exact handshake/error table

`WebSocketContractError` converts exhaustively to these `RelayError` variants:

| Contract error | Relay error |
|---|---|
| `UnknownRoute`, `MethodNotAllowed` | `UnknownEndpoint { method, path }` |
| `MissingSelector`, `AmbiguousQuery` | `InvalidRealtimeQuery` |
| `InvalidCallId` | `InvalidRealtimeCallId` |
| `PrivateDialectRequiresManaged`, `PrivateDialectNotSupported` | `UnsupportedRealtimeCapability` |

Malformed percent/UTF-8 query decoding also maps to `InvalidRealtimeQuery`.
Repeated metadata maps to `InvalidRealtimeHeader`; repeated Authorization to
`AmbiguousAuthorization`; browser-plus-Authorization or protocol/header
organization/project conflicts to `InvalidRealtimeSubprotocol`. Each mapping
has an exact rendered-envelope unit row and a real-socket zero-contact row.

| Variant/condition | Message | Status | Type | Code/header |
|---|---|---:|---|---|
| `InvalidRealtimeQuery` | `invalid Realtime WebSocket query` | 400 | `invalid_request_error` | `invalid_realtime_query` |
| `InvalidRealtimeSubprotocol` | `invalid Realtime WebSocket subprotocol` | 400 | `invalid_request_error` | `invalid_realtime_subprotocol` |
| existing `InvalidRealtimeCallId` | `invalid Realtime call_id` | 400 | `invalid_request_error` | `invalid_call_id` |
| existing `InvalidRealtimeHeader` | `invalid or repeated Realtime header` | 400 | `invalid_request_error` | `invalid_request_error` |
| existing `AmbiguousAuthorization` | `ambiguous Authorization: send exactly one credential` | 400 | `invalid_request_error` | `invalid_request_error` |
| existing `UnsupportedRealtimeCapability` | existing configured-profile message | 400 | `invalid_request_error` | `unsupported_realtime_capability` |
| existing `NoCredential` | existing message | 401 | `authentication_error` | `invalid_api_key` |
| existing `AdmissionSecretNotForwardable` | existing message | 401 | `authentication_error` | `invalid_api_key` |
| existing `UnknownEndpoint` | existing method/path message | 404 | `invalid_request_error` | `invalid_request_error` |
| `TooManyActiveRealtimeConnections` | `too many active Realtime connections` | 429 | `rate_limit_error` | `rate_limit_exceeded`, `Retry-After: 1` |
| `RealtimeWebSocketConnectTimeout` | `Realtime upstream WebSocket handshake timed out` | 504 | `server_error` | `upstream_server_error` |
| `RealtimeWebSocketUpstreamFailed` | `Realtime upstream WebSocket handshake failed` | 502 | `server_error` | `upstream_server_error` |
| `UpstreamWebSocketProtocol` | `Realtime upstream selected an invalid WebSocket subprotocol` | 502 | `server_error` | `upstream_websocket_protocol_error` |

Do not put tungstenite error strings into these envelopes because they may
contain a URL/query or handshake bytes.

The dedicated non-101 builder preserves the exact upstream status and only
`retry-after`, `x-request-id`, `openai-processing-ms`, `openai-version`, and
`x-ratelimit-*`. It discards the tungstenite body and upstream content type,
then sets `Content-Type: application/json` and this fixed envelope:

```json
{"error":{"message":"Realtime upstream WebSocket handshake rejected","type":"server_error","code":"upstream_websocket_rejected"}}
```

Close literals are equally fixed:

| Branch | Code | Reason |
|---|---:|---|
| per-frame/message cap | 1009 | `frame too large` |
| private pending aggregate cap | 1009 | `queued frames too large` |
| client-to-upstream send timeout | 1011 | `upstream send timed out` |
| upstream-to-client send timeout | 1011 | `downstream send timed out` |

Upstream 401, 403, 429, redirects, and 500 each have raw-TCP activation tests.
No error includes URL query, protocol token, frame payload, or close reason.

## Separate public and private pumps

### MODIFY `src/relay/pump.rs`

```rust
pub struct PumpPolicy {
    pub frame_bytes: usize,
    pub send_timeout: Duration,
    pub close_policy: ClosePolicy,
}

pub enum ClosePolicy { Transparent, PrivateNormalized }

pub type UpstreamSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type ConnectResult = Result<UpstreamSocket, RealtimeWebSocketConnectError>;

pub async fn run_public_pump(
    downstream: WebSocket,
    upstream: UpstreamSocket,
    policy: PumpPolicy,
    logger: FrameLogger,
) -> PumpOutcome;

pub async fn run_private_pump(
    downstream: WebSocket,
    connect: impl Future<Output = ConnectResult>,
    policy: PumpPolicy,
    logger: FrameLogger,
) -> PumpOutcome;

async fn run_connected<D, U>(
    downstream: D,
    upstream: U,
    policy: PumpPolicy,
    logger: FrameLogger,
) -> PumpOutcome
where
    D: Sink<AxumMessage> + Stream<Item = Result<AxumMessage, AxumError>> + Unpin,
    U: Sink<TungMessage> + Stream<Item = Result<TungMessage, TungError>> + Unpin;
```

Factor only the connected bidirectional loop and keep it generic over the
upstream `Sink + Stream`, which lets deterministic tests inject a pending
writer without changing production policy. Public receives an already-open
upstream and never allocates a pre-open queue. Private retains downstream-first
connect and its 32-frame queue, adding aggregate queued bytes capped at
`frame_bytes`. Both legs configure tungstenite/Axum max message and frame sizes,
check text/binary bytes before forwarding, and wrap every `send`, `flush`, and
close in `websocket_send_timeout` or the existing bounded close timeout.

Public close frames preserve peer code and reason when RFC-safe, but never log
the reason. Private downstream-to-upstream close remains normalized to
`1000 / client closed`; existing upstream-to-downstream preservation remains.
Ping/Pong stays library-managed and never consumes private queue count/bytes.

Use `write_buffer_size(0)`. Define `MAX_WEBSOCKET_FRAME_OVERHEAD = 14` and
reject `GPT_LIVE_WS_FRAME_MAX_BYTES` above
`usize::MAX - MAX_WEBSOCKET_FRAME_OVERHEAD` during config parsing. The finite
max write buffer is the checked sum `frame_bytes + 14` for Axum and tungstenite,
so it can hold one maximum masked frame but cannot become unlimited or overflow.
Queued-byte accounting also uses `checked_add`; overflow is the over-limit
branch. Unit tests pin maximum, maximum+1, exact aggregate cap, and cap+1.

## Router and connection ownership

### MODIFY `src/app.rs`

Add `AppState.active_connections: Arc<Semaphore>` from the already-bounded
`Limits.active_connections`.

The literal route table is:

| Route | Dispatch |
|---|---|
| `/v1/realtime` | alpha-free official standalone/existing-call; explicit V1/Frameless private |
| `/v1/realtime/` | recognized explicit private dialect only; alpha-free is existing 404 with zero contact |
| `/v1/realtime/translations` | official translation only |
| `/v1/realtime/translations/` | unregistered, existing 404 |
| `/v1/live/{call_id}` and slash form | private Frameless alias |
| `/v1/realtime/calls/{call_id}` and slash form | existing private alias |

Every registered entry uses `any` so the proxy owns wrong-method 404s. The old
slash-form direct route to `handle_sideband` is removed; both realtime slash
forms pass through deliberate dispatch and cannot accidentally add AVAS to an
alpha-free request.

Every WebSocket route acquires one connection permit before upstream contact.
Public permits span connect, downstream upgrade, pump, and teardown. Private
permits span downstream upgrade, upstream connect, queue, pump, and teardown.
429 paths do not read frames or contact upstream.

## Event fixture and conformance tests

### NEW `tests/fixtures/official/realtime-events.json`

Record source date and four explicit arrays:

- standard client: 11 event types;
- standard server: 46 event types;
- translation client: 3 event types;
- translation server: 7 event types.

Counts are a dated reference snapshot, not a permanent closed enum. Tests build
opaque JSON frames with unusual key order, whitespace, unknown fields, and a
UTF-8 sentinel. The proxy never parses them and must relay the exact text bytes.
`session.created` comes from the mock upstream; the proxy never synthesizes it.

### NEW `tests/official_websocket.rs`

Real sockets plus deterministic channels/barriers activate:

- exact standalone, existing-call, call-id-plus-model, and translation raw URLs;
- selector errors, malformed encoding, private alpha boundaries, and zero contact;
- malformed-query non-upgrades still return 404 before query parsing;
- browser credential/org/project protocols on V1, Frameless, private query
  joins, and both private path aliases reject before permit/contact in every
  supported managed profile;
- no AVAS insertion on alpha-free official paths;
- managed Authorization, client Authorization, and browser ephemeral protocol
  auth with exact upstream header/protocol maps and admission canaries;
- an actual managed `POST /v1/realtime/client_secrets` response whose returned
  value is then used in the browser protocol handshake, proving the REST-to-WS
  mint chain and absence of configured Authorization on that WS request;
- selected `realtime` propagation and rejection of credential selection;
- upstream 401/403/429/redirect/500, timeout, reset, malformed 101, and wrong
  protocol before downstream observes 101; each non-101 body carries a canary
  absent from the fixed local JSON, and content type is exactly JSON;
- exact tungstenite `NoSubProtocol`, `InvalidSubProtocol`, and
  `ServerSentSubProtocolNoneRequested` mappings plus repeated selected headers;
- first upstream `session.created`, all 11/46 standard types, and all 3/7
  translation types byte-identically;
- text/binary variant preservation and unknown future event opacity;
- frame cap and cap+1 in both directions with 1009;
- public close code/reason propagation in both directions;
- one permit occupied -> max+1 429 -> close -> successful recovery;
- client disconnect during held handshake and between upstream success and
  downstream upgrade, proving upstream/socket/permit release;
- send-timeout branches through the production generic connected-loop seam with
  a deliberately pending `Sink`, never OS-buffer saturation or sleeps;
- official/private trailing-slash route rows and zero-contact alpha-free 404;
- global trace managed/client/browser handshakes and U+FFFD frame canaries,
  proving no raw handshake or payload excerpt reaches either log sink.
- repeated/empty/non-UTF-8 Origin and account metadata with exact first/second
  value activation and full upstream-map equality.

### MODIFY `tests/sideband.rs`

Make `a_realtime_query_join_carries_the_intent_parameter` explicitly send
`openai-alpha: quicksilver=v1`; alpha-free query join is now public. Add private
pre-open aggregate byte-cap, post-open frame-cap, connect-timeout,
send-timeout, active-permit lifetime, and existing downstream-first queue-order
tests. Preserve all current close/header/frame regressions.

## Verification and audit gate

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --test official_websocket
cargo test --test sideband
cargo test --all-features
cargo +1.86 check --locked --all-targets --all-features
gitleaks detect --source=. --no-git --redact --no-banner
```

The implementation audit must verify exact URL/header/protocol captures,
upstream-first ordering, no false downstream 101, permit release at each race,
mutation-activating limit/timeout tests, and no credential/query/frame/close
payload in logs. Claims not guaranteed by official docs—selected token,
translation browser-subprotocol support, binary application frames, exact close
behavior, selector error body, size/deadline limits, call-ID syntax, and event
counts—must remain labeled proxy policy or dated snapshot evidence.
