# 015 — Downstream trust boundary: admission, origin, CORS, draining

Work-phase `wp2b-trust`. Deliverable: nothing reaches the relay handlers without passing this layer. Specification source: `001` §11.

This phase lands **before** 020/030 so that call-create and sideband are never "fully implemented" while unguarded.

## New files

```text
src/admission/mod.rs
src/admission/auth.rs     admission credential extraction + constant-time compare
src/admission/origin.rs   origin/host policy
src/admission/cors.rs     CORS header set + OPTIONS handling
src/admission/drain.rs    draining flag + 503 response
```

Modified: `src/error.rs` — the four trust-boundary variants declared in `010` (`AdmissionRequired`, `AdmissionSecretNotForwardable`, `OriginBlocked(RequestKind)`, `Draining`) get their `IntoResponse` arms here, wired to the exact rows of `001` §10.

## Config additions

```rust
pub struct Config {
    // ...
    pub admission_token: Option<BearerToken>,    // GPT_LIVE_API_KEY; reuses the redacting newtype from 010
    pub cors_allow_origins: Vec<String>,          // GPT_LIVE_CORS_ORIGINS, comma-separated
}

impl Config {
    pub fn requires_admission_auth(&self) -> bool;  // true unless bind IP is loopback
}
```

## `auth.rs`

```rust
pub fn check_admission(headers: &HeaderMap, cfg: &Config) -> Result<(), RelayError>;
pub fn is_admission_secret(value: &str, cfg: &Config) -> bool;
```

- Loopback bind → `Ok(())` immediately.
- Otherwise the first non-empty value among `x-gpt-live-api-key`, bearer `authorization`, `x-api-key` is compared to the configured token with `subtle::ConstantTimeEq`. `subtle` is a declared direct dependency (`002` D2); a hand-rolled comparison is **not** an accepted fallback on this path.
- A missing or non-matching credential → `401` `gpt-live-proxy API key required` (`authentication_error` / `invalid_api_key`), the reworded counterpart of the source's `opencodex API key required`.
- Separately, when the client `authorization` bearer **is** the admission secret, the relay refuses to forward it upstream and returns `401` with the exact message `gpt-live-proxy admission credentials cannot be forwarded upstream` (`authentication_error` / `invalid_api_key`). Callers needing both domains put the proxy credential in `X-GPT-Live-API-Key` and the upstream bearer in `Authorization`.

## `origin.rs`

```rust
pub fn check_origin(headers: &HeaderMap, cfg: &Config, kind: RequestKind) -> Result<(), RelayError>;
// returns Err(RelayError::OriginBlocked(kind)); RequestKind is declared in error.rs (010)
```

With admission auth **not** required: the request `Host` must be loopback with either no explicit port or the configured port; a missing `Origin` is accepted; loopback origins are accepted; configured extra origins are accepted.

With admission auth required: missing origin, loopback origin, an exact match of the request origin, or a configured extra origin is accepted, and host-loopback validation is skipped.

Rejection message differs by kind: `cross-origin data-plane request blocked` for HTTP, `WebSocket upgrade blocked: non-local Origin` for upgrades. Both are `403` with wire values `type: "invalid_request_error"`, `code: "origin_rejected"` — see the note under `001` §10, since the source's internal `origin_rejected` token is *not* the emitted `type`.

## `cors.rs`

```rust
pub const ALLOW_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, OPTIONS";
pub const ALLOW_HEADERS: &str = "Content-Type, Authorization, X-GPT-Live-API-Key, X-Api-Key, \
    ChatGPT-Account-Id, OpenAI-Alpha, X-Session-Id, Session-Id, Thread-Id, Originator, X-OAI-Attestation";

pub fn apply_cors(res: &mut Response, origin: Option<&str>, cfg: &Config);
pub async fn handle_options(...) -> Response;   // 204 allowed, 403 rejected, never authenticated
```

The six protocol header names must appear in `ALLOW_HEADERS` or a browser preflight strips them and the `75344b09` defect returns through the front door. A test asserts all six are present.

`Vary: Origin` is always set. `Access-Control-Allow-Origin` echoes an allowed origin, otherwise falls back to the local proxy origin.

## `drain.rs`

```rust
pub struct DrainState(Arc<AtomicBool>);
pub fn draining_response() -> Response;   // 503, text/plain "Service shutting down", Retry-After: 5
```

SIGINT/SIGTERM sets the flag, then graceful shutdown runs. While the flag is set, both call-create routes and the sideband upgrade answer `503` — CORS-wrapped — before any other work.

## Ordering inside a request

```text
draining -> admission auth -> origin policy -> handler -> CORS wrap
```

CORS wraps *every* HTTP response including errors; an established WebSocket has no further HTTP CORS processing.

## Exit criteria

Tests: loopback bind skips auth; non-loopback bind rejects a missing credential `401`; each of the three credential header names accepted; an admission bearer never forwarded upstream; origin acceptance and rejection in both auth modes with the two distinct messages; `OPTIONS` returning `204`/`403` without authentication; all six protocol headers present in `ALLOW_HEADERS`; draining returning `503` with the plain-text body and `Retry-After: 5`.
