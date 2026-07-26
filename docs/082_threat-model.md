# 082 — Realtime compatibility threat model

This is a C4 public-contract and credential-boundary change.

## Assets

- configured OpenAI/ChatGPT bearer, account ID, client ephemeral credentials,
  proxy admission secret, and attestation;
- OpenAI quota, spend, and rate-limit budget;
- SDP, session configuration, call IDs, audio/text/tool/control frames;
- request IDs and diagnostic artifacts;
- process availability and bounded memory/task/connection capacity.

## Entrypoints

- every protected HTTP path listed in `080`;
- voice and translation WebSocket upgrade paths, query values, headers,
  subprotocol tokens, data/control frames, and close reasons;
- environment configuration for bind/base/auth/CORS/limits/forensics;
- upstream redirects, response headers/bodies, delayed handshakes, malformed
  frames, resets, and stalls;
- dependencies, proc macros, and GitHub Actions.

## Trust boundaries and attackers

1. Client to admission/origin boundary: anonymous caller, malicious local
   website, or credential holder sends malformed/duplicated/oversized input.
2. Admission identity to upstream identity: a caller tries to make a proxy
   secret become an OpenAI credential or to substitute its bearer unexpectedly.
3. Proxy to configured upstream: compromised DNS/operator config/redirect tries
   to receive credentials and session data.
4. Downstream upgrade to upstream WebSocket: the upstream rejects or stalls
   after the client has observed success.
5. Shared admission principal to call ID: one user guesses or obtains another
   active call ID.
6. Relay task to logs/files: payload or subprotocol credentials leak through
   diagnostics.
7. PR/dependency to CI runner: an action, build script, or proc macro reads a
   token or alters release evidence.

## Assumptions

- loopback deployment is single-principal;
- network/multi-principal deployment requires a distinct admission credential
  and call ownership, or an explicit documented single-principal restriction;
- custom upstream bases are operator-controlled, not request-controlled;
- public GA and private GPT-Live capability profiles are not interchangeable;
- official schema fixtures are versioned evidence, not generated from the code
  under test.

## Credential decision

Credential meaning is selected by explicit route/profile policy, never token
shape guessing.

- `managed`: the proxy applies its configured upstream bearer. Inbound
  `Authorization` may prove admission only when configuration says so and is
  never copied.
- `client`: an official client bearer or credential-bearing WebSocket
  subprotocol is the upstream credential. Network admission must use the
  dedicated `X-GPT-Live-API-Key` domain; the same header cannot silently serve
  both purposes.
- `ephemeral`: enabled only on official raw-SDP and official WebSocket paths.
  The credential is forwarded as required by the official flow, marked
  sensitive, excluded from every render, and never accepted as a proxy
  admission secret.

Repeated credentials, conflicting credential channels, a proxy secret inside
an upstream credential channel, or an ephemeral credential on a private
ChatGPT path fail before upstream contact.

## Required controls and activation evidence

| Control | Activation scenario | Observable proof |
|---|---|---|
| exact route classifier | `model`, `call_id`, both, neither, duplicate query keys, translation path | exact variant/error; no upstream contact for invalid forms |
| credential-domain separation | managed/client/ephemeral × loopback/network × duplicate headers/subprotocols | exact upstream header set; canary secrets absent elsewhere |
| header allowlists | mixed casing, repeated known/unknown headers, cookies, hop-by-hop values | exact-map equality, not subset assertions |
| public upstream-first WS handshake | upstream 401/403, timeout, wrong subprotocol | downstream never observes a false successful session; mapped failure is deterministic |
| HTTP caps/timeouts | zero, limit, limit+1, slow body, stalled response, concurrent maximum | exact status, no over-cap append, bounded task cleanup |
| WS resource limits | oversized public post-open frame; oversized private pre-open/post-open frame; stalled sink, idle peer, connection flood | close/status, byte and connection counters return to baseline |
| egress/redirect policy | cross-host redirect and denied private/link-local target in production policy | credential/body never reaches denied server |
| call ownership | principal A creates, principal B joins/controls, expired/replayed mapping | B denied before upstream contact |
| privacy | canaries in every credential/header/body/frame/close/subprotocol, including U+FFFD | no canary in logs or CI artifacts |
| forward compatibility | unknown GA JSON event in each direction | byte-identical text frame and unchanged variant |

## Existing controls retained

- protected subrouter middleware and loopback Host/Origin checks;
- constant-time admission comparison and duplicate-Authorization rejection;
- explicit, built-not-cloned upstream headers;
- incremental body/response caps and first-writer-wins cancellation outcome;
- call-ID syntax bounds;
- frame variant preservation and a bounded pre-open queue on the private
  downstream-first legacy path only;
- sensitive header values and redacted diagnostics;
- immutable CI action commits and `persist-credentials: false`.

## Explicit hardening changes

- disable automatic HTTP redirects, then add only an audited same-origin policy
  if the official endpoint demonstrably needs one;
- separate request-read, upstream-response, handshake, send, and idle deadlines;
- cap frame bytes and active connections in addition to frame count;
- preserve safe retry/request-ID/rate-limit response metadata;
- remove payload excerpts from the production forensic record; corruption
  location and a non-reversible digest are sufficient;
- declare single-principal-only operation until call ownership is implemented,
  or bind returned call IDs to an admission principal and TTL before claiming
  multi-principal safety.
