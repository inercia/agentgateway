# Batch sync with auto-land ("yolo")

This is the **default** flow for any `sync N` invocation, including
`sync 1`. It batches up to N upstream commits into a single PR,
auto-lands when all gates pass, and falls back gracefully when they
don't.

## Mental model

Three gates govern landing — only the first two are mandatory:

1. **Clean rebase** (mandatory) — Adobe commits reapply on top of the
   batch with no conflicts. A conflict means upstream and Adobe touched
   the same logic — *that* needs human review.
2. **Tests pass** (mandatory) — `make test` matches or improves the
   baseline (counts ≥ baseline, no new warnings).
3. **Risk classification** (informational only) — surfaced in the PR
   body for retroactive review, not gating. Upstream is trusted.

When gates 1 and 2 pass, force-update `adobe` immediately. No `/land`
polling, no maintainer comment required, no grace period.

When gate 1 fails (conflict): `find_clean_prefix.py` bisects to find
the largest clean prefix K. Land K as one auto-PR, open a separate
single-commit PR for commit K+1 with the conflict surfaced, stop the
run.

When gate 2 fails (tests broken with clean rebase): open the batch PR
with failing-test detail in the body, fall back to the existing
`/land` polling flow so a human explicitly authorises landing.

## Branch shape

Same Shape D as the per-commit flow, just with N upstream commits at
the base instead of one:

```
<adobe-3'>                   ← Adobe commits, reapplied (new SHAs)
<adobe-2'>
<adobe-1'>
<upstream-N>                 ← VERBATIM (verbatim SHA, author, message)
<upstream-N-1>               ← VERBATIM
...
<upstream-1>                 ← VERBATIM
<last-shared-with-adobe>
```

`references/merge-strategies.md` still applies — the recommended GitHub
merge button is still wrong; landing is via `gh api PATCH refs/heads/adobe`.

## Step 1: re-check that batch metadata is in hand

Coming from `inspect-and-prepare.md`, you should already have:

- `batch_count` (integer ≥ 1, may be less than `batch_requested` if
  unsynced_count was smaller)
- `batch_end_sha`, `batch_end_short_sha`
- `batch_commits` (list of {sha, short_sha, subject, pr_num,
  author_date})
- `oldest_sha` (== `batch_commits[0].sha`) — still useful as the
  rebase fence

If `batch_count == 0`, return to the dispatcher — nothing to sync.

## Step 2: classify the batch (informational)

```bash
python3 "$SKILL_DIR/scripts/classify_batch.py" "$REPO" \
  <sha1> <sha2> <sha3> ... <shaN> > "$TMP_DIR/classify.json"
```

Inline every SHA from `batch_commits[*].sha` as separate args. Parse
the JSON if needed; **`$TMP_DIR/classify.json`** is the machine input for
**`compose_pr_body.py --merge`** in step 8.

**Do not gate on this output.** Even `risk: critical` proceeds — gate 1
(rebase) is what catches dangerous overlap with Adobe code. The label
is shown in the PR body so reviewers can ctrl-F for `SECURITY` after
the fact.

When using **`compose_pr_body.py --merge`**, elevated-risk rows
(`risk: critical` or `label: SECURITY`) automatically get the PR body banner
(step 8).

## Step 3: pick a sync branch name

```bash
python3 "$SKILL_DIR/scripts/pick_sync_branch.py" "$REPO" \
  --batch --count <batch_count> --end-short-sha <batch_end_short_sha>
```

Returns `sync/batch-<count>-to-<short_sha>` (with `-1`, `-2`, …
suffixes on collision against prior origin branches or any-state PRs,
same logic as single-commit mode). Tell the user the picked name.

## Step 4: baseline test run

```bash
python3 "$SKILL_DIR/scripts/run_tests.py" "$REPO" baseline --log-dir "$TMP_DIR" \
  --cache-file "$TMP_DIR/test_cache.json" \
  --cache-key "$(git -C "$REPO" rev-parse adobe)" \
  --output "$TMP_DIR/run_tests_baseline.json"
```

