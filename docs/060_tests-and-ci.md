# 060 — Tests, conformance, security, and CI

Work-phase `wp6-hardening`. The executable commands and workflow in this file
are the release evidence for the protocol contracts in `080` through `140`.

## Evidence layers

The Rust suites keep behavior ownership close to the implementation while
integration tests use real loopback sockets for claims a unit test cannot make.
They cover:

- exact REST route/query/header/body relay and profile capability errors;
- official standalone, existing-call, translation, and private WebSockets;
- multipart/raw-SDP WebRTC call-create followed by `Location` sideband join;
- request/response/frame/queue caps, deadlines, readiness, and permit recovery;
- public/private idle closure when explicitly configured, with no default idle
  cutoff;
- metadata-only frame forensics under hostile global TRACE filtering;
- fixed-seed properties and 64-round permit/socket/task soak tests.

`PROPTEST_CASES=256` and `PROPTEST_RNG_SEED=20260726` are set by CI. Property
tests exercise the production URL/query, call-ID, header, multipart-boundary,
and checked-arithmetic owners instead of creating a second classifier.

## Official compatibility evidence

The Node harness has two lanes whose names must not be collapsed.

`official-sdk` pins `openai@6.49.0`, `ws@8.21.1`, Node `22.23.1`, and npm
`10.9.8`. With only API-key and `baseURL` configuration it exercises:

- `client.realtime.clientSecrets.create()`;
- `client.realtime.calls.accept/reject/refer/hangup()`;
- `OpenAIRealtimeWS` standalone and existing-call connections;
- `OpenAIRealtimeWebSocket` browser-style ephemeral subprotocol auth;
- typed and generic event delivery, including an unknown future event.

`official-doc-transport` uses raw `fetch`/`ws` for multipart/raw-SDP
call-create, translation REST/WebSocket, optional browser organization/project
protocols, and `Location` to SDK sideband composition. Those surfaces do not
have corresponding helpers in the pinned SDK. This lane proves documented wire
compatibility, not actual browser media or `RTCPeerConnection` behavior.

The runner binds its mock upstream and proxy to ephemeral loopback ports, waits
on `/readyz`, rejects non-loopback Node egress, and terminates every child on
success or failure. Wire evidence contains method, path, header names, body
length, and SHA-256 only—never credential, header value, body, or frame bytes.

Fixture provenance is independently verified by
`scripts/verify-official-fixtures.mjs`. Each manifest entry records an immutable
SDK package identity plus official source URLs, retrieval date, operation/event
pointers, canonical-inventory SHA-256, checked-in fixture SHA-256, SDK version,
npm integrity, and npm shasum. The verifier checks the reviewed snapshot's
internal provenance and byte stability without network access, reads the
canonical inventory from a separate reviewed source-extraction file, and
requires the JSON fixture to match it exactly. A source refresh must re-extract
and review the official pages. Fixtures are not generated from Rust enums.

## Deterministic mutation gate

`scripts/mutation-check.mjs` copies the source tree into an isolated temporary
directory and applies one fixed, bounded mutation at a time. The seven mutation
families are:

1. route selection;
2. capability support;
3. credential policy;
4. response-header allowlist;
5. AVAS query construction;
6. cap boundary comparison;
7. pump terminal outcome.

Each source anchor must exist and each owner-focused test command must fail for
its mutant. A missing anchor, surviving mutant, skipped test, or timeout fails
the job. This is intentionally a checked-in deterministic runner rather than a
floating mutation tool or statistical threshold.

## Workflow jobs

`.github/workflows/ci.yml` runs on every pull request and push to `main` with
`permissions: contents: read`, workflow-level cancellation, and finite job
timeouts.

### `rust (ubuntu-latest | macos-latest)`

- immutable checkout, Rust toolchain, and cache action commits;
- locked metadata, clippy, and full-feature test graph;
- formatting after a locked dependency-graph check;
- cache writes only for trusted pushes to `main`.

### `msrv (1.86)`

- exact declared minimum Rust version;
- `cargo check --locked --all-targets --all-features`;
- trusted-main-only cache writes.

### `official conformance`

- immutable `actions/setup-node` commit
  `820762786026740c76f36085b0efc47a31fe5020`;
- exact Node `22.23.1` and committed npm lockfile;
- `npm ci --ignore-scripts --no-audit --no-fund`;
- locked Rust binary, fixture verification, and both compatibility lanes;
- loopback-only harness with no external OpenAI request.

### `security`

Checkout uses `fetch-depth: 0`. `ci/install-tools.sh` verifies exact release
archives before installing:

| Tool | Version | Linux x86-64 archive SHA-256 |
|---|---:|---|
| actionlint | 1.7.12 | `8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8` |
| gitleaks | 8.30.1 | `551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb` |
| cargo-audit crate | 0.22.2 | `700c2b240f7fd330c24b675fe429f73a5b676531fcc6300400b2b67f155ba12a` |

The job runs actionlint, full-history gitleaks with redaction, `cargo audit`, and
`npm audit --audit-level=high`. Cargo-audit is built with `--locked` from the
verified crates.io archive rather than installed by an unverified pipe.

### `deterministic mutation`

The mutation script has its own 30-minute Ubuntu job and exact Node/Rust setup.
It does not upload mutated trees or test output.

## Artifact and trust policy

No workflow step receives a repository or user secret. Checkout uses
`persist-credentials: false`; untrusted build scripts and proc macros cannot
read the automatic token from `.git/config`. No job uploads raw stdout, frame
logs, mock wire captures, header/body/environment files, or diagnostic bundles.

`/healthz` is process liveness. `/readyz` is unauthenticated readiness and
returns 503 while draining or at request/connection capacity, then 200 after
recovery. Both responses are deliberately content-free with respect to
accounts, credentials, limits, and active calls.

The runtime is single-principal. A non-loopback bind logs
`security_model=single_principal tenant_isolation=false`; an admission token is
access control, not tenant or call-ID ownership.

## Local release-equivalent commands

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo +1.86 check --locked --all-targets --all-features
npm ci --prefix conformance/node --ignore-scripts --no-audit --no-fund
npm ls --prefix conformance/node openai ws --depth=0
cargo build --locked --bin gpt-live-proxy
node scripts/verify-official-fixtures.mjs
npm test --prefix conformance/node
node scripts/mutation-check.mjs
ci/install-tools.sh
actionlint
gitleaks git . --no-banner --redact
cargo audit
npm audit --prefix conformance/node --audit-level=high --ignore-scripts
```

`ci/install-tools.sh` is intentionally Linux x86-64 only because it installs
the exact binaries used by the Ubuntu security job. macOS local development
still runs the Rust and Node gates directly.
