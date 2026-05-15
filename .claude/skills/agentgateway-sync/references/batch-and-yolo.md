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
  --output "$TMP_DIR/run_tests_baseline.json"
```

Same expectations as the per-commit flow: `all_passed: true`, zero
failures, zero warnings. If the baseline is dirty, **stop** — the fork
must have a clean baseline before any sync attempt. Surface the `tail`
field if present.

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
- **Conflict:** abort and bisect. Continue at "Conflict path" below.

## Step 5b. Conflict path (gate 1 failed)

The first action is *always* abort — no manual conflict resolution
inside batch mode. Manual triage happens on the single-commit follow-up
PR.

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

1. **Reset batch metadata** — treat the batch as the clean prefix:
   - `batch_count := clean_count`
   - `batch_end_sha := clean_end_sha`
   - `batch_end_short_sha := clean_end_short_sha`
   - `batch_commits` truncated to first `clean_count` entries
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
  --output "$TMP_DIR/run_tests_synced.json"
```

Compare against baseline. Parse JSON: if `all_passed` is false **after**
`retry_passed` is false (or `retry_passed` is null because `retry_errors`
occurred), follow **Test failure path** below. If `needed_retry` is true and
the final run passed, note "(retry)" in the landing comment / PR archive.

## Step 7. Push the sync branch

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

Parse the JSON on stdout. If `landed` is false or `errors` is non-empty
with a freshness mismatch / PATCH failure, **stop** and surface —
re-run sync if `adobe` moved. Force-flag details live in the script
docstring (always `-F force=true`, never `-f force=true`).

Archive to `$REPORTS_DIR/<sync-branch-without-prefix>/` — same as
`push-and-open-pr.md` step 6, with the report noting "auto-landed
(batch+yolo)".

## Step 10. Leave `sync/*` behind — checkout `adobe`, delete landed branch

After **`land_pr.py` stdout shows `"landed": true`** (and you have recorded any
archive): integration tip is **`adobe`**; the **`sync/*`** branch is disposable.

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
