# 090 — Protocol, route, and credential foundation

Work-phase: `wp1-proxy-foundation`. This phase changes the shared boundary but
does not add the full endpoint set. It must preserve all 255 baseline tests while
making public GA behavior independent from private Quicksilver defaults.

## Loop specification

- Archetype: spec-satisfaction repair.
- Trigger: official OpenAI clients currently fail after a base-host change.
- Goal: one runtime authority classifies public/private transport and selects an
  explicit credential/header policy before any upstream contact.
- Non-goals: endpoint breadth, semantic GA↔Frameless event translation, account
  pools, media processing.
- Verifier: focused route/auth/header/property tests plus the original full suite.
- Stop: every matrix row is reachable and mutation-checked; no baseline Live
  behavior changes.
- Escalation: re-plan if an official credential channel cannot be represented
  without weakening admission or if two worker packets fail independently.
- HOTL bounds: local writes only in this repository; official docs and research
  repos read-only; no live credential or paid call; one work-phase/cycle.

## Structural decision

Context: `src/live` mixes generic relay mechanics with private GPT-Live policy.
Public GA routes cannot safely reuse profile defaults that add AVAS or replace an
ephemeral credential.

Rejected: rename or rewrite the entire `live` module. That would make the first
compatibility slice a broad regression-prone move.

Chosen: add a public `realtime` facade and extract only mechanics that have a
second consumer into `relay`. Private URL/session policy remains in `live`.

Dependency direction after this phase:

```text
app -> realtime -> relay
app -> live     -> relay
wire (private schema policy) <- live only
admission/config <- app/realtime/live
```

## File manifest and diff-level changes

### NEW `src/relay/mod.rs`

Exports transport mechanics only:

```rust
pub mod body;
pub mod pump;
pub mod ws_convert;
```

### MOVE `src/live/pump.rs` → `src/relay/pump.rs`

No behavioral rewrite. Update imports from `crate::live::ws_convert` to
`crate::relay::ws_convert`. Preserve every existing unit/integration test and
public outcome/close literal.

### MOVE `src/live/ws_convert.rs` → `src/relay/ws_convert.rs`

No wire changes. Text/binary/control conversion remains variant-preserving.

### NEW `src/relay/body.rs`

Move only generic capped body reading from `src/live/body.rs`:

```rust
pub async fn read_capped(body: Body, max: usize) -> Result<Bytes, RelayError>;
```

Multipart-to-ChatGPT conversion stays in `live/body.rs`.

### NEW `src/realtime/mod.rs`

Declares only `contract` and `headers` in this phase. `http` is added by `100`
and `websocket` by `110`; Rust module declarations never precede their files.
The facade does not re-export private `WireAdapter` types.

### NEW `src/realtime/contract.rs`

Canonical runtime classification:

```rust
pub enum ApiDialect { OfficialGa, QuicksilverV1, Frameless }
pub enum Transport { Http, WebRtcCall, StandaloneWebSocket,
                     ExistingCallWebSocket, TranslationWebSocket }
pub enum SessionKind { Realtime, Transcription, Translation, Opaque }
pub enum CredentialPolicy { Managed, ClientBearer, Ephemeral }

pub struct ProtocolSelection {
    pub dialect: ApiDialect,
    pub transport: Transport,
    pub session_kind: SessionKind,
    pub credential: CredentialPolicy,
}
```

The app-server RPC version is not part of this type. `v2` RPC and Realtime V2
are separate axes (`~/Developer/codex/.../protocol/v2/realtime.rs:65`).

The exact pre-routing API is:

```rust
pub struct RouteFacts<'a> {
    pub method: &'a Method,
    pub path: &'a str,
    pub query: &'a [(String, String)],
    pub content_type: Option<&'a str>,
    pub openai_alpha: Option<&'a str>,
    pub credential_mode: UpstreamCredentialMode,
}

pub fn classify(facts: &RouteFacts<'_>)
    -> Result<ProtocolSelection, ContractError>;
```

It does not parse a GA JSON session body. Known private alpha values alone
select private dialects: `quicksilver=v1` → `QuicksilverV1` and
`quicksilver=v2` → `Frameless`; absent or unknown values remain `OfficialGa`.
WP1 exports classification and tests it but does not register a new endpoint.

| Facts | Exact result |
|---|---|
| `POST /v1/realtime/calls`, multipart, no private alpha | official WebRTC, opaque session, configured managed/client policy |
| same, `application/sdp` | official WebRTC, opaque session, `Ephemeral` |
| same, known private alpha | matching private WebRTC, opaque session, `Managed`; client mode is a contract error |
| `GET /v1/realtime`, no `call_id`, exactly one non-empty `model` | official standalone WS, Realtime session, configured managed/client policy |
| same, exactly one valid `call_id`, with zero or more `model` values | official existing-call WS, opaque session, configured managed/client policy; `model` is ignored by the upstream |
| same with neither selector, duplicate `call_id`, or duplicate/empty standalone `model` | `ContractError::AmbiguousQuery`, `MissingSelector`, or `InvalidCallId` |
| `GET /v1/realtime/translations`, exactly one `model` | official translation WS, Translation session, configured managed/client policy |
| translation path with `call_id`, missing/duplicate `model`, or private alpha | contract error |
| exact future REST bootstrap/control path with `POST` | official `Http` with Realtime, Transcription, Translation, or Opaque kind according to `080` |
| wrong method, malformed content type, unknown path | `MethodNotAllowed`, `UnsupportedContentType`, or `UnknownRoute` |