Same expectations as the per-commit flow: `all_passed: true`, zero
failures, zero warnings. If the baseline is dirty, **stop** — the fork
must have a clean baseline before any sync attempt. Surface the `tail`
field if present.

**Cache:** `--cache-key` is the current `adobe` SHA. On the second and
later loop iterations within one invocation, `adobe` equals the synced
tip that the previous iteration landed — which step 6 already cached as
a passing run. The baseline then returns `"cached": true` and skips a
full `make test`. A cache miss (first iteration, or `adobe` moved out of
band) runs normally. Treat a cached result exactly like a fresh pass.

Retain **`$TMP_DIR/run_tests_baseline.json`** for **`compose_pr_body.py --merge`**
(step 8).

## Step 5: rebase the batch

Two separate Bash calls. Inline the concrete values.

```bash
git -C "$REPO" switch -c "sync/batch-<count>-to-<short_sha>"
```

```bash
git -C "$REPO" rebase "<batch_end_sha>"
```

`git rebase <batch_end_sha>` from the new branch (which started at
`adobe`) takes commits in `<oldest_sha>^..adobe` (i.e. the Adobe-only
commits) and replays them onto `<batch_end_sha>`. Result: the N
upstream commits at the base, Adobe commits reapplied on top.

### Branch on rebase outcome

- **Clean rebase:** continue to step 6 (post-rebase tests).
- **Conflict:** check fast-path first (below), then abort and bisect.

## Step 5b. Conflict path (gate 1 failed)

### 5b.0 — Fast-path: `resource.pb.go`-only conflict

Before aborting and running the full bisection, check whether the
conflict is resolvable in-place without bisecting. This covers the most
common recurring pattern in this fork: every upstream MCP commit that
adds types regenerates `api/resource.pb.go`, conflicting with Adobe's
McpRewrite additions. Bisection is expensive (multiple full
rebase+abort cycles); the fast-path tries to resolve the whole batch in
one pass.

**Condition to attempt the fast-path:** the *only* conflicting file is
`api/resource.pb.go` and `crates/protos/proto/resource.proto` has no
conflict markers:

```bash
git -C "$REPO" status --porcelain | awk '/^UU/{print $2}'
# must output only: api/resource.pb.go

grep -c "<<<<<<<" "$REPO/crates/protos/proto/resource.proto" 2>/dev/null || echo 0
# must output 0
```

If both conditions hold, attempt a regeneration loop **without
aborting**:

```bash
PATH="./tools:$PATH" buf generate --path crates/protos/proto/resource.proto
git -C "$REPO" add api/resource.pb.go api/resource_json.gen.go
git -C "$REPO" rebase --continue
```

The rebase may stop again if another Adobe commit also conflicts.
Each time it stops, re-check: if **still** only `api/resource.pb.go`
(proto clean) → regenerate + stage + continue. Loop until the rebase
either:

- **Completes** — fast-path succeeded, skip bisection, continue to
  step 6 with the full `batch_count`.
- **Stops with a different conflict file** — fast-path cannot handle
  this. Abort:
  ```bash
  git -C "$REPO" rebase --abort
  git -C "$REPO" switch adobe
  git -C "$REPO" branch -D "sync/batch-<count>-to-<sha>"
  ```
  Fall through to full bisection below.

**If fast-path conditions are not met from the start**, skip directly
to full bisection.

---

The first action for full bisection is *always* abort — **no manual
conflict resolution inside batch mode, ever**. The only in-place
resolution allowed in batch mode is the `resource.pb.go` fast-path
(§5b.0). Every other conflict — regardless of how superficial it looks
— goes through abort → bisect → auto-resolve. Resolving by hand inside
a batch rebase produces a structurally incorrect branch (wrong merge
base, GitHub "conflicts" indicator, verify_branch_shape failure) and
must not be done without explicit user approval.

