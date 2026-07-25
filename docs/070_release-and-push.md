# 070 — Git history, GitHub repository, push

Work-phase `wp8-release`. Deliverable: the repository published with its history intact.

## Commit plan

One commit per completed work-phase, in order:

```text
docs: research the GPT-Live wire contract and OpenCodex relay behavior
feat: scaffold the axum service, config model, and error taxonomy
feat: enforce the downstream trust boundary (admission, origin, CORS, draining)
feat: model the three wire adapters and their session shapes
feat: implement the call-create relay
feat: implement the sideband WebSocket relay
feat: add tracing and opt-in metadata-only frame forensics
test: add integration tests, CI workflow, and README
```

The order follows the build sequence `010 → 015 → 040 → 020 → 030 → 050 → 060`, not the document numbers.

One commit per phase, so the history reads as a sequence of reviewed units
rather than a single drop.

## Publication

```bash
gh repo create lidge-jun/gpt-live-proxy --public --source . --remote origin --push
```

Scoped to this repository only. Nothing in the OpenCodex repository is
committed, branched, or pushed by this work.

## Verification

```bash
git log --oneline
git ls-remote origin
gh repo view lidge-jun/gpt-live-proxy
```

The reported SHA must match local `HEAD`. A CI run on the pushed commit is reported by its actual conclusion; a green run on an earlier commit never stands in for the pushed one.

## Safety checks before pushing

Three layers, because a single regex sweep is not a secret scan:

1. **Pattern sweep.** `rg` the tree for `sk-`, `eyJ`, `Bearer [A-Za-z0-9._-]{20,}`, and `gho_`; the only permitted hits are redaction tests using obvious dummies.
2. **Staged-diff inspection.** `git diff --cached` reviewed before the first commit, and `git log -p` skimmed before the push — a file removed later still lives in history.
3. **Tooling.** `gitleaks detect --no-git` (or `git secrets --scan`) over the tree when available; if neither is installed, that is recorded as a gap in the final report rather than silently skipped.

Additional checks:

- No `.env` committed; `.gitignore` covers `/target`, `.env*`, and `*.jsonl`.
- Frame logs are operator-chosen paths, so `.gitignore` cannot guarantee exclusion — the README instructs writing them outside the working tree.
- Every CI action is pinned to an immutable **commit** SHA — verified with `gh api repos/<owner>/<repo>/commits/<sha>`, which fails for an annotated tag object.
- No repository or user secrets reach the workflow. The automatic `GITHUB_TOKEN` that `actions/checkout` uses is scoped `contents: read` and is not persisted into `.git/config`.