Ordered query input preserves duplicates for rejection. Query values are not
decoded a second time. A sole empty/whitespace standalone selector is
`MissingSelector`. One valid `call_id` takes precedence over every `model`
value because the official server-side controls contract says `model` is
ignored when joining an existing call. Tests fire every
row, both credential modes, exact
private values, an unknown alpha, selector order permutations, and cap-adjacent
call IDs where applicable. Exact call-control segment validation remains owned
by `100` rather than duplicated here.

Content-type classification validates the full MIME token/parameter syntax,
rejects duplicate or empty parameters, and requires exactly one non-empty
`boundary` for multipart. It does not accept a valid-looking media type prefix
followed by malformed parameters.

### NEW `src/realtime/headers.rs`

Build, never clone, public request and response maps.

```rust
pub fn upstream_headers(
    client: &HeaderMap,
    profile: &UpstreamProfile,
    selection: &ProtocolSelection,
) -> Result<HeaderMap, RelayError>;

pub fn response_headers(upstream: &HeaderMap) -> HeaderMap;
```

The public request allowlist is exact:

```text
content-type, accept, openai-organization, openai-project,
openai-safety-identifier, openai-beta, idempotency-key, openai-alpha,
x-oai-attestation (private dialects only)
```

`openai-alpha` and `x-oai-attestation` are copied only for a recognized private
dialect; public unknown alpha values are not allowed to switch policy.
`content-type`, organization,
project, safety identifier, idempotency key, alpha, and `authorization` are
singletons: more than one value is an error. `accept` and `openai-beta` are
list-valued and every non-empty value is appended in original order. HeaderMap
normalizes names case-insensitively. Organization, project, safety identifier,
idempotency key, private attestation, and every credential value are marked
sensitive. Unknown names, empty values, admission headers, cookies, proxy
authorization, and hop-by-hop/framing headers are dropped.

`chatgpt-account-id` is inserted from the configured profile only for a selected
private dialect. It is never emitted for `OfficialGa`, even if an invalid caller
combines an official selection with a ChatGPT-shaped profile.

Authentication is inserted last. `Managed` requires configured managed auth
and ignores a single client Authorization value; `ClientBearer` and HTTP
`Ephemeral` require exactly one syntactically valid `Bearer` Authorization and
copy it sensitive. The token requires at least one RFC 6750 base character
before optional trailing `=` padding, so padding-only values are invalid. A
missing or repeated required credential is an error. The
later browser-WebSocket phase obtains `Ephemeral` from its validated
subprotocol parser rather than this HTTP header branch.

The safe response allowlist is also exact:

```text
content-type, location, retry-after, x-request-id,
openai-processing-ms, openai-version
```

In addition, any header whose normalized name starts exactly with
`x-ratelimit-` is preserved for forward-compatible official rate-limit
metadata. All values for allowed response names are appended in upstream order.
`set-cookie`, `connection`, `upgrade`, `transfer-encoding`, proxy/admission
headers, and arbitrary lookalike prefixes are absent. Exact-map tests include
mixed case, duplicate list values, duplicate singleton rejection, a
`x-ratelimit-` future name, a near-miss prefix, cookies, and hop-by-hop names.

### MODIFY `src/config.rs`

Add an explicit auth policy; do not infer from token prefixes:

```rust
pub enum UpstreamCredentialMode { Managed, Client }
pub struct Limits {
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub websocket_frame_bytes: usize,
    pub active_connections: usize,
    pub request_read_timeout: Duration,
    pub upstream_timeout: Duration,
    pub websocket_connect_timeout: Duration,
    pub websocket_send_timeout: Duration,
}
```

`UpstreamProfile::ApiKey` splits into
`ApiKeyManaged { base_url, auth }` and `ApiKeyClient { base_url }`;
`managed_auth()` returns an option and `credential_mode()` is derived from the
variant. `Config` stores no separately mutable mode, so a library caller cannot
construct `Managed + None`, `Client + Some(token)`, or `ChatGPT + Client`.
`ChatGptBackend` remains managed-only and retains a required token.
`GPT_LIVE_CREDENTIAL_MODE=managed|client` defaults to `managed`. In API-key
client mode `GPT_LIVE_TOKEN` is optional and is not stored even when present;
the caller bearer is the only upstream credential. Managed mode and every
ChatGPT profile require a non-empty token. `chatgpt + client` is a config error.
Tests prove client mode starts with no token, its Debug has no ignored token,
managed mode fails without one, and the invalid profile/mode pair fails.

