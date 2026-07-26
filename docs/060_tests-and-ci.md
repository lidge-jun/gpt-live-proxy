# 060 — Integration tests, CI, README

Work-phase `wp7-tests-ci`. Deliverable: executable proof for the contract
claims, plus an honest ledger of what is *not* proven — see "Known gaps".

## Test layout

What actually exists, rather than what was planned. Most contract coverage lives
in unit tests next to the code it pins; the integration suites cover what only a
real socket can prove.

```text
tests/support/mod.rs     mock upstream HTTP server with request capture
tests/call_create.rs     the POST contract over real sockets      (15 tests)
tests/sideband.rs        the WebSocket contract over real sockets (15 tests)
tests/forensics.rs       frame-log privacy and the tracing contract (11 tests)
src/**/tests             unit coverage for every module           (214 tests)
```

`tests/trust_boundary.rs` and `tests/wire_adapters.rs` were planned as separate
files and were not created: the trust boundary is covered by unit tests in
`src/admission/` plus router-level tests in `src/app.rs`, and the adapter truth
tables are unit tests in `src/wire/`. Both are genuinely covered; the file
layout simply differs from the plan.

## What each integration suite proves

`call_create.rs`

- The ChatGPT rewrite: multipart in, `{sdp, session}` out, AVAS URL, session id
  stripped, and no top-level `type` added.
- The API-key path forwards multipart byte for byte with its boundary intact.
- All six protocol headers forwarded; `authorization`, `chatgpt-account-id`,
  `x-openai-fedramp`, and `cookie` replaced or dropped.
- An absent `openai-alpha` is never invented.
- Both relayed response headers are asserted positively — an upstream
  `content-type: application/sdp` and the `location` — while cookies, request
  ids, and cache headers are dropped.
- Cap boundaries, upstream timeout, unreachable upstream, an exact multipart
  error message, a non-POST 404, and an invalid configured credential failing
  before any upstream contact.
- A downstream disconnect actually cancelling the upstream call, proven by an
  upstream that signals when its response body is dropped.

`sideband.rs`

- The Frameless-path and Realtime-query join styles reaching their documented
  upstream paths over real sockets, including the `3b766d91` rule. The
  `realtime/calls` path style and the full six-row style-by-profile matrix are
  unit-tested rather than socket-tested.
- Text, binary, and a >1 MiB UTF-8 payload surviving unchanged, with binary
  asserted at the variant level rather than by decoded bytes.
- Both close directions against a recording upstream: an upstream close
  propagates its code and reason; a client close arrives as `1000`/`client closed`.
- The pre-open queue, using a handshake held open by an explicit barrier: eight
  frames flushed in order, a 33rd frame closing `1009`, and 50 pings not
  consuming the budget.
- A trailing slash reaching the parser, a non-upgrade GET returning the contract
  404, and an unreachable upstream closing `1011`.

`forensics.rs`

- A clean transcript writing no payload at all.
- Text/binary corruption recording only a first fault byte offset; adjacent
  credential canaries and the removed `context` field are absent.
- A hostile global trace directive cannot re-enable tungstenite handshake,
  protocol, frame-payload, or close targets.
- An append failure being non-fatal, both directly and mid-relay.
- A live relay logging both directions and excluding keepalives, mutation-checked.
- The span contract for call-create and sideband, asserted against the isolated
  span-CLOSE line so an event carrying the same value cannot satisfy it.

## Known gaps

Stated rather than papered over:

- The 16 MiB cap boundary is exercised at 16/17 bytes in a unit test and with a
  512-byte cap in integration; a literal 16 MiB + 1 body is not sent.
- Mid-*body* client cancellation is covered by unit tests of the classifier and
  the guard; only mid-*response* cancellation is proven end to end.
- The `missing upstream` and `upstream not open` close mappings are unreachable
  by construction and are documented as such in `pump.rs` rather than tested.
- Three *reachable* close mappings have no test: `upstream error` (a transport
  failure mid-relay), `upstream send failed`, and `client send failed`. Each
  needs a peer that accepts a connection and then misbehaves in a specific way,
  which the current mock upstreams cannot express.

## CI

`.github/workflows/ci.yml`, triggered on pushes to `main` and on every pull
request:

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

No repository or user secrets are supplied to any step, and no elevated
permissions are requested. `actions/checkout` does use the automatic
`GITHUB_TOKEN`, which is scoped `contents: read`; `persist-credentials: false`
keeps it out of `.git/config`, so the untrusted build scripts, proc macros, and
tests that run afterwards on a fork PR cannot read it.

## README

Sections: what it is, the two upstream profiles, the route table, the header ownership rule with the `75344b09` reference, the sideband host rule with the `3b766d91` reference, configuration env vars, the frame-log privacy note, and a build/test quickstart.

## Exit criteria

`cargo test` green with a non-trivial test count; `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean; workflow file present and mirroring those three commands.
