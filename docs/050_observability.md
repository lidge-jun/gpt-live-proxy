# 050 — Observability and frame forensics

Work-phase `wp3-standalone-websocket`. Deliverable: credential-safe tracing and
an opt-in, strictly metadata-only frame log.

## Tracing

`GPT_LIVE_LOG` configures the user `EnvFilter` and defaults to `info`. The
formatting layer ANDs that filter with a non-overridable privacy filter that
rejects every dependency target beginning with:

```text
tungstenite
tokio_tungstenite
```

Tungstenite can trace the complete serialized WebSocket client request, text
messages, hexadecimal binary frames, and close reasons. Those records may
contain `Authorization`, `openai-insecure-api-key.<ephemeral>`, transcripts, or
peer-controlled secrets, and `HeaderValue::set_sensitive` cannot redact bytes
after serialization. Consequently even explicit dependency directives cannot
enable those targets:

```text
GPT_LIVE_LOG=trace,tungstenite::handshake::client=trace
```

Other tracing targets continue to obey the user filter. Tests emit canaries on
the exact target and a child target while also proving an unrelated trace event
survives.

On a non-loopback bind the service emits one deployment warning containing only
the fixed fields `security_model=single_principal` and
`tenant_isolation=false`. It carries no bind credential, upstream credential,
account, origin, call ID, URL, or request data. A loopback bind emits no such
warning. The warning is about the absence of tenant isolation: admission auth
does not establish call-ID ownership.

Application spans record bounded operational metadata such as method, local
path, upstream host, status, elapsed time, join style, and terminal outcome.
They never record authorization values, account IDs, bearer tokens, SDP/session
bodies, frame payloads, close reasons, full upstream URLs, call IDs, or browser
protocol values.

Credential protection remains layered:

1. `BearerToken`'s `Debug` prints `Bearer <redacted>`.
2. Credential-bearing `HeaderValue`s are marked sensitive.
3. `redacted_headers` is the only sanctioned header renderer and also redacts
   known credential channels by name.
4. All Tungstenite and Tokio-Tungstenite dependency targets are hard-disabled
   after the user filter is parsed.

`/healthz` and `/readyz` are intentionally credential-free observability
surfaces. They return liveness/readiness status only. Readiness becomes 503
while draining or when request or connection capacity is exhausted and returns
to 200 after recovery; no response includes configured limits or active counts.

## Frame-forensics record

The logger is inert unless `GPT_LIVE_FRAME_LOG` or its compatibility alias
`OCX_LIVE_FRAME_LOG` supplies a non-empty path. Each text or binary application
frame produces one JSONL record immediately before its send attempt:

```rust
pub struct FrameRecord {
    pub ts: String,                         // RFC 3339 UTC
    pub dir: &'static str,                  // "c2u" | "u2c"
    pub kind: &'static str,                 // "text" | "binary"
    pub bytes: usize,                       // wire payload bytes
    pub fffd: bool,                         // U+FFFD or invalid UTF-8 found
    pub fault_byte_offset: Option<usize>,   // first fault, omitted when clean
}
```

`fault_byte_offset` is a zero-based byte offset, not a Unicode-scalar index. For
text it marks the first literal U+FFFD. For binary it marks whichever appears
first: a literal U+FFFD in the valid prefix or the first malformed UTF-8 byte.
The field is diagnostic metadata only; it does not identify or retain adjacent
content.

The record never contains:

- text or binary payload bytes;
- a payload excerpt or reversible digest;
- authorization, protocol, model, call-ID, or account values;
- WebSocket close reasons.

This guarantee applies equally to clean and faulty frames. A credential directly
adjacent to U+FFFD or an invalid binary byte is absent from the JSONL output.
Tests pin both cases with distinct canaries and assert the removed `context`
field cannot reappear.

## Relay safety

Records are handed to a dedicated writer thread through a bounded channel.
`try_send` drops records when the queue is full, so diagnostics cannot stall a
voice relay. Synchronous file open/write errors are swallowed. Shutdown waits
for the writer only up to the configured drain budget; an upgraded relay clone
or blocked filesystem cannot prevent process termination.

Ping, pong, and close control frames are not frame-forensics records. `bytes`
is the UTF-8 byte length for text and the untouched raw length for binary. The
inspection result never feeds back into the relayed frame.

## Exit criteria

- A hostile EnvFilter cannot enable Tungstenite/Tokio-Tungstenite handshake,
  protocol, frame, or close targets, while unrelated tracing remains enabled.
- A clean frame has `fffd: false` and omits `fault_byte_offset`.
- Text and binary faults report the exact first byte offset.
- Adjacent U+FFFD and binary credential canaries never appear in output.
- No serialized record has `context`, payload, close-reason, or digest fields.
- An unwritable path and a saturated writer cannot interrupt relay traffic.
- Bearer `Debug`, header sensitivity, and `redacted_headers` remain covered.
- Non-loopback startup emits only the fixed single-principal warning fields;
  loopback startup emits none.
- Health and readiness responses contain no configuration or principal data.
