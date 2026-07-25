# 020 — call-create relay

Work-phase `wp3-callcreate`. Deliverable: `POST /v1/live` and `POST /v1/realtime/calls` fully implemented.

Depends on `015` (trust boundary) and `040` (wire adapters). Build order is `010 → 015 → 040 → 020 → 030 → 050 → 060 → 070`.

## Adapter resolution

`WireAdapter` is the single authority for the AVAS rule and the `openai-alpha` value, so this phase must *consume* it rather than restate it:

```rust
pub fn resolve_adapter(client_headers: &HeaderMap) -> Option<WireAdapter>;
```

Derived solely from an inbound `openai-alpha` value: `quicksilver=v1` → `V1`, `quicksilver=v2` → `FramelessBidi`, anything else → `None`. A missing header yields `None`, and the relay then applies the profile-shape default without inventing a header — **absent stays absent** is contractual (`001` §3), so `resolve_adapter` never writes back into the outgoing map.

When the adapter is `None` the URL builder falls back to the profile shape alone, which is exactly the behavior the source has today (it never inspects `openai-alpha`).

## New files

```text
src/live/mod.rs
src/live/url.rs        AVAS query + call-create URL builders
src/live/headers.rs    protocol whitelist + precedence merge
src/live/body.rs       capped read + multipart→JSON rewrite
src/live/call_create.rs  the handler
src/live/location.rs   Location header → call id parsing
```

## `url.rs`

```rust
// AVAS_QUERY and SIDEBAND_API_ROOT are re-exported from `wire` (040); this module does not redeclare them.
use crate::wire::{AVAS_QUERY, SIDEBAND_API_ROOT, WireAdapter};

pub fn with_avas_query(url: &str) -> String;   // no-op only if BOTH intent= and architecture= present
pub fn keyed_call_create_url(base: &str) -> String;   // strip trailing /v1, append /v1/realtime/calls, add AVAS
pub fn forward_call_create_url(base: &str, backend_shape: bool, adapter: Option<WireAdapter>) -> String;
//   backend_shape  → {base}/realtime/calls, AVAS appended per adapter.wants_avas_query(true)
//   !backend_shape → {base}/live, AVAS appended per adapter.wants_avas_query(false)
//   adapter == None → profile default: AVAS on the backend shape, none on /live
```

Unit tests pin every literal in `001` §2.3 including the idempotence rule and the `?` vs `&` choice, plus the `(adapter, backend_shape)` truth table proving this builder and `WireAdapter::wants_avas_query` never disagree.

## `headers.rs`

```rust
pub const CLIENT_PROTOCOL_HEADERS: [&str; 6] = [
    "openai-alpha", "x-session-id", "session-id", "thread-id", "originator", "x-oai-attestation",
];

pub fn client_protocol_headers(h: &HeaderMap) -> HeaderMap;  // non-empty values only
pub fn merge_upstream_headers(
    client: HeaderMap,
    profile: &UpstreamProfile,
    adapter: Option<WireAdapter>,
) -> HeaderMap;
//   order: client whitelist -> provider static -> auth (authorization, chatgpt-account-id)
//   `adapter` is threaded through for assertion only: a debug_assert checks that any forwarded
//   `openai-alpha` equals adapter.openai_alpha(). It NEVER inserts the header when absent.
```

Auth and account header values are constructed with `HeaderValue::set_sensitive(true)` (`002` D5) before insertion.

Tests: all six forwarded; empty values dropped; `x-openai-fedramp` never forwarded; a client `authorization` losing to proxy auth; and — passing an adapter supplied **independently** of the request, since `resolve_adapter` can only return `Some` when the header is present — an absent `openai-alpha` is still never invented in the outgoing map.

## `body.rs`

```rust
pub async fn read_capped(body: Body, max: usize) -> Result<Bytes, RelayError>;
pub async fn backend_json_from_multipart(body: Bytes, content_type: &str)
    -> Result<(Bytes, &'static str), RelayError>;
```

The rewrite requires a string `sdp`, emits `{"sdp":…}` when `session` is absent, otherwise parses the `session` string as arbitrary JSON and emits `{"sdp":…,"session":<value>}` with `application/json`. Field order in the emitted JSON is `sdp` then `session`, matched by a serialized-string assertion.

Each of the four multipart failures maps to its exact message.

## `location.rs`

```rust
pub fn parse_call_id(location: &str) -> Option<String>;
fn is_call_id_segment(s: &str) -> bool;  // rtc_ + non-empty suffix, or 8-4-4-4-12 hex UUID
```

Split on `?`, keep the path, scan segments right-to-left, first valid wins. This is used by tests and by any future client mode; the relay itself passes `Location` through untouched.

## `call_create.rs`

Handler order: trust boundary (`015`) → **install `OutcomeGuard`** → read body (capped) → resolve profile and adapter → build URL → rewrite body when forward+backend+multipart → merge headers → set proxy-owned `content-type` last → hand the whole upstream lifecycle to the spawned task described below → await its join result → return status and body with only `content-type` and `location`.

The guard is installed at handler entry, *before* body reading, so a client that disappears mid-body is observed by the same mechanism as one that disappears mid-response. Response-body buffering is **not** performed by the handler; it belongs to the spawned task.

