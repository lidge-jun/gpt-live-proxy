# 150 — Release, push, and exact-SHA CI

Work-phase: `wp7-release-ci`. No new behavior enters here. Any fix discovered by
verification returns to the owning implementation phase or is recorded as a
new work-phase; release does not patch around a failed gate.

## Preconditions

- every `080` matrix row has final `supported`, `adapted`, `unsupported`, or
  `out-of-base-scope` status with evidence;
- all planned files are accounted for in an independent changed-file review;
- no Critical/High or unhandled Medium finding remains;
- README examples match executed conformance scenarios;
- working tree contains no unrelated/user changes; cxc-generated `.codexclaw/`
  cache is ignored or explicitly excluded before claiming a clean scoped tree.

## Commands

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
actionlint
gitleaks git . --no-banner --redact
cargo audit
npm audit --prefix conformance/node --audit-level=high --ignore-scripts
git diff --check
git status --short --branch
```

Run an independent final reviewer against `de1240b..HEAD`, including a changed-
file coverage ledger and compatibility-matrix falsification pass. Repair and
re-run until PASS.

The current session's original request explicitly authorizes a push, and the
follow-up requires CI to pass. Immediately before pushing, re-read the latest
user instruction and stop if that authorization has been revoked or narrowed.
With authorization still active, push `main`, then verify:

```bash
git rev-parse HEAD
git ls-remote origin refs/heads/main
gh run list --branch main --limit 5 --json databaseId,status,conclusion,headSha,url
gh run view <latest-run-id> --json status,conclusion,headSha,jobs,url
```

DONE requires local HEAD = `origin/main` = the successful run's `headSha`, and
every job in that run has `status=completed`, `conclusion=success`. A predecessor
run does not prove a later documentation or release commit.
