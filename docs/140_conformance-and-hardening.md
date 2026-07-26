# 140 — Conformance, resource, privacy, and documentation hardening

Work-phase: `wp6-hardening`. Depends on all behavior phases.

## Official-client conformance

### NEW `conformance/node/package.json`

Pin `openai` to the registry-proven exact version `6.49.0`; no ranges. Scripts
start the Rust proxy and hermetic mock upstream on ephemeral ports and exercise
the official SDK with only base URL/API key configuration plus the documented
Realtime helpers available in that version. The lockfile is reviewed and no
install script is permitted without explicit justification.

### NEW `conformance/node/realtime.mjs`

Black-box scenarios:

- REST client-secret/session operations and upstream error mapping;
- multipart and raw-SDP call creation;
- standalone WebSocket, existing-call sideband, and translation WebSocket;
- standard and browser subprotocol authentication;
- `session.update`, audio append, response, tool, error, and unknown-event
  transcripts.

The mock captures exact wire artifacts; the harness never contacts OpenAI and
never prints credentials.

### NEW `tests/fixtures/official/`

Versioned fixture manifest records official source URL, fetch date, schema/event
name, and SHA-256. Fixtures are not generated from Rust enums. Standard client
and server event-name inventories include the current 11/46 sets; translation
fixtures are separate.

## Resource and egress hardening

### MODIFY `src/config.rs`, `src/app.rs`, relay modules

- enforce request-read, response, WS connect, WS send, and idle deadlines;
- bound request concurrency, active WebSockets, pre-open bytes, and frame bytes;
- return `429` plus `Retry-After` when local permits are exhausted;
- disable redirects and prove cross-host redirects never receive auth/body;
- expose readiness separately from liveness if permit/drain state makes the
  service unable to accept work;
- document single-principal-only network operation unless call-ID ownership is
  implemented in this phase.

Call ownership decision: remain explicitly single-principal in version 0.1.x.
Multi-principal call-ID binding requires an identity store and is outside this
proxy's stateless architecture. A network bind therefore requires one shared
principal and says so at startup/README; it must not claim tenant isolation.

## Privacy

### MODIFY `src/observability/frame_log.rs`, `docs/050_observability.md`

Remove the payload `context` excerpt. Record direction, kind, byte count,
replacement/UTF-8 fault flag, fault offset, and a keyed or non-reversible digest
only. Canary tests cover headers, HTTP bodies, text/binary frames, close reasons,
and subprotocols. No test artifact may contain the canary.

## Test strength

- property tests: URL/query normalization, call IDs, header casing/duplicates,
  multipart boundaries, limit arithmetic;
- mutation checks: route variants, credential policy, header allowlists, AVAS
  truth table, cap comparisons, pump outcomes, capability matrix;
- deterministic fault peers: upstream/client reset during read/send and stalled
  handshake/sinks;
- literal 16 MiB and +1 HTTP boundary;
- bounded load/soak: permits and task counts return to baseline.

No skip, retry-as-fix, threshold reduction, assertion deletion, or fixture
generated from the implementation can satisfy this phase.

## CI changes

Modify `.github/workflows/ci.yml` with immutable action commit SHAs only:

1. existing Rust Linux/macOS and MSRV jobs;
2. hermetic Node SDK conformance;
3. dependency and secret scan using repository-approved pinned tools;
4. selective mutation job where runtime cost is acceptable, otherwise a
   checked-in mutation script executed in the main test job;
5. failure artifacts containing metadata only.

Run `actionlint` and verify every new action SHA resolves to a commit, not an
annotated tag object.

## Documentation sync

Update README route/profile matrices, configuration, official Node/WebSocket/
WebRTC examples, security model, compatibility limits, and verification counts.
Update `002`, `050`, and `060` so no authority retains the old relay-only claim.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
npm ci --prefix conformance/node
npm test --prefix conformance/node
actionlint
gitleaks detect --source . --no-banner --redact
```
