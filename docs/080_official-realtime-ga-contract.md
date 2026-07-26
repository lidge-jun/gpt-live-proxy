# 080 — Official OpenAI Realtime GA compatibility contract

Research snapshot: 2026-07-26. This document supersedes the scope statement in
`002` D1b. The target is no longer only the OpenCodex call-create/sideband relay;
it is a base-URL-compatible proxy for the current public Realtime API, with the
OpenCodex/GPT-Live aliases retained as an additional profile.

## Primary sources

- [Realtime overview](https://developers.openai.com/api/docs/guides/realtime)
- [WebSocket guide](https://developers.openai.com/api/docs/guides/realtime-websocket)
- [WebRTC guide](https://developers.openai.com/api/docs/guides/realtime-webrtc)
- [Server-side controls](https://developers.openai.com/api/docs/guides/realtime-server-controls)
- [Realtime client events](https://developers.openai.com/api/reference/resources/realtime/client-events)
- [Realtime server events](https://developers.openai.com/api/reference/resources/realtime/server-events)
- [Transcription guide](https://developers.openai.com/api/docs/guides/realtime-transcription)
- [Translation guide](https://developers.openai.com/api/docs/guides/realtime-translation)
- [SIP guide](https://developers.openai.com/api/docs/guides/realtime-sip)
- OpenAPI `2.3.0`, fetched through the official developer-docs service on
  2026-07-26. The exact tool calls, canonical path inventory, operation IDs
  available from per-path fetches, and SHA-256 are recorded in `083`.

The local OpenCodex and `~/Developer/codex/016_realtime-voice` records remain
authoritative for the private Quicksilver/Frameless behavior only. They do not
override the public GA contract.

## Public HTTP surface

The reproducible official OpenAPI inventory in `083` contains these Realtime
operations, all under `/v1`:

| Method | Path | Request | Success |
|---|---|---|---|
| `POST` | `/realtime/calls` | multipart `sdp` + `session`, or raw `application/sdp` with an ephemeral credential | `201`, SDP answer, `Location` |
| `POST` | `/realtime/calls/{call_id}/accept` | JSON Realtime session | `200` |
| `POST` | `/realtime/calls/{call_id}/reject` | optional JSON SIP status | `200` |
| `POST` | `/realtime/calls/{call_id}/refer` | JSON `target_uri` | `200` |
| `POST` | `/realtime/calls/{call_id}/hangup` | empty | `200` |
| `POST` | `/realtime/client_secrets` | JSON expiry + GA session | `200` JSON |
| `POST` | `/realtime/sessions` | legacy session-token request | `200` JSON |
| `POST` | `/realtime/transcription_sessions` | transcription session request | `200` JSON |
| `POST` | `/realtime/translations/client_secrets` | translation session request | `200` JSON |

The translation guide additionally documents
`POST /v1/realtime/translations/calls`. It is absent from the canonical
nine-path endpoint inventory whose exact bytes and hash are recorded in `083`.
The implementation will support the documented path as an opaque call relay,
but the conformance record must label its status/header schema as a
guide/OpenAPI drift until the official spec exposes it.

## Connection surface

| Use | Public URL | Authentication |
|---|---|---|
| standalone voice-agent WebSocket | `/v1/realtime?model=gpt-realtime-2.1` | server `Authorization`, or browser ephemeral WebSocket subprotocol |
| existing WebRTC/SIP sideband | `/v1/realtime?call_id=rtc_...` | `Authorization`; `model` is ignored |
| translation WebSocket | `/v1/realtime/translations?model=gpt-realtime-translate` | standard or ephemeral credential |
| WebRTC voice call | `POST /v1/realtime/calls` | standard key with multipart, or ephemeral key with raw SDP |
| WebRTC translation call | `POST /v1/realtime/translations/calls` | translation client secret with raw SDP |

Browser WebSocket authentication uses these subprotocol tokens:

```text
realtime
openai-insecure-api-key.<ephemeral-key>
openai-organization.<organization-id>    # optional
openai-project.<project-id>              # optional
```

A proxy must participate in subprotocol selection. Copying the inbound header
without returning the selected protocol downstream is not compatible.

## GA session and event contract

The current public voice-agent session has `type: "realtime"`. The current
model shown in official examples is `gpt-realtime-2.1`; the session shape places
audio configuration below `audio.input` and `audio.output` and may carry
reasoning, tools, MCP-related configuration, prompt references, tracing, and
truncation settings.

The standard client event set currently has eleven event types:

```text
session.update
input_audio_buffer.append
input_audio_buffer.commit
input_audio_buffer.clear
conversation.item.create
conversation.item.retrieve
conversation.item.truncate
conversation.item.delete
response.create
response.cancel
output_audio_buffer.clear
```

The server reference currently exposes 46 event types across session,
conversation, input-buffer/transcription, response text/audio/tool output,
output-buffer, MCP, rate-limit, and error families. The proxy contract is
forward-compatible opacity: it does not deserialize or normalize public GA
events. Text remains text, binary remains binary, and unknown future events
remain byte-identical.

Translation is a distinct lifecycle. It uses a dedicated path and events such
as `session.input_audio_buffer.append`, `session.close`, translated audio and
transcript deltas, and `session.closed`; it does not use the normal assistant
Response lifecycle. Transcription likewise has its own session type and emits
input transcription deltas/completion without model speech output.

## Header contract

Request headers with public meaning are classified explicitly:

- credentials: `authorization` and credential-bearing WebSocket subprotocols;
- account routing: `openai-organization`, `openai-project`;
- abuse attribution: `openai-safety-identifier`;
- compatibility: `openai-beta` for legacy official clients and
  `openai-alpha` for the private GPT-Live profile;
- content negotiation: `content-type`, `accept`;
- optional idempotency and trace headers only when a documented consumer needs
  them.

The proxy never clones an inbound header map. Hop-by-hop headers, cookies,
proxy admission credentials, and arbitrary caller headers do not cross.

Response compatibility requires more than the old two-header Live rule. The
safe response allowlist must preserve at least `content-type`, `location`,
`retry-after`, request-correlation IDs, and documented OpenAI rate-limit
metadata while still dropping cookies and hop-by-hop headers.

## Definition of base-URL compatibility

For API-key mode, a supported official client is compatible when changing only
its HTTP/WS API host makes the same route, query, body bytes, content type,
credential semantics, status, safe response headers, and WebSocket event stream
reach the upstream.

SIP trunk configuration and incoming webhook delivery are not selected by an
OpenAI API base URL and are not proxied. Their four HTTP call controls and
`call_id` monitor WebSocket are inside scope.

ChatGPT/GPT-Live mode is a separate capability profile. It may expose an
official-looking route only when the private backend can provide equivalent
semantics or when a tested adapter performs a lossless mapping. Unsupported
translation, transcription, SIP, credential-minting, reasoning, or event
semantics return a stable capability error; they never return fake success.
