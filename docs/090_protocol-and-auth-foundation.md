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

Route classification takes method, path, the ordered query pairs, content type,
and private negotiation header. It does not parse an entire GA session body in
this phase. Conditional activation tests cover `model`, `call_id`, both,
neither, duplicates, raw SDP, multipart, and translation paths.

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

Public request allowlist includes content negotiation, organization/project,
safety identifier, official compatibility headers, and explicitly approved
idempotency/correlation values. Authentication is inserted last according to
`CredentialPolicy`. Private `x-oai-attestation` remains sensitive.

Safe response allowlist includes `content-type`, `location`, `retry-after`,
request IDs, and documented OpenAI rate-limit metadata. `set-cookie`,
`connection`, `upgrade`, transfer framing, and arbitrary headers remain absent.

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

`GPT_LIVE_CREDENTIAL_MODE=managed|client`, default `managed`. Client mode on a
non-loopback bind requires `GPT_LIVE_API_KEY` in the dedicated header domain;
an `Authorization` bearer is never silently reused for admission.

Construct the HTTP client with redirects disabled. Preserve custom `http://`
bases for local tests/development, but document them as operator-trusted and
require HTTPS for the default public bases.

### MODIFY `src/admission/auth.rs` and `src/admission/mod.rs`

Before: `authorization` is always an admission candidate and a matching
admission secret is always rejected as forwardable.

After: admission extraction receives the configured credential mode. In client
mode, only `X-GPT-Live-API-Key`/`X-Api-Key` can satisfy network admission;
`Authorization` belongs to the upstream domain. In managed mode, existing
behavior remains. Repeated authorization remains a hard error.

### MODIFY `src/admission/cors.rs`

Add `OpenAI-Safety-Identifier`, `OpenAI-Organization`, `OpenAI-Project`, and
`OpenAI-Beta`. Browser credential-bearing WebSocket subprotocols are not a CORS
request-header token and are governed by the handshake parser.

### MODIFY `src/app.rs`, `src/live/mod.rs`, imports and tests

Register the `relay` and `realtime` modules and update moved imports. Existing
route behavior remains unchanged in this phase.

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

Tests assert exact maps, not `contains` subsets; every credential-policy branch
has a fired test; replacing any route/dialect/credential enum arm with its
neighbor must fail.
