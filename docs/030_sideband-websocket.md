# 030 — Sideband WebSocket relay

Work-phase `wp4-sideband`. Deliverable: the three join styles proxied transparently.

Depends on `015` (trust boundary) and `040` (wire adapters), both of which land first.

## New files

```text
src/live/sideband.rs   target parsing + upstream ws URL
src/live/ws_convert.rs bidirectional Message conversion between axum and tungstenite
src/live/pump.rs       the handshake/queue state machine
```

## Target parsing

```rust
pub enum SidebandTarget {
    FramelessPath { call_id: String },
    RealtimeCallsPath { call_id: String },
    RealtimeQuery { call_id: String },
}

pub fn parse_sideband_target(path: &str, query: &HashMap<String, String>) -> Option<SidebandTarget>;
```

Rules from `001` §1: `/v1/live/{id}`, `/v1/realtime/calls/{id}`, `/v1/realtime?call_id={id}`, each accepting one optional trailing slash. Path ids are percent-decoded, query ids are trimmed. The decoded id must match `^[A-Za-z0-9_-]{1,128}$` — implemented as a hand-rolled char scan plus length check rather than a regex dependency.

A percent-decode **failure** returns `None` too, which is the deliberate divergence recorded in `001` §1: the TypeScript parser lets `decodeURIComponent` throw, the Rust port maps it to the same `404`.

Boundary tests: 1 char, 128 chars, 129 chars, empty, slash, percent-encoded slash, `+`, unicode, malformed escape.

## Upstream URL

```rust
pub fn sideband_upstream_url(base: &str, backend_shape: bool, t: &SidebandTarget) -> String;
```

Backend shape ignores `base` entirely and uses `SIDEBAND_API_ROOT` — this is the `3b766d91` rule and carries a comment naming it. Non-backend strips a trailing `/v1` from the provider root and rebuilds the same three shapes. `https` → `wss`, `http` → `ws`, anything else unchanged.

The join style is also derivable from `WireAdapter::sideband_join` (`040`); when the relay only sees an inbound path it uses the parsed `SidebandTarget`. A test asserts both routes produce the same URL for each adapter, so there is one authority rather than two.

## Message conversion (`ws_convert.rs`)

`axum::extract::ws::Message` and `tungstenite::protocol::Message` are **distinct enums** even at a matched dependency version, so conversion is explicit and total:

```rust
pub fn axum_to_tungstenite(m: AxumMessage) -> Option<TungsteniteMessage>;
pub fn tungstenite_to_axum(m: TungsteniteMessage) -> Option<AxumMessage>;
```

| axum | tungstenite | Note |
|---|---|---|
| `Text` | `Text` | payload moved, never re-encoded |
| `Binary` | `Binary` | payload moved, never decoded |
| `Ping` / `Pong` | `Ping` / `Pong` | see the ping policy below |
| `Close(Option<CloseFrame>)` | `Close(Option<CloseFrame>)` | code and reason converted explicitly |
| — | `Frame(_)` | never produced by a read; returns `None` |

`CloseFrame` conversion is **not** a `u16`-to-`u16` copy: axum exposes `code: u16`, while tungstenite 0.29 uses a `CloseCode` enum with `From<u16>` / `Into<u16>` conversions, so each direction converts explicitly. A missing code becomes `1000` and a missing reason becomes `""`, matching the source. Round-trip tests cover a normal code, a reserved/unknown code, and both defaults.

**Ping policy, stated rather than assumed.** Each library answers pings on its own socket automatically. The relay therefore does **not** forward ping or pong across the boundary; each leg keeps its own keepalive. Consequently pings are not counted against the pre-open queue, which holds data messages only. A test asserts a downstream ping does not appear upstream and that a data frame sent right after still arrives.

## The pump state machine (`pump.rs`)

The naive shape — `connect_async().await` and only then read downstream — cannot implement the pre-open queue at all, because no downstream frame can arrive before the handshake resolves. The queue window is exactly the interval between accepting the downstream upgrade and the upstream handshake completing, so both must be polled **concurrently**:

```rust
enum UpstreamState {
    Connecting { queue: VecDeque<TungsteniteMessage> },
    Open,
    Closed,
}

pub async fn run_pump(down: WebSocket, connect: BoxFuture<'static, ConnectResult>, log: FrameLogger);
```

Before the handshake resolves:

```rust
tokio::select! {
    res = &mut connect_fut => {
        // Ok  -> flush the queue in insertion order, transition to Open
        // Err -> close both sides 1011 "upstream connect failed"
    }
    Some(msg) = down_rx.next() => {
        // data message -> push; if len would exceed 32 -> close both 1009 "too many pending frames"
        // ping/pong    -> handled locally, not queued
        // close        -> abandon the handshake, close 1000 "client closed" once connected or drop
    }
}
```

After `Open`, the socket is split and two forwarding tasks run until either side terminates. The 32-frame bound applies **only** in `Connecting`; after open there is no accounting, matching `001` §5.

`connect_async` receives a request built via `IntoClientRequest` on the URL string and *then* extended with the merged protocol and auth headers, so the library still generates `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Connection`, and `Upgrade`. Hand-rolling a bare `http::Request` would drop those and is explicitly forbidden.

## Behavior pinned from `001` §5

- Frames are relayed as whole messages; `Text` stays `Text`, `Binary` stays `Binary`. No JSON parsing, no re-encoding.
- Upstream close → downstream closes with the same code and reason; missing code `1000`, missing reason `""`.
- Downstream close → upstream closes `1000` `client closed`, deliberately discarding the client's own code.
- Error mapping: missing upstream `1011 missing upstream`; connect failure `1011 upstream connect failed`; send failures `1011 upstream send failed` / `1011 client send failed`; upstream transport error `1011 upstream error`.
- No post-open backpressure accounting and no upstream connect timeout, matching the source.

## Handler wiring

`GET` on the three paths with an `Upgrade: websocket` header → run the `015` trust boundary → resolve profile and headers with the same code path as call-create → `ws.on_upgrade(...)`. A non-upgrade request on those paths falls to the unknown-endpoint `404`.

## Exit criteria

Unit tests for parsing and URL construction covering all six style/shape combinations, plus round-trip tests for every `ws_convert` arm including `CloseFrame` defaults. Integration tests against a mock upstream WebSocket server asserting: the exact join URL received, protocol headers present on the handshake, byte-identical text and binary echo in both directions, binary staying `Binary` at the variant level, a >1 MiB UTF-8 payload intact, close code and reason propagating upstream→downstream, the `1000`/`client closed` normalization downstream→upstream, the 33rd-frame `1009`, a connect failure yielding `1011 upstream connect failed`, and ping non-forwarding.

**Deliberately not "fixed":** the asymmetric close and the absence of post-open backpressure are reproduced from the source. Changing either requires updating `002` D3 first.
