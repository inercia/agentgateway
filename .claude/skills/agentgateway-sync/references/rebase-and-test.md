# Rebase and test

This covers three things: baseline `make test`, pick a sync branch name, rebase onto the oldest upstream commit, then re-run `make test`. If the rebase stops on a conflict, branch to `conflict-triage.md` and come back here afterwards.

## 1. Baseline test run

The `run_tests.py` script wraps `make test` so a single script invocation matches one allowlist entry (instead of the bash one-liner `(cd "$REPO" && make test) > log 2>&1` triggering a "shell operators require approval" prompt every time).

**The log file is too big for stdout — keep the `--log-dir` flag but still parse the counts from stdout.**

```bash
python3 "$SKILL_DIR/scripts/run_tests.py" "$REPO" baseline --log-dir "$TMP_DIR"
```

Parse the JSON from the Bash tool's stdout:

```json
{
  "label": "baseline",
  "log_path": "$TMP_DIR/agw_make_test_baseline.log",
  "returncode": 0,
  "suites_ok": 12,
  "suites_failed": 0,
  "passed": 794,
  "failed": 0,
  "ignored": 4,
  "warnings": 0,
  "all_passed": true
}
```

Expect `all_passed: true` with `failed: 0` and `warnings: 0`. If any of these are off, stop and ask the user whether to continue — the fork should have a clean baseline before any sync attempt. When the script surfaces a `tail` field (last 30 lines), show it so the user can diagnose without opening the full log.

Retain the baseline counts in context — `push-and-open-pr.md` compares them to the post-rebase counts in the PR body.

## 2. Pick a sync branch name

`pick_sync_branch.py` derives the sync branch name from the upstream PR's source branch and handles collisions against prior origin branches and prior Adobe-Apis PRs (any state). It also returns the list of prior PRs being superseded — useful for the PR body.

Inline the `source_branch` value you already parsed from `inspect_state.py` output. No `$(jq …)` substitution.

```bash
python3 "$SKILL_DIR/scripts/pick_sync_branch.py" "$REPO" "telemetry/span-links"
```

Parse the JSON from stdout:

```json
{
  "sync_branch": "sync/telemetry-span-links-1",
  "base_name": "sync/telemetry-span-links",
  "suffix": 1,
  "superseded_prs": [
    {"number": 11, "state": "CLOSED", "url": "https://github.com/Adobe-Apis/agentgateway/pull/11", "title": "..."}
  ]
}
```

Tell the user which branch name was picked. If `suffix > 0`, list every entry in `superseded_prs` (number, state, URL) — they go into a `## Supersedes` section in the new PR body.

## 3. Create the branch and rebase

Two separate Bash calls. No `&&` chaining. Inline the concrete values — do not use `$SYNC_BRANCH` or `$OLDEST_SHA` shell variables in the rebase command (they don't exist in Claude's shell session across calls).

```bash
git -C "$REPO" switch -c "sync/telemetry-span-links-1"
```

```bash
git -C "$REPO" rebase "772c618820ad7384e435672fde76605f889429ea"
```

`git rebase <oldest_sha>` takes every commit on the sync branch that is not reachable from `<oldest_sha>` (i.e. the Adobe-only commits) and reapplies them one-by-one on top of `<oldest_sha>`. The result is the linear Shape-D history: one upstream commit at the base, Adobe commits reapplied on top with new SHAs.

### 3.5 Protected-path smoke check (Adobe)

Before post-rebase tests, verify Adobe-specific patches are still present (or run the `check-patches` skill). Quick grep examples:

```bash
grep -nE "claim_as_millis|TokenError::Expired" "$REPO"/crates/agentgateway/src/jwt.rs"
grep -nE "HEADER_SESSION_ID|mcp-session-id" "$REPO"/crates/agentgateway/src/mcp/sse.rs"
```

If expected markers are missing, **stop** — resolve manually before running `run_tests.py`.

### If rebase stops on a conflict

**Load `references/conflict-triage.md` and follow it.** Never emit `git rebase --continue` without first understanding what the conflict is. Return to step 4 of this file once the rebase completes.

## 4. Post-rebase test run

```bash
python3 "$SKILL_DIR/scripts/run_tests.py" "$REPO" synced --log-dir "$TMP_DIR"
```

Compare the synced counts to baseline side by side in your response:

```
Baseline:   794 passed, 4 ignored, 0 failed, 0 warnings, 12 suites ok
Post-rebase: 798 passed, 4 ignored, 0 failed, 0 warnings, 12 suites ok (+4 tests introduced by upstream)
```

A higher `passed` or `ignored` count is expected when the upstream commit adds new tests — note it but continue.

### Retry-once policy

**If `all_passed == false`** (or `failed` / `warnings` increased vs. baseline), a flaky test may be the cause. Run once more:

```bash
python3 "$SKILL_DIR/scripts/run_tests.py" "$REPO" synced-retry --log-dir "$TMP_DIR"
```

- **Retry passes:** note to the user that it was a retry pass and continue to `push-and-open-pr.md`. Remember this — the PR comment is prefixed with "(retry)".
- **Retry also fails:** surface the retry JSON's `tail` field and **stop**. Do not push, do not open a PR. The sync branch has a genuine test failure that needs investigation first.

## Return to the dispatcher

Tests pass. Carry the following values forward to `push-and-open-pr.md`:

- `sync_branch` (e.g. `sync/telemetry-span-links-1`)
- `suffix` + `superseded_prs` (for the PR body)
- `oldest_sha`, `pr_num`, `source_branch`, `oldest_subject` (from inspect)
- baseline vs. synced test counts
- whether the synced run needed a retry
