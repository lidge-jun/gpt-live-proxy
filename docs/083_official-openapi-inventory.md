# 083 — Reproducible official OpenAPI inventory

Snapshot date: 2026-07-26. Source service: the OpenAI developer-docs MCP backed
by `developers.openai.com` and `api.openai.com`.

## Retrieval record

The inventory was fetched with
`mcp__openaiDeveloperDocs__list_api_endpoints({})`. The result reported base URL
`https://api.openai.com/v1`, 181 total paths, and the nine Realtime paths below.
Per-path schemas were requested with
`mcp__openaiDeveloperDocs__get_openapi_spec({"url": <absolute-url>})`.
Successful schemas identify OpenAPI `3.1.0`, API document version `2.3.0`.

The control-path entries are present in `list_api_endpoints`; on this snapshot,
`get_openapi_spec` returned `No OpenAPI spec found` for URLs containing the
literal `{call_id}` placeholder. Their operation IDs are therefore recorded as
`not returned`, not guessed. The path inventory itself remains verified.

## Canonical inventory

Canonicalization is UTF-8, one path per line in the order returned by the
official tool, with a final newline:

```text
/realtime/calls
/realtime/calls/{call_id}/accept
/realtime/calls/{call_id}/hangup
/realtime/calls/{call_id}/refer
/realtime/calls/{call_id}/reject
/realtime/client_secrets
/realtime/sessions
/realtime/transcription_sessions
/realtime/translations/client_secrets
```

SHA-256:
`dd19637621a18d3e5a8067dd309653a5fa7f621e407977e1408e7098210cff09`.

Reproduction command after copying the block without the fence:

```bash
printf '%s\n' \
  '/realtime/calls' \
  '/realtime/calls/{call_id}/accept' \
  '/realtime/calls/{call_id}/hangup' \
  '/realtime/calls/{call_id}/refer' \
  '/realtime/calls/{call_id}/reject' \
  '/realtime/client_secrets' \
  '/realtime/sessions' \
  '/realtime/transcription_sessions' \
  '/realtime/translations/client_secrets' | shasum -a 256
```

## Method and operation-ID evidence

| Method | Path | OpenAPI operation ID |
|---|---|---|
| `POST` | `/realtime/calls` | `create-realtime-call` |
| `POST` | `/realtime/calls/{call_id}/accept` | not returned by per-path tool |
| `POST` | `/realtime/calls/{call_id}/hangup` | not returned by per-path tool |
| `POST` | `/realtime/calls/{call_id}/refer` | not returned by per-path tool |
| `POST` | `/realtime/calls/{call_id}/reject` | not returned by per-path tool |
| `POST` | `/realtime/client_secrets` | `create-realtime-client-secret` |
| `POST` | `/realtime/sessions` | `create-realtime-session` |
| `POST` | `/realtime/transcription_sessions` | `create-realtime-transcription-session` |
| `POST` | `/realtime/translations/client_secrets` | `create-realtime-translation-client-secret` |

The method and body semantics for the four controls are independently anchored
by the official [server-side controls guide](https://developers.openai.com/api/docs/guides/realtime-server-controls)
and their individual reference pages. The missing operation-ID strings are not
used as runtime routing inputs.

## Guide/OpenAPI drift

The official [Realtime translation guide](https://developers.openai.com/api/docs/guides/realtime-translation)
documents `POST /v1/realtime/translations/calls`. That path is not present in
the exact nine-line OpenAPI endpoint inventory above. This proves inventory
absence only for this dated snapshot; it does not claim that the guide path is
unsupported. The compatibility plan implements it and labels its exact
response schema as guide-derived until a later OpenAPI snapshot includes it.