```bash
git -C "$REPO" rebase --abort
```

Then return to `adobe` (the bisection script will manage its own
working state, but start from a known place):

```bash
git -C "$REPO" switch adobe
```

Run the bisection script:

```bash
python3 "$SKILL_DIR/scripts/find_clean_prefix.py" "$REPO" \
  --base-ref adobe --upstream-ref upstream/main --count <batch_count>
```

Parse the JSON:

```json
{
  "requested_count": 50,
  "available_count": 50,
  "clean_count": 47,
  "clean_end_sha": "...",
  "clean_end_short_sha": "...",
  "conflicting_count": 48,
  "conflicting_sha": "...",
  "conflicting_short_sha": "...",
  "conflicting_subject": "...",
  "conflicting_pr_num": "1234",
  "attempts": [
    {"k": 25, "sha": "...", "result": "clean"},
    {"k": 38, "sha": "...", "result": "conflict"},
    ...
  ]
}
```

Three sub-cases:

### 5b.1 — `clean_count == 0`

Even commit #1 conflicts when reapplying Adobe commits on top.

**Do not land any prefix.** Record a halt and **stop the dispatcher loop**
— the oldest unsynced upstream commit needs **manual** conflict
resolution against Adobe commits before automation can proceed.

Tell the user:

> Bisection: even the oldest unsynced commit (`<short_sha>` —
> "<subject>") conflicts with Adobe commits on rebase. Resolve manually
> (merge/cherry-pick onto current `adobe`), verify `make test`, then
> re-invoke **`sync`**.

### 5b.2 — `0 < clean_count < batch_count`

Land the clean prefix, then attempt to auto-resolve the conflicting
commit. This is the path that turns "stop and wake the user" into
"keep going unless something genuinely needs human attention."

1. **Reset batch metadata and re-inspect** — treat the batch as the
   clean prefix:
   - `batch_count := clean_count`
   - `batch_end_sha := clean_end_sha`
   - `batch_end_short_sha := clean_end_short_sha`
   - `batch_commits` truncated to first `clean_count` entries

   **Re-run `inspect_state.py` with the prefix count** so the saved
   `inspect.json` reflects only the commits that will actually land.
   This is the file `compose_pr_body.py` reads for the commit table —
   if it still holds the full batch, the PR title will say "4 commits"
   but the body will list 12:

   ```bash
   python3 "$SKILL_DIR/scripts/inspect_state.py" "$REPO" \
     --count <clean_count> > "$TMP_DIR/inspect.json"
   ```

   **Also filter `classify.json`** to only the prefix SHAs. The
   simplest approach: keep the existing classification results but drop
   rows for commits outside the prefix (they will be classified again
   when their batch runs):

   ```python
   prefix_shas = {c["sha"] for c in batch_commits[:clean_count]}
   filtered = {**classify_data, "commits": [c for c in classify_data["commits"] if c["sha"] in prefix_shas]}
   # write to $TMP_DIR/classify.json
   ```

2. **Re-pick the sync branch name** with the new (smaller) count and
   end-sha (the original `sync/batch-<N>-to-<sha>` branch was never
   created since the bisect script cleaned up after itself):
   ```bash
   python3 "$SKILL_DIR/scripts/pick_sync_branch.py" "$REPO" \
     --batch --count <clean_count> --end-short-sha <clean_end_short_sha>
   ```
3. **Resume from step 5** (real rebase + test + auto-land) using the
   new, smaller batch. This lands the clean prefix exactly as the
   happy path would.
