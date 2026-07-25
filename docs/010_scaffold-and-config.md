# 010 — Scaffold, config model, server skeleton

Work-phase `wp2-scaffold`. Deliverable: a crate that builds and serves `/healthz`.

## Files

```text
Cargo.toml
.gitignore
src/main.rs        binary entry: config from env, tracing init, serve
src/lib.rs         library root, re-exports, `pub mod` list
src/config.rs      Config, UpstreamProfile, UpstreamAuth, BearerToken
src/error.rs       RelayError + IntoResponse mapping to the JSON error envelope
src/app.rs         router construction + AppState
```

## `src/config.rs`

```rust
pub struct Config {
    pub bind: SocketAddr,              // GPT_LIVE_BIND, default 127.0.0.1:10110
    pub upstream: UpstreamProfile,
    pub frame_log: Option<PathBuf>,    // GPT_LIVE_FRAME_LOG | OCX_LIVE_FRAME_LOG
    pub upstream_timeout: Duration,    // 120s
    pub max_body_bytes: usize,         // 16 MiB
    pub max_response_bytes: usize,     // 16 MiB
}

pub enum UpstreamProfile {
    /// ChatGPT backend-api: JSON call-create body, sideband on the API host.
    ChatGptBackend { base_url: String, auth: UpstreamAuth, account_id: Option<String> },
    /// OpenAI API key: multipart preserved, `/v1/realtime/calls` call-create.
    ApiKey { base_url: String, auth: UpstreamAuth },
}

pub struct UpstreamAuth(BearerToken);
pub struct BearerToken(String);  // Debug prints `Bearer <redacted>`
```

`UpstreamProfile::uses_backend_shape()` returns true when the base URL contains `/backend-api`, mirroring the upstream rule rather than the enum variant, so a custom base behaves identically.

Env keys: `GPT_LIVE_BIND`, `GPT_LIVE_UPSTREAM_MODE` (`chatgpt` | `apikey`), `GPT_LIVE_BASE_URL`, `GPT_LIVE_TOKEN`, `GPT_LIVE_ACCOUNT_ID`, `GPT_LIVE_FRAME_LOG`.

## `src/error.rs`

```rust
pub enum RelayError {
    BodyTooLarge, ResponseTooLarge(usize), ClientCanceled, BodyUnreadable(String),
    MultipartParse, MultipartMissingSdp, MultipartSessionNotString, MultipartSessionNotJson,
    NoUpstream, NoCredential, UpstreamTimeout, UpstreamFailed(String),
    UpgradeFailed, UnknownEndpoint { method: String, path: String },

    // Trust-boundary variants (implemented in 015, declared here so the taxonomy is complete).
    AdmissionRequired,                       // 401 "gpt-live-proxy API key required"
    AdmissionSecretNotForwardable,           // 401 admission bearer must not go upstream
    OriginBlocked(RequestKind),              // 403; the kind selects one of two exact messages
    Draining,                                // 503 plain text + Retry-After: 5
}

pub enum RequestKind { Http, WebSocketUpgrade }
```

`OriginBlocked` carries its `RequestKind` because the two rejections have **different** exact messages (`cross-origin data-plane request blocked` vs `WebSocket upgrade blocked: non-local Origin`); a payload-free variant could not select between them. `Draining` is a variant rather than a bypass so that every response, including the shutdown path, flows through one `IntoResponse`.

`IntoResponse` produces the exact status / message / `type` / `code` rows inventoried in `001` §10 — those literals are the contract, so they live in one table constant with a table-driven test that walks every row. Rows marked "omitted (pool-only)" in that inventory (`409`, `429`, and the two pool `401`s) have no variant here.

## `src/app.rs`

```rust
pub struct AppState { pub config: Arc<Config>, pub http: reqwest::Client }
pub fn router(state: AppState) -> Router
```

Routes registered in this phase: `GET /healthz` → `{"status":"ok","service":"gpt-live-proxy","version":<pkg version>}`. Call-create and sideband routes are registered in 020 and 030, behind the trust boundary that lands in **015**.

Graceful shutdown on SIGINT/SIGTERM via `axum::serve(...).with_graceful_shutdown(...)`. The observable draining contract — `503`, plain-text `Service shutting down`, `Retry-After: 5` — belongs to **015**, which owns its implementation and test.

Admission credentials, origin policy, and CORS are **not** modeled in this phase's `Config`; `015` extends `Config` with `admission_token` and `cors_allow_origins`. This phase covers upstream selection only.

## Exit criteria

`cargo build --all-targets`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` all clean; one test asserting `/healthz` returns 200 with the expected JSON.
