# 050 — Observability and frame forensics

Work-phase `wp6-observability`. Deliverable: tracing plus the opt-in, metadata-only frame log.

## New files

```text
src/observability/mod.rs
src/observability/frame_log.rs
```

## Tracing

`tracing_subscriber` with an env filter (`GPT_LIVE_LOG`, default `info`). Spans: `call_create` (method, path, upstream host, status, elapsed) and `sideband` (join style, upstream host, close code). **Never** logged by the tracing layer: authorization values, account ids, bearer tokens, SDP bodies, session JSON, frame payloads, close reasons, or full upstream URLs (the host only, since a URL embeds the call id).

The opt-in frame log is the one exception and is governed by its own guarantee below: it writes nothing for a clean frame and a bounded excerpt for a corrupted one.

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
- `context` is present only when U+FFFD is found, spanning at most 24 scalars on each side of the first occurrence, clamped to char boundaries. A frame shorter than that window yields the whole frame; see the guarantee section below.
- Any IO error while logging is swallowed — forensics must never break a call.

`context` is computed over Unicode scalar values with char-boundary clamping. The TypeScript original slices UTF-16 code units with an exclusive end (up to 24 before, at most 23 after); the invariant preserved here is a *bounded* excerpt, not the exact unit count. `001` §8 records the divergence, and the guarantee section below states precisely what "bounded" does and does not promise.

## The privacy guarantee, stated precisely

The claim is **bounded excerpt**, not **no payload**. Specifically:

- A clean frame produces no excerpt at all. This is the common case and the one that matters for a normal call.
- A corrupted frame produces up to `CONTEXT_CHARS` scalars on each side of the first replacement character. A frame shorter than that window yields the whole frame.
- Therefore a secret adjacent to the corruption **is** in the excerpt.

That last point is a deliberate trade rather than an oversight: an excerpt that omitted the surrounding bytes could not attribute corruption, which is the only reason the log exists. The mitigations are that the log is opt-in, that clean frames write nothing, and that the operator is told to treat the file as sensitive.

Because the operator chooses the path, `.gitignore` cannot guarantee exclusion — the README instructs writing the log outside the working tree.

## Non-blocking by construction

Records are handed to a dedicated writer thread through a bounded channel and dropped when it is full. A synchronous `open`/`write` inside the relay would block on a slow disk, a stalled network filesystem, or a FIFO, and blocking there stops frame forwarding. Losing a diagnostic record under pressure is strictly better than stalling a voice call.

## Exit criteria

Tests: inert when unset; a clean text frame produces `fffd: false` and no `context`; a frame containing U+FFFD produces a bounded excerpt, and text beyond the window is absent while adjacent text is captured by design; a multi-byte payload clamps to char boundaries; a binary frame reports raw byte length; an unwritable path does not panic or abort the relay; an append failure mid-relay leaves the relay running; `BearerToken` redaction, header sensitivity, and `redacted_headers`.
