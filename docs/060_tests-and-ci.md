# 060 — Integration tests, CI, README

Work-phase `wp7-tests-ci`. Deliverable: an executable proof of every contract claim.

## Test layout

```text
tests/support/mod.rs          mock upstream HTTP + WS servers, request capture
tests/trust_boundary.rs       admission, origin, CORS, draining
tests/call_create.rs          the POST contract
tests/sideband.rs             the WebSocket contract
tests/wire_adapters.rs        session serialization and truth tables
tests/forensics.rs            frame-log privacy
```

The mock upstream is a real `axum` server bound to port 0, recording each received request (method, full URI including query, header map, body bytes) into a shared `Arc<Mutex<Vec<CapturedRequest>>>`. Asserting against a captured URI string is what makes the URL rules testable rather than merely documented.

## Cases

`call_create.rs`

- ChatGPT backend: multipart in → `{"sdp":…,"session":…}` out, URI ends with `/realtime/calls?intent=quicksilver&architecture=avas`, content type `application/json`.
- SDP-only multipart → `{"sdp":…}` with no `session` key.
- API key: multipart preserved byte for byte, URI `/v1/realtime/calls?intent=quicksilver&architecture=avas`.
- All six protocol headers forwarded; empty values dropped; `x-openai-fedramp` absent; client `authorization` replaced by proxy auth.
- `/v1/realtime/calls` behaves identically to `/v1/live`.
- Response relay: only `content-type` and `location` survive; status and body preserved.
- Caps: exactly 16 MiB accepted, 16 MiB + 1 rejected `413`; oversized upstream response `502`.
- Upstream timeout `504`; connect failure `502`.
- Each of the four multipart errors returns its exact message.
- `GET /v1/live` without an upgrade → `404`.
- Client cancellation mid-body and mid-response: asserted by reading the recorded `Outcome::ClientCanceled` from the spawned upstream task's slot, **not** by reading a response — a departed client cannot receive one. `020` §Cancellation defines the ownership model that makes this observable.
- The full error inventory of `001` §10 walked as a table-driven test: every implemented row asserts status, message, `type`, and `code`.

`sideband.rs`

- All three join styles produce the exact upstream URL, for both backend and API-key profiles.
- The `3b766d91` rule: a ChatGPT backend profile still joins `api.openai.com`.
- Protocol headers present on the upstream handshake.
- Text echo, binary echo, and a >1 MiB Korean UTF-8 payload all byte-identical.
- Binary stays binary (asserted on the message variant, not on decoded bytes).
- Upstream close code and reason propagate downstream; a client close arrives upstream as `1000` `client closed`.
- The 33rd pre-open frame closes `1009`.
- Call-id boundaries: 1, 128, 129 chars, empty, slash, encoded slash, unicode.
- Malformed percent escape → `404`, not a panic.
- Connect failure → `1011 upstream connect failed`; missing upstream → `1011 missing upstream`; upstream transport error → `1011 upstream error`; both send-failure paths.
- Upstream close with no code / no reason → downstream sees `1000` / `""`.
- Ping not forwarded, and a following data frame still arrives.

`trust_boundary.rs`

- Loopback bind skips admission; non-loopback rejects a missing credential `401`.
- All three admission header names accepted; an admission bearer never forwarded upstream.
- Origin acceptance and rejection in both auth modes, with the two distinct `403` messages.
- `OPTIONS` → `204` allowed / `403` rejected, never authenticated.
- All six protocol header names present in `Access-Control-Allow-Headers`.
- Draining → `503`, plain text, `Retry-After: 5`.

`wire_adapters.rs` additionally asserts single authority: the AVAS decision in `020` and the join style in `030` both come from `WireAdapter`, and no second copy of either rule exists.

## CI

`.github/workflows/ci.yml`, triggered on push and pull request:

```yaml
jobs:
  check:
    strategy: { matrix: { os: [ubuntu-latest, macos-latest] } }
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1        # v7.0.1
      - uses: dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4  # stable @ 2026-07-26
        with: { components: rustfmt, clippy }
      - uses: Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4     # v2.9.1
      - run: cargo fmt --all -- --check
      - run: cargo clippy --all-targets --all-features -- -D warnings
      - run: cargo test --all-features
```

Every action is pinned to an **immutable commit SHA** with the resolved version recorded in a trailing comment; `@stable` and bare `@v2` are mutable references and are not acceptable.

SHAs were resolved from the GitHub API on 2026-07-26. One trap worth recording: `git/ref/tags/v2.9.1` for `Swatinem/rust-cache` returns `23869a5b…`, which is the **annotated tag object**, not a commit. Dereferencing it via `git/tags/23869a5b…` yields the actual commit `c1937114…`, which is what the workflow pins. A verification step for any future bump is `gh api repos/<owner>/<repo>/commits/<sha>` — it succeeds only for a real commit.

The workflow consumes no secrets and requests no elevated permissions.

## README

Sections: what it is, the two upstream profiles, the route table, the header ownership rule with the `75344b09` reference, the sideband host rule with the `3b766d91` reference, configuration env vars, the frame-log privacy note, and a build/test quickstart.

## Exit criteria

`cargo test` green with a non-trivial test count; `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean; workflow file present and mirroring those three commands.