### Cancellation and timeout

Axum has no `req.signal` equivalent, and the obvious design does not work: if the handler future is dropped when the client disconnects, that future's own `select!` is dropped with it and can never observe its own cancellation, so it cannot execute a `499` branch. Ownership has to sit **outside** the handler future.

```rust
pub struct CallOutcome(Arc<Mutex<Outcome>>);

pub enum Outcome { InFlight, Completed(StatusCode), TimedOut, Failed(String), ClientCanceled }

impl CallOutcome {
    /// Terminal transitions are conditional: they apply ONLY from `InFlight`.
    /// Whoever reaches a terminal state first wins, so a request completing
    /// concurrently with a disconnect can never overwrite `ClientCanceled`,
    /// and a late disconnect can never overwrite `Completed`.
    pub fn finish(&self, next: Outcome) -> bool;
    pub fn get(&self) -> Outcome;
}

/// Owns BOTH halves of cancellation: it records the outcome and it cancels the token.
pub struct OutcomeGuard {
    slot: CallOutcome,
    token: CancellationToken,   // a clone, so Drop can actually cancel
}

impl Drop for OutcomeGuard {
    fn drop(&mut self) {
        // conditional: no-op if the spawned task already reached a terminal state
        if self.slot.finish(Outcome::ClientCanceled) {
            self.token.cancel();
        }
    }
}

pub struct UpstreamResult {
    pub status: StatusCode,
    pub headers: HeaderMap,   // already filtered to content-type + location
    pub body: Bytes,          // already read under the response cap
}
```

Mechanism:

1. At handler entry the handler creates a `CallOutcome` and a `tokio_util::sync::CancellationToken`, then constructs an `OutcomeGuard` holding a clone of **both**. The guard lives in the handler future.
2. The handler **spawns** the upstream work as an independent task holding its own clones of the slot and the token. That task owns the *entire* upstream lifecycle: building and sending the request, awaiting the response, extracting `content-type` and `location`, and buffering the body under `LIVE_RESPONSE_MAX_BYTES`. It returns a fully materialized `UpstreamResult`. Nothing droppable remains in the handler.
3. The handler awaits the join handle. If the client disconnects, the handler future is dropped, `OutcomeGuard::drop` runs, conditionally records `ClientCanceled`, and cancels the token — which aborts the spawned task's in-flight `reqwest` call and its body read.
4. The spawned task runs `tokio::time::timeout(Duration::from_secs(120), work)` selected against `token.cancelled()`, and reports its result with `finish(...)`, whose conditional semantics make step 3 and step 4 race-free in either order.

Because `finish` only transitions from `InFlight`, exactly one terminal outcome is ever recorded and the disconnect test is deterministic rather than timing-dependent.

Outcomes: timeout elapsed → `504` `live upstream timed out`; transport error → `502` `live relay failed: {error}`; response over cap → `502` `live response too large ({n} bytes)`; client gone → `ClientCanceled`, logged and recorded as `499`.

The inbound body read also runs under the guard: a client that vanishes mid-body drops the handler, the guard fires, and the read is abandoned. `read_capped` needs no cancellation parameter because dropping its future is sufficient — the guard, not the reader, owns the observation.

Because a departed client cannot receive a response, `499` is an **observability** outcome rather than a delivered status. Tests assert it by reading the recorded `Outcome`, not by reading a response — the client-disconnect test drops the connection mid-body and mid-response and asserts `Outcome::ClientCanceled`. `060` states this explicitly so the test is implementable from the ownership model described here.

A `499` *is* still returned as a real response in the one case where a client remains: an inbound body read that fails with an abort error while the connection is technically still open maps to the `499` row of `001` §10 directly.

Failure mapping is exactly the table in `001` §2.5.

### `multer` specifics

The boundary is extracted from the inbound `Content-Type` with `multer::parse_boundary`, then the buffered body is fed as a single-chunk stream.

Field policy, asserted rather than left to library defaults:

- The **first** occurrence of `sdp` and of `session` wins; later duplicates are ignored.
- `sdp` must be a non-file textual field and valid UTF-8. This is not a preference: the rewritten backend body is JSON containing `"sdp": "<string>"`, and a Rust JSON string is UTF-8 by definition, so arbitrary bytes cannot be carried there without an undocumented encoding. The source enforces the same thing by requiring `sdp` to be a string. A non-UTF-8 or file-valued `sdp` maps to `ChatGPT voice relay expects multipart field sdp on call-create`.
- `session`, when present, must likewise be textual and valid UTF-8; a non-string field maps to `ChatGPT voice relay expected a string multipart session field`, and unparsable content maps to `ChatGPT voice relay expected JSON in the multipart session field`.
- A body that cannot be parsed as multipart at all maps to `ChatGPT voice relay could not parse multipart call-create body`.

The **keyed** path is unaffected: it never parses or rewrites, so it forwards arbitrary bytes and the original boundary verbatim. Byte-lossless SDP transport therefore remains available on that path.

## Exit criteria

Unit tests for `url`, `headers`, `body`, `location`; integration tests against a mock upstream asserting the ChatGPT rewrite path, the keyed multipart-preserving path, header precedence, cap boundaries, timeout, and response-header filtering.
