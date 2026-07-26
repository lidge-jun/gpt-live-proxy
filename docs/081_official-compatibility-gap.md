# 081 — Current compatibility gap

Baseline: `de1240b9126f813cdd7727a396325434626477ad`.

## Matrix

| Public contract | Baseline | Evidence | Required owner |
|---|---|---|---|
| multipart `POST /v1/realtime/calls` | partial | route exists (`src/app.rs:61-63`), but API-key URL always gains private AVAS query (`src/live/url.rs:22-26`) | `100`; `120` composition regression |
| raw-SDP call with ephemeral bearer | broken | inbound bearer is replaced by configured auth (`src/live/headers.rs:83-100`) | `090`, `100`; `120` composition regression |
| call response SDP + `Location` | supported | status/body and two old headers preserved (`src/live/call_create.rs:243-298`) | `100` regression; `120` end-to-end composition |
| client-secret/session endpoints | missing | no routes in `src/app.rs:61-87` | `100` |
| translation call/client-secret paths | missing | no translation route or URL owner | `100`, `110` |
| SIP/WebRTC call controls | missing | no accept/reject/refer/hangup routes | `100` |
| standalone WS `?model=` | missing | parser requires `call_id` (`src/live/sideband.rs:78-81`) | `110` |
| sideband WS `?call_id=` | partial | private `intent=quicksilver` is injected (`src/live/sideband.rs:102-109`) | `120` |
| browser WS subprotocol authentication | missing | downstream selects none and upstream whitelist omits it (`src/live/sideband.rs:179-252`) | `090`, `110` |
| GA event transport | conditional support | pump is opaque after a connection exists (`src/live/pump.rs:112-228`) | `110` conformance |
| public request headers | partial | only six private protocol headers pass (`src/live/headers.rs:17-25`) | `090` |
| official retry/correlation response headers | missing | only content-type/location pass (`src/live/call_create.rs:23-25`) | `090` |
| API-key Frameless `/v1/live` | broken production branch | keyed branch bypasses the direct-Frameless URL helper (`src/live/call_create.rs:157-177`) | `120` |

## Scope correction

`docs/002_design-decisions.md` D1b records the first release's standalone
WebSocket exclusion as historical scope. `080` revokes that target: standalone
sessions are the main public GA surface. D1 remains valid only for OpenCodex
product-policy features such as account pools, cooldown, and thread affinity.

The README sentence that tells a user to point a client base at this proxy is
currently true only for the narrow OpenCodex WebRTC relay path. It must not make
a drop-in compatibility claim until `090` through `150` pass.

## Reuse decisions

No-code options were considered and rejected:

- configuration cannot make `/v1/realtime?model=` parse as a standalone
  connection;
- a generic external reverse proxy cannot translate ChatGPT backend body/header
  rules or keep admission credentials separate;
- deleting the private paths would regress the already proven GPT-Live flow.

The implementation reuses these owners:

- whole-subrouter admission/origin/CORS/drain middleware (`src/app.rs`);
- capped reads and cancellation ownership (`src/live/body.rs`,
  `src/live/call_create.rs`);
- byte/variant-transparent WebSocket conversion and pump
  (`src/live/ws_convert.rs`, `src/live/pump.rs`);
- redacted configuration/header diagnostics;
- real-socket HTTP and WebSocket test harnesses.

Generic transport pieces will move behind a shared relay boundary before the
official facade consumes them. The existing private URL/body/header decisions
remain in `live`; public route classification must not depend on private
`WireAdapter` defaults.

## Dependency order

1. `090`: classify routes and credentials, define safe header policy, and extract
   transport primitives without changing observable Live behavior.
2. `100`: add official REST bootstrap/control routes using the audited HTTP
   relay.
3. `110`: add standalone and translation WebSockets, including upstream-first
   handshake and browser subprotocol negotiation.
4. `120`: align public WebRTC/sideband behavior and retain private aliases.
5. `130`: implement only source-proven ChatGPT mappings and capability errors.
6. `140`: close resource, egress, privacy, mutation, SDK-conformance, and docs
   gaps.
7. `150`: fresh release gates, push, and exact-SHA remote CI.
