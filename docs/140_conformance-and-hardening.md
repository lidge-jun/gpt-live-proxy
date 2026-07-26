# 140 — Conformance, resource, privacy, and CI hardening

Work-phase: `wp6-hardening`. Depends on `100` through `130`.

## Two conformance lanes

The npm registry was checked on 2026-07-26: `openai@6.49.0` is current and its
integrity is pinned in the lockfile. `ws@8.21.1` satisfies the SDK's `^8.18.0`
peer range and is pinned exactly by this project. Node is `22.23.1` with bundled
npm `10.9.8`.

### Official SDK lane

NEW `conformance/node/` uses the real package with only API key and `baseURL`
configuration:

- `client.realtime.clientSecrets.create()`;
- `client.realtime.calls.accept/reject/refer/hangup()`;
- `OpenAIRealtimeWS` standalone `{ model }` and existing-call `{ callID }`;
- `OpenAIRealtimeWebSocket` browser-style `realtime` + ephemeral subprotocol;
- typed and generic `event` listeners for session, audio, response, tool, error,
  and unknown events.

These are the helpers actually shipped in 6.49.0. The test does not claim SDK
coverage for APIs that package does not expose.

### Official documented-transport lane

Raw `fetch`/`ws` scenarios follow the official guides for:

- multipart and raw-SDP call-create;
- translation client-secret/call/WebSocket;
- optional browser organization/project protocols;
- returned `Location` → call ID → SDK sideband composition.

This proves documented wire/base-URL compatibility, not browser media-plane or
`RTCPeerConnection` behavior.

`runner.mjs` starts a mock upstream and the Rust proxy on ephemeral loopback
ports, waits on readiness, runs both lanes, and always terminates children. An
egress guard rejects non-loopback Node connections; every proxy upstream base is
also asserted loopback. Captures expose method, path, header names, byte length,
and SHA-256 only—never credentials or body/frame bytes.

Install and verify with:

```bash
npm ci --prefix conformance/node --ignore-scripts --no-audit --no-fund
npm ls --prefix conformance/node openai ws --depth=0
cargo build --locked --bin gpt-live-proxy
npm test --prefix conformance/node
```

## Fixture provenance

NEW `tests/fixtures/official/manifest.json` and
`scripts/verify-official-fixtures.mjs` record and verify, per fixture:

- exact SDK package/version plus official source URLs and retrieval date;
- operation/event inventory and JSON Pointer where applicable;
- a separate reviewed canonical source-extraction file with its SHA-256, plus
  the checked-in JSON fixture SHA-256 as a distinct field;
- SDK version, npm integrity, and shasum.

Fixtures are reviewed source snapshots, never generated from Rust enums. The
offline verifier proves their internal provenance and byte stability and fails
on a hash, count, duplicate, source-extraction/fixture, or manifest mismatch.
Refreshing a source snapshot still requires re-extracting and reviewing the
listed official pages.

## Resource behavior

Existing owners already enforce request-read/upstream-response, WebSocket
connect/send deadlines; request/response/frame/pre-open byte caps; request and
connection semaphores; redirect refusal; cancellation cleanup; and exact
`429`/`Retry-After: 1`. WP6 mutation- and soak-activates them rather than adding
second implementations.

### Opt-in idle policy

The source contract disables WebSocket idle timeout (`0`), and default official
compatibility must remain unchanged. Add `GPT_LIVE_WS_IDLE_TIMEOUT_MS`, default
`0`. A nonzero operator value starts only after upstream connection, is reset by
any received data or control frame on either leg, and closes both legs with
`1001 / idle timeout`. Public and private pumps share one timer implementation;
tests prove socket/permit recovery. No default 300-second cutoff is introduced.

### Readiness and deployment model

`/healthz` remains unauthenticated process liveness. NEW `/readyz` is also
credential-free and returns 503 while draining or while either request or
connection capacity is fully exhausted, then returns 200 after recovery. It
contains no config, account, or credential data.

Non-loopback startup emits one structured warning:
`security_model=single_principal tenant_isolation=false`. Admission auth is
access control, not call-ID ownership. Version 0.1.x does not claim safe
multi-tenant isolation; loopback startup emits no warning.

## Test strength

- fixed-seed `proptest 1.9.0` with `PROPTEST_CASES=256` and
  `PROPTEST_RNG_SEED=20260726` for URL/query normalization, call IDs,
  header casing/duplicates, multipart boundaries, and checked cap arithmetic;
- real-socket exact 16 MiB and 16 MiB+1 request/response boundaries;
- 64-round barrier-driven HTTP/public-WS/private-WS permit soak with baseline
  task/socket/permit recovery after every round;
- deterministic upstream/client reset and send-failure pump outcomes;
- hostile EnvFilter and metadata-only frame-log tests retained unchanged;
- NEW `scripts/mutation-check.mjs` copies the tree to a temporary directory,
  applies seven fixed one-token mutants (route, capability support, credential
  policy, header allowlist, AVAS, cap comparison, pump outcome), and requires
  the owner-focused tests to fail for each.

The checked-in deterministic mutation runner is the CI authority; no floating
mutation tool or threshold is used. A missing source anchor, surviving mutant,
timeout, skipped test, or deleted assertion fails.

## CI

MODIFY `.github/workflows/ci.yml`:

- all actions remain immutable commit SHAs; `actions/setup-node` is pinned to
  `820762786026740c76f36085b0efc47a31fe5020`;
- add workflow concurrency cancellation and job timeouts;
- Rust Linux/macOS and MSRV 1.86 commands use `--locked`;
- Node conformance uses pinned `actions/setup-node` SHA, Node `22.23.1`, lockfile,
  `npm ci --ignore-scripts`, fixture verification, and loopback egress guard;
- Ubuntu security installs
  `https://github.com/rhysd/actionlint/releases/download/v1.7.12/actionlint_1.7.12_linux_amd64.tar.gz`
  at SHA-256 `8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8`
  and
  `https://github.com/gitleaks/gitleaks/releases/download/v8.30.1/gitleaks_8.30.1_linux_x64.tar.gz`
  at SHA-256 `551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb`.
  It installs `cargo-audit 0.22.2 --locked` from the crates.io archive whose
  SHA-256 is `700c2b240f7fd330c24b675fe429f73a5b676531fcc6300400b2b67f155ba12a`,
  then runs full-history gitleaks, cargo audit, and
  `npm audit --audit-level=high`;
- deterministic mutation script runs in its own bounded Ubuntu job;
- no raw logs, wire captures, headers, bodies, frame logs, or environment files
  are uploaded as artifacts. Therefore no upload action is required.

Keep `permissions: contents: read` and `persist-credentials: false`. Cache writes
are limited to trusted main pushes.

## Documentation and verification

Update README and `docs/{002,050,060}` with readiness, opt-in idle, exact SDK vs
documented-transport claims, single-principal security, and executable commands.
Ignore only `/.codexclaw/` tool state; do not hide source or release files.

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo +1.86 check --locked --all-targets --all-features
npm ci --prefix conformance/node --ignore-scripts --no-audit --no-fund
npm test --prefix conformance/node
node scripts/verify-official-fixtures.mjs
node scripts/mutation-check.mjs
actionlint
gitleaks git . --no-banner --redact
```
