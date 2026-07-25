# 050 — Observability and frame forensics

Work-phase `wp6-observability`. Deliverable: tracing plus the opt-in, metadata-only frame log.

## New files

```text
src/observability/mod.rs
src/observability/frame_log.rs
```

## Tracing

`tracing_subscriber` with an env filter (`GPT_LIVE_LOG`, default `info`). Spans: `call_create` (method, path, upstream host, status, elapsed) and `sideband` (join style, upstream host, close code). **Never** logged: authorization values, account ids, bearer tokens, SDP bodies, session JSON, frame payloads.

Credential protection is layered, because a newtype alone is insufficient (`002` D5):

1. `BearerToken`'s `Debug` prints `Bearer <redacted>`, covering `{:?}` on `Config`.
2. That protection **ends** the moment the token becomes a `HeaderValue` inside a `HeaderMap`, so every credential-bearing value is built with `HeaderValue::set_sensitive(true)`, and logging a whole `HeaderMap` is prohibited.
3. `redacted_headers(&HeaderMap) -> String` is the only sanctioned renderer; it replaces the value of any sensitive header, plus a name list of `authorization`, `chatgpt-account-id`, `x-api-key`, `x-gpt-live-api-key`, and `x-oai-attestation`, with `<redacted>`. The attestation is client-supplied and copied into the upstream map, so it is treated as bearer-grade material rather than as an ordinary protocol header.

Tests assert all three: the `Debug` output, that a constructed auth header reports `is_sensitive()`, and that `redacted_headers` never echoes a token substring.

## Frame forensics

```rust
pub struct FrameLogger { path: Option<PathBuf> }

#[derive(Serialize)]
pub struct FrameRecord<'a> {
    pub ts: String,          // RFC 3339
    pub dir: &'static str,   // "c2u" | "u2c"
    pub kind: &'static str,  // "text" | "binary"
    pub bytes: usize,
    pub fffd: bool,
    #[serde(skip_serializing_if = "Option::is_none")] pub context: Option<&'a str>,
}

impl FrameLogger {
    pub fn from_env() -> Self;                       // GPT_LIVE_FRAME_LOG, alias OCX_LIVE_FRAME_LOG
    pub fn log(&self, dir: Direction, msg: &Message);
}
```

Rules carried over verbatim from `001` §8:

- Unset env var → the logger is inert and does no work at all.
- One JSONL record appended per relayed frame, immediately before the send attempt, so a record proves receipt and attempted forwarding rather than delivery.
- `bytes` is the UTF-8 byte length for text and the raw length for binary.
- Binary payloads are decoded **only** to detect U+FFFD; the decoded form never feeds back into the relayed frame.
- `context` is present only when U+FFFD is found, spanning at most 24 characters on each side of the first occurrence, clamped to char boundaries.
- Any IO error while logging is swallowed — forensics must never break a call.

`context` is computed over Unicode scalar values with char-boundary clamping. The TypeScript original slices UTF-16 code units with an exclusive end (up to 24 before, at most 23 after); the invariant preserved here is *bounded excerpt, never the full payload*, not the exact unit count. `001` §8 records the divergence.

Excerpts can contain adjacent transcript text, so the README documents the log as sensitive diagnostic output. The operator chooses the path, so `.gitignore` alone cannot guarantee exclusion — the README states that the log must be written outside the working tree.

## Exit criteria

Tests: inert when unset; a clean text frame produces `fffd: false` and no `context`; a frame containing U+FFFD produces a bounded excerpt and never the full payload; a multi-byte payload clamps to char boundaries; a binary frame reports raw byte length; an unwritable path does not panic or abort the relay; an append failure mid-relay leaves the relay running; `BearerToken` redaction, header sensitivity, and `redacted_headers`.