4. **After the prefix lands**, **load `references/auto-resolve-conflict.md`**
   with these inputs from the bisection result:
   - `conflicting_sha`, `conflicting_short_sha`, `conflicting_subject`,
     `conflicting_pr_num`
   - `clean_count` (for the auto-resolve PR body's "landed in batch
     run" reference)
   - `prefix_pr_num` (the PR # just landed)

   That playbook handles the single conflicting commit through one of
   four terminal outcomes:

   | Outcome | Dispatcher action |
   |---|---|
   | Auto-resolve clean + tests pass | `landed_count += 1`, **continue loop** to next batch |
   | Protected-path conflict | **Halt** — manual merge for conflicting upstream SHA (prefix already landed). Append `halts`. |
   | Semantic hunk detected | **Halt** — manual merge (prefix already landed). Append `halts`. |
   | Auto-resolve clean + tests fail | Open PR with `[TESTS FAILING]` prefix, `/land` hand-off, **stop loop** |

   The playbook tells the dispatcher which case fired. Act accordingly.

### 5b.3 — `clean_count == batch_count`

Bisection disagrees with the rebase that just failed. This is a
genuine error condition — surface the `attempts` log to the user and
stop. Don't attempt a third rebase.

## Step 6. Post-rebase tests (gate 2)

```bash
python3 "$SKILL_DIR/scripts/run_tests.py" "$REPO" synced --log-dir "$TMP_DIR" --retry-once \
  --cache-file "$TMP_DIR/test_cache.json" \
  --cache-key "$(git -C "$REPO" rev-parse HEAD)" \
  --output "$TMP_DIR/run_tests_synced.json"
```

`--cache-key` is the synced branch tip (`HEAD`). When this batch lands,
`land_pr.py` force-updates `adobe` to exactly this SHA, so the next
iteration's step-4 baseline (keyed on `adobe`) hits this cache entry
and skips re-running. The cache only ever stores passing runs.

Compare against baseline. Parse JSON: if `all_passed` is false **after**
`retry_passed` is false (or `retry_passed` is null because `retry_errors`
occurred), follow **Test failure path** below. If `needed_retry` is true and
the final run passed, note "(retry)" in the landing comment / PR archive.

## Step 7. Pre-push guard + push the sync branch

**Before pushing, verify the working tree is clean.** Uncommitted
changes at this point mean a conflict resolution or manual edit was
applied but never staged+committed — pushing would leave those fixes
off the branch and break `adobe` after landing (this was the root
cause of the PR #75 / #76 incident).

```bash
git -C "$REPO" diff HEAD --stat
```

If the output is non-empty (any modified or staged files), **stop**:

> Pre-push guard: working tree is not clean. Stage and commit the
> outstanding changes before pushing. Outstanding files:
> `<list from diff --stat>`

Do not push. Do not auto-land. Fix the working tree first (stage the
changes, `git commit --amend` or a new commit, re-run tests if the
added content is non-trivial), then continue.

**Then verify branch shape (Shape D integrity + commit count).** This
asserts the upstream batch commits are carried *verbatim* and that the
branch matches the count the PR body will claim — catching the PR #75
(rewritten upstream commit) and PR #81 (count drift) incident classes
before they reach `origin`:

```bash
python3 "$SKILL_DIR/scripts/verify_branch_shape.py" "$REPO" \
  --head HEAD \
  --expected-shas "<comma-joined batch_commits[*].sha>" \
  --expected-count <batch_count>
```

`--expected-count` must be the same `batch_count` the PR body is
composed from (step 8's `inspect.json`). Parse JSON: if `ok` is false,
**stop** and surface `errors` — do not push. A `missing_shas` entry
means the rebase rewrote an upstream commit; a `count_match: false`
means the branch and the body disagree on how many upstream commits
landed.

Only when both the uncommitted-changes guard and `verify_branch_shape.py`
pass, push:

```bash
git push -u origin "sync/batch-<count>-to-<short_sha>"
```

Same auth caveats as `push-and-open-pr.md` apply (workflow scope, no
embedded tokens).

## Step 8. Capture freshness, compose PR body, open PR

Capture the current `origin/adobe` SHA *before* opening the PR. This
is the freshness marker used at landing time.

```bash
git -C "$REPO" rev-parse origin/adobe
```

Ensure machine-readable inputs exist (re-run a script with the same args + output
paths if you only parsed stdout earlier):

- **`$TMP_DIR/inspect.json`** — JSON from **`inspect_state.py "$REPO" --count <batch_count>`**
  (same **`batch_count`** as this batch).
- **`$TMP_DIR/classify.json`** — from step 2 (**`classify_batch.py`** redirect).
- **`$TMP_DIR/run_tests_baseline.json`** / **`$TMP_DIR/run_tests_synced.json`** — from
  steps 4 and 6 (**`run_tests.py --output`**).

Compose the PR body in one shot (**`--merge`** joins **`inspect_state`** +
**`classify_batch`** + **`run_tests`** — avoids the manual
**`batch_commits` ↔ classify** merge that caused broken tables on some PRs):

```bash
python3 "$SKILL_DIR/scripts/compose_pr_body.py" --kind batch --merge \
  --inspect-state "$TMP_DIR/inspect.json" \
  --classify-batch "$TMP_DIR/classify.json" \
  --baseline-tests "$TMP_DIR/run_tests_baseline.json" \
  --post-tests "$TMP_DIR/run_tests_synced.json" \
  --adobe-at-creation "$(git -C "$REPO" rev-parse origin/adobe)" \
  --out "$TMP_DIR/agw_pr_body.md"
```

The fallback **`--json-file`** workflow (hand-built JSON) lives in
**`references/push-and-open-pr.md`** under "Fallback / manual path".

Open the PR:

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh pr create \
  --repo Adobe-Apis/agentgateway \
  --base adobe \
  --head "sync/batch-<count>-to-<short_sha>" \
  --title "Sync upstream main: <count> commits (<oldest_short>..<batch_end_short>)" \
  --body-file "$TMP_DIR/agw_pr_body.md"
```

## Step 9. Auto-land — instant

Both gates passed. Land immediately, no `/land` polling, no grace
period.

Capture the sync-branch tip SHA (PR head):

```bash
git -C "$REPO" rev-parse HEAD
```

Run **`scripts/land_pr.py`** — it checks freshness (`adobe_at_creation`
from step 8), force-updates `refs/heads/adobe`, posts the landing
comment, closes the PR if still open, and resets local `adobe`:

```bash
python3 "$SKILL_DIR/scripts/land_pr.py" "$REPO" \
  --pr <pr_number_from_gh_pr_create> \
  --head-sha <full_sha_from_rev_parse_above> \
  --expected-adobe <adobe_at_creation_from_step_8> \
  --reason auto-batch
```

Parse the JSON on stdout. The authoritative success signal is
**`landed_confirmed_by_ancestry: true`** (`adobe` now points at the PR
head SHA) — **not** `pr_state_after`. A force-push land cannot be
reliably auto-detected as a merge, so the PR ending **`CLOSED`** rather
than `MERGED` is the expected, correct terminal state; do not treat
CLOSED as a failure. If `landed_confirmed_by_ancestry` is false, or
`errors` carries a freshness mismatch / PATCH failure, **stop** and
surface — re-run sync if `adobe` moved. Force-flag details live in the
script docstring (always `-F force=true`, never `-f force=true`).

Archive to `$REPORTS_DIR/<sync-branch-without-prefix>/` — same as
`push-and-open-pr.md` step 6, with the report noting "auto-landed
(batch+yolo)".

## Step 10. Leave `sync/*` behind — checkout `adobe`, delete landed branch

After **`land_pr.py` stdout shows `"landed_confirmed_by_ancestry": true`** (and
you have recorded any archive): integration tip is **`adobe`**; the **`sync/*`**
branch is disposable.

1. Capture the sync branch name **before** leaving it (you should still be on it):

   ```bash
   SYNC_BRANCH=$(git -C "$REPO" rev-parse --abbrev-ref HEAD)
   ```

   If **`SYNC_BRANCH`** does **not** match **`sync/*`**, stop and surface — do not
   delete an unexpected branch.

2. Switch back to **`adobe`** and align with **`origin/adobe`** (same intent as
   **`inspect-and-prepare.md`**):

   ```bash
   git -C "$REPO" switch adobe
   ```

   If **`adobe`** does not exist locally yet:

   ```bash
   git -C "$REPO" switch -c adobe --track origin/adobe
   ```

   Then:

   ```bash
   git -C "$REPO" pull --ff-only origin adobe
   ```

   If **`pull --ff-only`** fails, apply **`inspect-and-prepare.md`** § **Landing-artifact auto-recovery**
   before retrying **`pull --ff-only`**.

3. Delete the **remote** sync branch (best effort — branch protection may deny this):

   ```bash
   env -u GITHUB_TOKEN -u GH_TOKEN git -C "$REPO" push origin --delete "$SYNC_BRANCH"
   ```

   On **`403`** / **`unable to delete`** / similar: mention in the chat summary that the
   branch remains on GitHub; do **not** treat that as a failed land.

4. Drop the **local** sync branch if it still exists:

   ```bash
   git -C "$REPO" branch -d "$SYNC_BRANCH"
   ```

   Prefer **`-d`** (safe delete). Only use **`-D`** if Git refuses incorrectly **after**
   a confirmed successful land when **`adobe`** already contains the same commits.

Why **`adobe` HEAD**: matches **`inspect-and-prepare.md`** (next batch rebases from **`adobe`**),
avoids confusing **`git status`** on an obsolete **`sync/*`** tip, and mirrors where humans
usually continue after integration lands.

## Test failure path (gate 2 failed after retry)

Both retries failed but rebase was clean. Open the PR anyway with the
failing test results in the body, then fall back to the per-PR `/land`
flow so a human explicitly authorises landing despite the test issue.

1. Compose the body with **`compose_pr_body.py --kind batch --merge`** (same as
   step 8). The post-rebase **`run_tests`** JSON must be the **failed** run
   (still use **`--output`**); **`--merge`** copies **`tail`** into the body when
   **`all_passed`** is false. Fallback manual JSON: **`push-and-open-pr.md` §3**.
2. Open the PR with title prefix `[TESTS FAILING] `.
3. Post a comment:
   ```
   ❌ Post-rebase `make test` failed (after retry). Manual review
   required before landing. /land will still work if a maintainer
   explicitly approves the failure.
   ```
4. Hand off to `references/poll-and-land.md` for `/land` authorisation —
   maintainer comments `/land`, then **re-invokes** `land PR #N`
   (section 2). Do NOT auto-land.
5. Do NOT auto-land.

## Dispatcher outcomes (reference for `SKILL.md` step 6)

| Outcome | `landed_count` delta | Continue loop? |
|---|---|---|
| Full batch clean + tests pass | +batch_count | yes |
| Bisect partial + auto-resolve clean + tests pass | +clean_count then +1 | yes |
| Bisect partial + protected-path conflict | +clean_count (prefix landed first) | **NO** — manual merge |
| Bisect partial + semantic hunk | +clean_count | **NO** — manual merge |
| Bisect partial + auto-resolve clean + tests fail | +clean_count | **NO** — open `[TESTS FAILING]` PR → `/land` hand-off |
| Tests fail with no conflict | +0 | **NO** — open `[TESTS FAILING]` PR → `/land` hand-off |
| `clean_count == 0` | +0 | **NO** — manual conflict triage |
| Bisection anomaly (`clean_count == batch_count` after conflict) | — | **NO** |

Auto-resolve specifics also appear at the bottom of `auto-resolve-conflict.md`.

## Stopping summary for the dispatcher loop

On any **NO** row above, append to `halts`, print the final summary, and stop
unless the flow already auto-landed the happy path.
