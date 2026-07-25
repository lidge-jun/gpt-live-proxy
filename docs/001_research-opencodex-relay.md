# 001 — OpenCodex relay behavior (research)

Research notes on the relay in [`lidge-jun/opencodex`](https://github.com/lidge-jun/opencodex)
at tree state `5a550867`. Citations use `path:line` relative to that repository,
so each one resolves at
`https://github.com/lidge-jun/opencodex/blob/5a550867/<path>#L<line>`.

Verification run by the explorer against that tree:

```text
bun test tests/server-live.test.ts tests/relay-eager.test.ts
30 pass, 0 fail, 142 assertions
```

## 1. Inbound routes

| Method | Path | Behavior |
|---|---|---|
| `POST` | `/v1/live` | GPT-Live / Frameless call-create |
| `POST` | `/v1/realtime/calls` | identical handler; the public Realtime alias |
| WS upgrade | `/v1/live/{callId}` | sideband, style `frameless-path` |
| WS upgrade | `/v1/realtime/calls/{callId}` | sideband, style `realtime-calls-path` |
| WS upgrade | `/v1/realtime?call_id={callId}` | sideband, style `realtime-query` |

(`src/server/index.ts:581-612`, `src/server/index.ts:614-649`)

Both call-create routes disable the normal request timeout, reject while draining, run admission auth, enforce origin policy, call `handleLive`, then add CORS. Anything else under `/v1/*` falls through to the JSON unknown-endpoint guard (`src/server/index.ts:652-658`).

Trailing slashes are accepted on all three sideband paths. The decoded call id must match `^[A-Za-z0-9_-]{1,128}$`; path ids go through `decodeURIComponent`, query ids are trimmed. An id that decodes successfully but fails the pattern makes the parser return `null` and the request falls to the 404 guard (`src/server/live.ts:137-142`, `src/server/live.ts:178-197`).

**Exception worth porting deliberately:** a *malformed* percent escape (for example `/v1/live/%zz`) makes `decodeURIComponent` **throw** rather than return `null`, and the parser does not catch it — so the TypeScript implementation has no Live-specific mapping for that input. The Rust port uses a fallible decode and maps a decode failure to the same `404` as a pattern failure, which is a deliberate, tested divergence rather than an accident.

Successful downstream upgrade is `101`; a failed upgrade is `426` (`src/server/index.ts:619-649`).

## 2. call-create algorithm

Order of operations (`src/server/index.ts:584-611`, `src/server/live.ts:454-512`):

1. draining → `503` plain text `Service shutting down` with `Retry-After: 5`
2. admission auth (`data-plane`)
3. origin policy
4. read body under the 16 MiB cap
5. resolve provider and upstream credentials
6. transform and send upstream
7. buffer the upstream response under the 16 MiB cap and return it

Body reading precedes provider resolution, so an oversized or malformed body is rejected before voice-auth resolution (`src/server/live.ts:459-465`).

### 2.1 Capped body read

`readBodyCapped` reads incrementally, skips empty chunks, and cancels the reader as soon as `total > maxBytes`; an absent body is zero bytes (`src/server/live.ts:286-327`). Failures map to `413` `live request body too large`, `499` `live request canceled by client`, or `400` `live request body unreadable: {error}` (`src/server/live.ts:329-350`).

### 2.2 Provider selection

Eligible providers only (`src/providers/openai-tiers.ts:4-9`, `:32-38`; `src/providers/openai-sidecar.ts:41-49`, `:112-127`):

- forward: id `openai`, adapter `openai-responses`, `authMode: "forward"`, base exactly `https://chatgpt.com/backend-api/codex`;
- keyed: id `openai-apikey`, adapter `openai-responses`, non-forward, base exactly `https://api.openai.com/v1`, non-empty key.

The forward candidate is tried first. An *unusable* forward candidate falls back to keyed; a *mapped* forward-auth exception returns before the keyed branch and blocks fallback (`src/server/live.ts:381-446`). With neither candidate present the relay returns `400` with the exact "Built-in ChatGPT voice needs an OpenAI upstream…" message (`src/server/live.ts:371-379`). With candidates but no usable credential it returns `401` `voice relay needs ChatGPT auth (Authorization header) or an OpenAI API-key provider` (`src/server/live.ts:447-451`).

Pool errors map to `429` cooldown, `409` thread-affinity expiry, and `401` reauthentication / pool authentication (`src/server/live.ts:393-415`).

### 2.3 Upstream URL construction

```text
forward + backend shape : {base}/realtime/calls?intent=quicksilver&architecture=avas
forward + non-backend   : {base}/live                     (no AVAS query)
keyed                   : {base minus /v1}/v1/realtime/calls?intent=quicksilver&architecture=avas
```

(`src/server/live.ts:152-170`). `withAvasQuery` leaves the URL alone only when both `intent=` and `architecture=` are already present; otherwise it appends the whole literal with `?` or `&` as appropriate (`src/server/live.ts:156-159`).

### 2.4 Multipart → JSON rewrite

Applies only when the relay is forward, the provider is a `/backend-api` backend, and the lowercased inbound content type contains `multipart/form-data`; the default content type when absent is `application/octet-stream` (`src/server/live.ts:459-485`). The rewrite requires a string `sdp`, emits `{ "sdp": ... }` when `session` is absent, otherwise parses the string `session` as JSON and emits `{ "sdp": ..., "session": <value> }` with content type `application/json` (`src/server/live.ts:238-284`).

Four distinct `400` messages exist (`src/server/live.ts:242-279`):

```text
ChatGPT voice relay could not parse multipart call-create body
ChatGPT voice relay expects multipart field sdp on call-create
ChatGPT voice relay expected a string multipart session field
ChatGPT voice relay expected JSON in the multipart session field
```

The keyed branch never rewrites; it forwards body and boundary verbatim (`src/server/live.ts:472-485`).

### 2.5 Upstream response

Always `POST`, with a signal combining client cancellation and a 120 s timeout (`src/server/live.ts:467-497`). The response is buffered to 16 MiB, and only non-empty `content-type` and `location` are relayed back; status and body bytes are preserved, everything else is dropped (`src/server/live.ts:501-512`). Failures map to `502` oversize, `499` client abort, `504` `live upstream timed out`, and `502` `live relay failed: {error}` (`src/server/live.ts:513-530`).

## 3. Header ownership

Client protocol whitelist, empty values omitted (`src/server/live.ts:57-71`, `:128-135`):

```text
openai-alpha, x-session-id, session-id, thread-id, originator, x-oai-attestation
```

Effective precedence is whitelist → provider static headers → resolved auth → proxy-owned `content-type` for HTTP (`src/server/live.ts:418-445`, `:467-495`). In keyed mode the API key cannot be overridden by client or provider config. `x-openai-fedramp` and arbitrary client headers are not forwarded (`tests/server-live.test.ts:218-256`). The same resolved header map is used for the sideband upgrade (`src/server/index.ts:640-647`).

## 4. Sideband upstream URL

Backend calls always join the public API host, reusing the same bearer (`src/server/live.ts:49-55`, `:208-220`):

```text
frameless-path       → wss://api.openai.com/v1/live/{callId}
realtime-calls-path  → wss://api.openai.com/v1/realtime/calls/{callId}
realtime-query       → wss://api.openai.com/v1/realtime?intent=quicksilver&call_id={encoded}
```

Non-backend providers normalize a trailing `/v1` and use the provider root with the same three shapes; `https` → `wss`, `http` → `ws`, other schemes unchanged (`src/server/live.ts:172-175`, `:222-235`).

## 5. Sideband pumping

The upstream socket is created when the downstream `open` callback fires, i.e. after the downstream upgrade is accepted (`src/server/index.ts:634-648`, `:668-677`). Client frames arriving before the upstream is open are queued and flushed in insertion order on open (`src/server/index.ts:173-185`, `:681-703`). The queue holds 32 frames; the 33rd closes both sides `1009` `too many pending frames` (`src/server/index.ts:131-148`, `:684-692`). WebSocket idle timeout is disabled with `0` (`src/server/index.ts:668-669`).

Frames are relayed as whole messages with no parsing, decoding, or re-encoding; text stays text and binary stays binary, including byte-offset-exact slicing of typed-array views (`src/server/index.ts:186-197`, `:681-703`). Tests pin UTF-8 text, binary, and a ~1.3 MiB Korean message as byte-identical in both directions (`tests/server-live.test.ts:606-716`).

There is no post-open backpressure accounting and no upstream connect timeout; the only bound is the 32-frame pre-open queue (`src/server/index.ts:173-197`). The Responses-side `MAX_WS_FRAME_BYTES = 50 MiB` never applies to Live (`src/server/index.ts:129-131`).

Close and error mapping (`src/server/index.ts:150-208`, `:820-824`):

| Event | Result |
|---|---|
| missing upstream URL | `1011` `missing upstream` |
| outbound constructor throws | `1011` `upstream connect failed` |
| pre-open queue overflow | `1009` `too many pending frames` |
| upstream not open on send | `1011` `upstream not open` |
| upstream send throws | `1011` `upstream send failed` |
| downstream send throws | `1011` `client send failed` |
| upstream `error` | `1011` `upstream error` |
| upstream closes | downstream closes with upstream code/reason; falsy code → `1000`, falsy reason → `""` |
| client closes | upstream closes `1000` `client closed`, regardless of the client's code |

Close propagation is therefore asymmetric.

## 6. CORS

`OPTIONS` is answered before route matching: `204` for an allowed origin, `403` otherwise, with no API-key check (`src/server/index.ts:289-298`). Response headers (`src/server/auth-cors.ts:18-22`, `:78-89`):

```text
Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS
Access-Control-Allow-Headers: Content-Type, Authorization, X-OpenCodex-API-Key, X-Api-Key,
  Anthropic-Version, Anthropic-Beta, ChatGPT-Account-Id, OpenAI-Alpha, X-Session-Id,
  Session-Id, Thread-Id, Originator, X-OAI-Attestation
Vary: Origin
```

Rejected HTTP call-create is `403` `cross-origin data-plane request blocked`; rejected upgrades are `403` `WebSocket upgrade blocked: non-local Origin` (`src/server/index.ts:597-599`, `:626-630`).

## 7. Named constants

| Constant | Value |
|---|---|
| `LIVE_UPSTREAM_TIMEOUT_MS` | `120_000` |
| `LIVE_REQUEST_MAX_BYTES` | `16 * 1024 * 1024` |
| `LIVE_RESPONSE_MAX_BYTES` | `16 * 1024 * 1024` |
| `LIVE_RELAY_HEADERS` | `["content-type", "location"]` |
| `LIVE_AVAS_QUERY` | `intent=quicksilver&architecture=avas` |
| `LIVE_SIDEBAND_API_ROOT` | `https://api.openai.com/v1` |
| `LIVE_FRAME_LOG_ENV` | `OCX_LIVE_FRAME_LOG` |
| `LIVE_FRAME_LOG_CONTEXT_CHARS` | `24` |
| `LIVE_CALL_ID_RE` | `^[A-Za-z0-9_-]{1,128}$` |
| sideband pending max | `32` frames |
| WebSocket idle timeout | `0` (disabled) |

(`src/server/live.ts:40-55`, `:82-89`, `:137`; `src/server/index.ts:129-131`, `:668-669`)

## 8. Frame forensics

Disabled entirely when the env var is unset. When set to a path, every relayed frame appends one JSONL record (`src/server/live.ts:93-125`):

```json
{ "ts": "<ISO-8601>", "dir": "c2u|u2c", "kind": "text|binary", "bytes": 123, "fffd": false, "context": "<optional>" }
```

`context` appears only when the decoded representation contains U+FFFD (`src/server/live.ts:82-90`, `:114-122`). The TypeScript window is computed in **UTF-16 code units** with an exclusive end: `slice(max(0, idx - 24), min(len, idx + 24))`, so it yields up to 24 units before the replacement character and at most 23 after it — not "24 characters on each side". The Rust port works in Unicode scalar values with char-boundary clamping, which is a documented, tested divergence; the invariant that survives is a *bounded* excerpt. Note that "bounded" is not "empty": a frame shorter than the window is captured whole, in the source as well as in the port.

A clean frame writes no payload, headers and URLs never appear, and a logging exception can never break the relay. The excerpt around a replacement character does contain adjacent transcript text and is therefore sensitive diagnostic data (`src/server/live.ts:73-80`).

## 9. Test-derived contract inventory

`tests/server-live.test.ts` pins: multipart → backend JSON with the AVAS URL; keyed multipart preservation; the protocol-header whitelist and its omission behavior; `/v1/realtime/calls` as an alias; SDP-only bodies; the voice preflight header list; the missing-upstream `400`; the non-upgrade `GET /v1/live` `404`; pool credentials overriding caller bearer/account; sideband upgrade URL, headers and bidirectional echo; parser mappings; UTF-8/binary/large-frame transparency; metadata-only forensics (`tests/server-live.test.ts:140-419`, `:421-604`, `:606-815`).

Gaps the Rust suite should additionally cover: all three sideband styles with trailing slashes and full regex boundaries; oversized and invalid call ids; every multipart error message; exact cap boundary (16 MiB pass, +1 reject) on both directions; upstream timeout, connect failure, client cancellation, response-header filtering; forward-unusable → keyed fallback versus mapped-error precedence; header conflict precedence; the 32-frame queue and the 33rd-frame close; every close/error mapping including asymmetry; binary type preservation rather than mere byte equality; forensics append failure staying non-fatal.

`relay.ts` / `relay-eager.ts` are **not** part of the GPT-Live path — they serve HTTP SSE relays and must not be transplanted into the sideband design (`src/server/index.ts:80-94`, `:150-209`).

## 10. Error inventory

Every emitted status with its exact message, JSON `type`, and `code`. This table is the contract; the Rust port implements it verbatim except where the "port" column says otherwise (`src/server/index.ts:589-611`, `:620-657`; `src/server/live.ts:329-350`, `:371-451`, `:489-526`; `src/lib/errors.ts:79-190`).

| Status | Message | `type` | `code` | Port |
|---:|---|---|---|---|
| upstream | (upstream body verbatim; only `content-type` + `location` kept) | — | — | same |
| `400` | `Built-in ChatGPT voice needs an OpenAI upstream (ChatGPT login or an OpenAI API-key provider), but none is configured in opencodex. Routed providers cannot serve voice call-create.` | `invalid_request_error` | `invalid_request_error` | reworded for the standalone service, same status |
| `400` | `ChatGPT voice relay could not parse multipart call-create body` | `invalid_request_error` | `invalid_request_error` | same |
| `400` | `ChatGPT voice relay expects multipart field sdp on call-create` | `invalid_request_error` | `invalid_request_error` | same |
| `400` | `ChatGPT voice relay expected a string multipart session field` | `invalid_request_error` | `invalid_request_error` | same |
| `400` | `ChatGPT voice relay expected JSON in the multipart session field` | `invalid_request_error` | `invalid_request_error` | same |
| `400` | `live request body unreadable: {error}` | `invalid_request_error` | `invalid_request_error` | same |
| `401` | `opencodex API key required` | `authentication_error` | `invalid_api_key` | reworded to `gpt-live-proxy API key required`, same status/type/code |
| `401` | `OpenCodex admission credentials cannot be forwarded upstream` | `authentication_error` | `invalid_api_key` | reworded to `gpt-live-proxy admission credentials cannot be forwarded upstream`, same status/type/code |
| `401` | `Selected Codex account needs reauthentication` | `authentication_error` | `invalid_api_key` | **omitted** (pool-only) |
| `401` | `CodexPoolAuthenticationError.message` (dynamic) | `authentication_error` | `invalid_api_key` | **omitted** (pool-only) |
| `401` | `voice relay needs ChatGPT auth (Authorization header) or an OpenAI API-key provider` | `authentication_error` | `invalid_api_key` | reworded, same status |
| `403` | `cross-origin data-plane request blocked` | `invalid_request_error` | `origin_rejected` | same |
| `403` | `WebSocket upgrade blocked: non-local Origin` | `invalid_request_error` | `origin_rejected` | same |
| `404` | `Unknown endpoint: {METHOD} {pathname}` | `invalid_request_error` | `invalid_request_error` | same |
| `409` | `Codex thread account affinity expired; start a new session` | `invalid_request_error` | `invalid_request_error` | **omitted** (pool-only) |
| `413` | `live request body too large` | `invalid_request_error` | `invalid_request_error` | same |
| `426` | `WebSocket upgrade failed` | `invalid_request_error` | `upgrade_required` | same |
| `429` | `Selected Codex account is cooling down` | `rate_limit_error` | `rate_limit_exceeded` | **omitted** (pool-only) |
| `499` | `live request canceled by client` | `invalid_request_error` | `client_closed_request` | same |
| `502` | `live response too large ({total} bytes)` | `server_error` | `upstream_server_error` | same |
| `502` | `live relay failed: {error}` | `server_error` | `upstream_server_error` | same |
| `503` | `Service shutting down` (plain text, `Retry-After: 5`) | — | — | same |
| `504` | `live upstream timed out` | `server_error` | `upstream_server_error` | same |

JSON envelope shape in all cases: `{"error":{"message":…,"type":…,"code":…}}`.

### Port-only rows

One row exists in this port that has no counterpart in the source. It is listed
here so the "every emitted response is pinned by §10" rule stays true.

| Status | Message | `type` | `code` | Why |
|---:|---|---|---|---|
| `400` | `ambiguous Authorization: send exactly one credential` | `invalid_request_error` | `invalid_request_error` | A repeated `Authorization` header is ambiguous for a relay that must forward exactly one credential, and inspecting only the first value is a duplicate-header bypass: a caller can put a real upstream bearer first and the proxy's own admission secret second. The source reads only the first value and has no such row; refusing the request outright is a deliberate hardening, and it is called out here rather than left as an undocumented status. |

Note on the `403` rows: the handler passes the *internal* type string `origin_rejected` to `formatErrorResponse`, and `classifyError` intercepts it ahead of the generic permission branch, emitting `type: "invalid_request_error"` with `code: "origin_rejected"` (`src/lib/errors.ts:136-137`, `src/server/index.ts:346`, `:598`). The wire values — not the internal token — are what this table pins.

## 11. Trust boundary

The relay's *downstream* trust boundary is separate from upstream credential selection and must not be skipped (`src/server/auth-cors.ts:121-177`):

- **Loopback bind → admission auth disabled.** A non-loopback bind requires a credential.
- Admission accepts the first non-empty value among `X-OpenCodex-API-Key`, bearer `Authorization`, and `X-Api-Key`, compared in constant time against the configured token.
- A bearer that *is* the admission secret must never be forwarded upstream; that returns `401` `OpenCodex admission credentials cannot be forwarded upstream`. Callers needing both put the proxy credential in the dedicated header and the upstream bearer in `Authorization` (`src/server/auth-cors.ts:151-160`).
- Origin policy: with auth not required, the request `Host` must be loopback on the configured port; missing `Origin`, loopback origins, and configured extra origins are accepted. With auth required, host-loopback validation is not applied (`src/server/auth-cors.ts:24-39`, `:59-75`).
- Draining: while shutting down, both routes answer `503` with plain-text `Service shutting down` and `Retry-After: 5`, wrapped in CORS (`src/server/index.ts:584-590`).

This section is the specification for phase 015.
