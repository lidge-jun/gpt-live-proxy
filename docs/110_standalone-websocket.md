# 110 — Standalone and translation WebSockets

Work-phase: `wp3-standalone-websocket`. Depends on `090`; consumes route and
credential classification without private AVAS defaults.

## File changes

### NEW `src/realtime/websocket.rs`

Separate connection targets:

```rust
pub enum WebSocketTarget {
    Standalone { model: String },
    ExistingCall { call_id: String },
    Translation { model: String },
}
```

Parsing uses ordered query pairs so duplicate `model`/`call_id` values are
detectable. `model` and `call_id` together are rejected. Existing-call ignores
an absent model; standalone requires one; translation accepts only its dedicated
path.

The public upstream URL preserves the exact accepted query and adds no
`intent=quicksilver`. Private V1/Frameless standalone paths are enabled only in
API-key mode and keep their source-proven private query/path rules.

### Public upstream-first handshake

Establish and validate the upstream handshake under the configured connect
timeout before completing the downstream upgrade. Preserve an allowed upstream
selected subprotocol. Upstream non-101 status maps to a bounded downstream HTTP
error while HTTP is still possible; timeout maps deterministically.

In axum terms, the public handler completes `connect_async` and validates the
selected protocol before returning `ws.on_upgrade(...)`. There is no downstream
frame stream and therefore no pre-open queue on this public path. A failed
upstream handshake returns an HTTP error and the client never observes a false
`101 Switching Protocols`.

The implementation must not hand-roll WebSocket security headers. It builds the
upstream request via `IntoClientRequest`, then adds the audited header map and
subprotocol list.

### NEW `src/realtime/subprotocol.rs`

Parse comma-tokenized protocols without reflecting arbitrary values.

Accepted official tokens are `realtime`, one
`openai-insecure-api-key.*`, and optional organization/project tokens.
Duplicates, multiple credentials, control bytes, oversized tokens, and an
admission canary fail. All credential-bearing values are marked sensitive and
redacted by name and token class.

### MODIFY `src/app.rs`

Dispatch `/v1/realtime` by query classification rather than assuming sideband;
add `/v1/realtime/translations`. Keep `/v1/live/{call_id}` as a private alias.

### MODIFY/MOVE `src/relay/pump.rs`

Add frame byte bounds and send deadlines to the open public pump. Active
connection permits are owned outside the pump and released on every terminal
path.

The private legacy V1/Frameless sideband path remains downstream-first because
its source-proven contract includes frames arriving while the upstream
handshake is in flight. Only that private policy retains a pre-open queue,
bounded by both frame count and aggregate bytes. Public and private handshake
states are separate entry points rather than contradictory branches in one
state machine.

Close behavior for the public transparent relay preserves peer code/reason when
safe; private asymmetric normalization remains selected by policy rather than
hardcoded globally.

### NEW `tests/official_websocket.rs`

Real-socket tests cover:

- official `?model=gpt-realtime-2.1` connection and first `session.created`;
- `session.update`/`session.updated` and all 11 client/46 server event type
  fixtures as byte-identical unknown JSON;
- `?call_id=` with no invented intent;
- translation path and translation-specific events;
- server header and browser subprotocol authentication;
- exact downstream selected protocol;
- upstream 401/403, timeout, wrong protocol, reset, stalled sink;
- frame count/byte limits, oversized post-open frame, active-connection limit;
- text/binary variant and close propagation.

No test sleeps. Handshake gates, sink barriers, and channels activate each
conditional path.

## Verification

```bash
cargo test --test official_websocket
cargo test --test sideband
cargo test --all-features
```