Environment/default mapping is exact:

| Field | Environment | Default |
|---|---|---|
| `request_bytes` | `GPT_LIVE_REQUEST_MAX_BYTES` | 16 MiB |
| `response_bytes` | `GPT_LIVE_RESPONSE_MAX_BYTES` | 16 MiB |
| `websocket_frame_bytes` | `GPT_LIVE_WS_FRAME_MAX_BYTES` | 16 MiB |
| `active_connections` | `GPT_LIVE_MAX_CONNECTIONS` | 128 |
| `request_read_timeout` | `GPT_LIVE_REQUEST_READ_TIMEOUT_MS` | 30 s |
| `upstream_timeout` | `GPT_LIVE_UPSTREAM_TIMEOUT_MS` | 120 s |
| `websocket_connect_timeout` | `GPT_LIVE_WS_CONNECT_TIMEOUT_MS` | 15 s |
| `websocket_send_timeout` | `GPT_LIVE_WS_SEND_TIMEOUT_MS` | 15 s |

Positive integers only; zero, overflow, and malformed values identify the exact
environment key in `ConfigError`. Existing `max_body_bytes`,
`max_response_bytes`, and `upstream_timeout` call sites move to `config.limits`
in this phase; future WS fields may remain unused public configuration until
their owning phase.

`AppState::new` constructs
`Client::builder().redirect(reqwest::redirect::Policy::none()).build()` and
remains fallible. Custom `http://` bases remain operator-trusted for local
tests/development; default public bases use HTTPS. A real-socket 307 canary test
proves the current call-create request, body, and credential never reach the
redirect target and the 307 is relayed instead.

### MODIFY `src/admission/auth.rs` and `src/admission/mod.rs`

Before: `authorization` is always an admission candidate and a matching
admission secret is always rejected as forwardable.

After: admission extraction receives the configured credential mode. The exact
matrix is:

| Mode/bind | Admission candidates and result | Upstream Authorization |
|---|---|---|
| managed, loopback | admission skipped; duplicate Authorization still rejected | configured managed token wins |
| managed, network | strict name precedence `X-GPT-Live-API-Key` → `Authorization` → `X-Api-Key`; first non-empty name wins and any matching value within it passes | configured managed token wins |
| client, loopback | admission skipped; duplicate Authorization still rejected | exactly one client bearer required by header builder |
| client, network | only `X-GPT-Live-API-Key` → `X-Api-Key` participate, with the same strict precedence/any-matching-value rule; missing configured admission secret fails closed | exactly one client bearer required |

A wrong higher-priority admission header is never rescued by a lower one.
Repeated dedicated admission values retain baseline any-match semantics because
they are proxy-only and never forwarded; repeated Authorization is always
`AmbiguousAuthorization`. In client mode Authorization is never used to satisfy
admission. In every mode and on loopback, an Authorization value equal to the
configured admission secret is rejected before upstream contact. Tests cover
every row, missing secret, wrong/high-priority values, duplicate dedicated
values, duplicate Authorization, split credential success, and the admission
secret canary in the upstream domain. A present non-UTF-8 value in the
higher-priority admission name is a decisive rejection; it never disappears and
falls through to a lower-priority secret.

### MODIFY `src/admission/cors.rs`

Add `OpenAI-Safety-Identifier`, `OpenAI-Organization`, `OpenAI-Project`,
`OpenAI-Beta`, and non-safelisted `Idempotency-Key`. `Accept` is already a CORS
safelisted request header. Browser credential-bearing WebSocket subprotocols are
not a CORS request-header token and are governed by the handshake parser.

### MODIFY `src/error.rs`

Add `InvalidRealtimeHeader`, rendered as `400 invalid_request_error`, so
singleton/shape rejection is never mislabeled as a `502` upstream failure.

### MODIFY `src/app.rs`, `src/live/mod.rs`, imports and tests

Register the `relay` and `realtime` modules and update moved imports. Existing
route behavior remains unchanged in this phase.

The current `/v1/realtime/calls` registration still points at the private
legacy handler until `100`, because URL/body/header/status activation must move
as one atomic REST slice. WP1 therefore does not claim client-mode route
compatibility: a real-socket test pins client mode to a fail-before-contact 401
on that legacy handler. `100` replaces that test with the successful exact-map
client-bearer route test and is the first phase allowed to consume
`classify`, `upstream_headers`, and `response_headers` in an official handler.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test admission::
cargo test realtime::contract::
cargo test realtime::headers::
cargo test live::
cargo test --all-features
```

Test tables are the classifier, admission, header, config, and redirect tables
above. Tests assert exact maps, not `contains` subsets; every policy branch has
a fired test; replacing any route/dialect/credential enum arm with its neighbor
must fail. The current 255-test suite remains green after ownership moves.
