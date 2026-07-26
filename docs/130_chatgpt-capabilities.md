# 130 — ChatGPT/GPT-Live capability profile

Work-phase: `wp5-chatgpt-compat`. Depends on `100` through `120`.

## Source-proven boundary

The 2026-07-23 Codex snapshot proves ChatGPT auth for private V1 and
Frameless WebRTC call-create plus API-host sideband. It does not prove official
GA standalone, translation, transcription, client-secret, or SIP semantics.
Those public capabilities remain native in the API-key profiles and fail with a
proxy-originated capability error in the ChatGPT profile.

No GA↔Frameless event rename layer is added. Public and private events remain
opaque; a partial converter would corrupt tool, reasoning, transcript, and turn
semantics.

## Capability authority

### NEW `src/realtime/capability.rs`

Use three explicit profile kinds: API-key managed, API-key client, and ChatGPT.
The capability key includes the operation/target, not only
`ProtocolSelection`:

- all ten official REST operations are separate rows;
- official standalone, existing-call, and translation WebSockets are separate;
- private V1/Frameless call-create, standalone, existing-call query, and
  historical sideband alias/path surfaces are separate.

`Capability::ALL` and `ProfileKind::ALL` drive an exhaustive table test. Every
profile × capability pair has exactly one `Native`, `Adapted`, or `Unsupported`
decision. New variants must make the test or an exhaustive match fail.

The table is policy only. URL, body, header, credential, timeout, permit, and
pump behavior remain owned by `100`–`120`.

The support matrix is:

| Surface | API-key managed | API-key client | ChatGPT |
|---|---|---|---|
| Official REST 10 operations | Native | Native | Unsupported |
| Official standalone/existing-call/translation WS | Native | Native | Unsupported |
| Private V1/Frameless call-create | Adapted | Unsupported | Adapted |
| Private V1/Frameless standalone WS | Adapted | Unsupported | Unsupported |
| Private V1/Frameless existing-call/aliases | Adapted | Unsupported | Adapted |

SIP call controls and an existing-call WebSocket are covered by the official
rows. SIP trunk configuration and incoming webhook delivery are outside a
base-URL proxy and remain explicit out-of-scope items.

### Integration points

- `realtime::contract` classifies a valid private dialect independently of the
  configured credential mode. API-key client therefore reaches the capability
  table instead of failing early as `PrivateDialectRequiresManaged`; the table
  owns the profile decision.
- `realtime::http` converts `ClassifiedRest.operation` to a capability and
  replaces the temporary ChatGPT GA gate.
- `realtime::websocket` converts `ClassifiedWebSocket.target` plus dialect to a
  capability and replaces `unsupported_profile`.
- private sideband validates path × `openai-alpha` first, then applies the
  capability table before credentials, permit, or upstream contact.

Split `live::headers` and `realtime::headers` into validation and construction
steps. Validation returns a typed, credential-free result; only the later build
step inserts authorization/account identity. This makes the precedence below
executable rather than aspirational.

Error precedence is fixed:

1. trust boundary;
2. route/method/path/query/upgrade and dialect/content-type classification;
3. official profile capability rejection;
4. official header/subprotocol validation;
5. private path/header/subprotocol validation;
6. private profile capability rejection;
7. credential construction, permit, body read, and upstream work.

This keeps ChatGPT official requests on one stable capability error while
preserving the stricter private authentication boundary.

## Session evidence

### NEW `src/realtime/chatgpt.rs`

Session shape is not a pre-body capability. Private multipart is read under the
existing cap and parsed once, then its optional session JSON is classified as:

- `Absent`;
- `Opaque`;
- `Quicksilver` (`type=quicksilver`);
- `Frameless` (no top-level type and `delegation.type=client`);
- `OfficialRealtime` (`type=realtime`);
- `OfficialTranscription` (`type=transcription`);
- `Contradictory` (known private markers disagree inside one object).

Absent and opaque shapes remain accepted to preserve source-proven SDP-only and
future-private compatibility. Matching V1/Frameless evidence is accepted.
Known private evidence under the wrong negotiated dialect fails with
`invalid_realtime_session_shape`. Explicit official realtime/transcription
evidence in a ChatGPT private call fails with the profile-capability error.

Evidence edge cases are exact:

- malformed JSON, non-UTF-8 session text, missing textual SDP, and malformed
  multipart retain their existing multipart error variants;
- any known top-level type combined with `delegation.type=client` is
  `Contradictory` (including `quicksilver`, `realtime`, and `transcription`);
- an unknown top-level type combined with the Frameless delegation marker is
  also `Contradictory`, because Frameless is explicitly type-less;
- scalar/array JSON and unknown types without a private marker are `Opaque`.

All ChatGPT private call-create shapes, including a direct API-shaped base, now
parse multipart once for evidence. This intentionally tightens only malformed,
non-UTF-8, or explicitly contradictory/official session bodies. SDP-only and
valid opaque JSON remain compatible, and direct upstream forwarding still uses
the original bytes.

### MODIFY `src/live/body.rs`, `src/live/call_create.rs`

Split multipart parse from backend JSON construction. A parsed private call is
reused for ChatGPT session evidence and backend body construction; no body is
parsed twice. Direct API-shaped private upstreams still receive the original
multipart bytes unchanged. Session evidence is selected by profile, never by
whether the base URL happens to look backend-shaped.

## Stable errors

### MODIFY `src/error.rs`

Keep HTTP 400 and `unsupported_realtime_capability`, but add a data-bearing
proxy-originated profile error:

```json
{
  "error": {
    "message": "gpt-live-proxy: this Realtime capability requires the `apikey` upstream profile",
    "type": "invalid_request_error",
    "code": "unsupported_realtime_capability",
    "param": "upstream_profile",
    "source": "gpt-live-proxy",
    "capability": "...",
    "configured_profile": "chatgpt",
    "required_profiles": ["apikey_managed", "apikey_client"]
  }
}
```

Each unsupported table row owns its exact `required_profiles` array. Official
rows accept both API-key profiles; private call-create/sideband rows accept the
managed profiles that actually support that surface; private standalone accepts
only API-key managed. Protocol negotiation errors keep their existing wire code. Session evidence
conflicts use `invalid_realtime_session_shape` so clients can distinguish a
profile limitation from a contradictory private body.

## Documentation and tests

Update `README.md` and `docs/002_design-decisions.md` with the surface-level
matrix. Keep app-server RPC v2, public Realtime GA/V2, and
`quicksilver=v2` Frameless as three distinct concepts.

NEW `tests/chatgpt_capabilities.rs` activates:

- every static table row;
- all ten official REST and three official WebSocket targets under ChatGPT,
  with exact error metadata and zero upstream contact;
- private V1/Frameless call-create and sideband success;
- private standalone and API-key-client private rejection;
- session evidence × negotiated dialect, including absent/opaque compatibility;
- body/permit zero-consumption for pre-body rejections;
- byte-identical opaque GA and private events, proving no semantic converter.

## Verification

```bash
cargo test --test chatgpt_capabilities
cargo test --test call_create --test sideband
cargo test --all-features
```
