# Auto-resolve conflicts (full-automation path)

Loaded from `batch-and-yolo.md` when the bisection pass found a clean
prefix < batch_count and there is a conflicting commit at the
boundary. Goal: handle that one commit without waking the user, but
**only when it's safe to do so**. Halt with a PR open whenever any
gate fails.

## Hard guarantees this playbook upholds

1. **Tests are the only ground truth.** Every auto-resolution is
   verified by `make test`. A passing resolution lands. A failing
   resolution halts with full context.
2. **Protected paths are never auto-resolved.** Conflicts touching
   `jwt.rs`, `mcp/sse.rs`, or anything under `adobe/` halt
   immediately. The whole point of marking these is that Adobe owns
   that behavior — collisions there are almost certainly intentional
   and tests don't cover security properties.
3. **Semantic hunks halt.** The classifier is conservative — when in
   doubt, halt. Better to false-halt than false-resolve.
4. **Repo state is recoverable.** Any halt aborts the rebase, returns
   HEAD to `adobe`, deletes the throwaway branch. The next sync
   invocation starts from a clean slate.

## Inputs from `batch-and-yolo.md`

- `conflicting_sha`, `conflicting_short_sha`, `conflicting_subject`,
  `conflicting_pr_num` — the upstream commit at the bisection boundary.
- `clean_count` — how many commits already landed in the prefix PR
  (for context in the auto-resolve PR body and the dispatcher report).

## 1. Set up a fresh attempt branch

After the prefix PR auto-landed, `adobe` already advanced. Refresh
local pointer and branch off:

```bash
git -C "$REPO" switch adobe
```

```bash
git -C "$REPO" pull --ff-only origin adobe
```

```bash
git -C "$REPO" switch -c "sync/auto-resolve-<conflicting_short_sha>"
```

If the branch name collides (rare — only happens if a previous
auto-resolve attempt for the same commit was pushed and abandoned),
pass through `pick_sync_branch.py` for a numbered suffix:

```bash
python3 "$SKILL_DIR/scripts/pick_sync_branch.py" "$REPO" \
  --batch --count 1 --end-short-sha "<conflicting_short_sha>"
```

(The `--batch --count 1` form yields `sync/batch-1-to-<short>` which
is fine — different from the per-commit `sync/<source-branch>` form
the single-commit fallback uses, deliberately.)

## 2. Trigger the conflict

```bash
git -C "$REPO" rebase "<conflicting_sha>"
```

Rebase will stop. The skill remembers it's now in step 3 of *this*
playbook — classify conflicts using **`references/auto-resolve-conflict.md`**
section 5 (superficial vs semantic).

## 3. Identify conflicting files

```bash
git -C "$REPO" status --porcelain
```

Files with `UU`, `AA`, or `DD` prefix are the conflicting paths.
Record the list as `CONFLICT_FILES`.

## 4. Protected-path check (hard halt)

For each path in `CONFLICT_FILES`, normalise (replace `\` with `/`)
then classify with **`scripts/check_protected_paths.py`**:

```bash
python3 "$SKILL_DIR/scripts/check_protected_paths.py" <path1> <path2> ...
```

Parse JSON — if `is_protected` is true:

```bash
git -C "$REPO" rebase --abort
```

```bash
git -C "$REPO" switch adobe
```

```bash
git -C "$REPO" branch -D "sync/auto-resolve-<conflicting_short_sha>"
```

**Halt the dispatcher loop.** Surface:

> Halted on protected-path conflict for upstream commit `<short_sha>`
> — "<subject>". Upstream SHA `<full_sha>` needs **manual** merge onto
> current `adobe` (Adobe patches). Use `check-patches` skill after you
> resolve. Re-invoke **`sync`** when the conflict is integrated — do
> not skip past this commit silently.

Do **not** auto-open a scripted follow-up PR.

## 4b. Generated-file regeneration (run before per-file classification)

Before manually classifying hunks, check whether any conflicting path
is a **known generated file** that can be regenerated from its source:

| Conflicting path | Generator command (run from repo root) | Source |
|---|---|---|
| `api/resource.pb.go` | `PATH="./tools:$PATH" buf generate --path crates/protos/proto/resource.proto` | `crates/protos/proto/resource.proto` |
| `api/resource_json.gen.go` | same command as above | same |

**Why regenerate instead of manually merging:** Generated protobuf files
contain numeric field indexes, protoc version stamps, and interleaved
Go types that diverge structurally when both sides add new types. Manual
hunk-merging is unreliable. The `.proto` source auto-merges cleanly
(upstream and Adobe add to non-overlapping sections), so regenerating
from the resolved source produces the correct combined output.

**How to apply:**

1. Check if `api/resource.pb.go` or `api/resource_json.gen.go` is in
   `CONFLICT_FILES`.
2. If yes, verify the `.proto` source merged cleanly (no conflict markers):
   ```bash
   grep -c "<<<<<<" crates/protos/proto/resource.proto || echo "clean"
   ```
   If the source is clean, regenerate:
   ```bash
   PATH="./tools:$PATH" buf generate --path crates/protos/proto/resource.proto
   ```
   The `buf` wrapper lives in `tools/buf`; warnings about `proto3_optional`
   from the jsonshim plugin are expected and can be ignored.
3. Verify no conflict markers remain in the generated files:
   ```bash
   grep -c "<<<<<<" api/resource.pb.go api/resource_json.gen.go 2>/dev/null || echo "clean"
   ```
4. Stage the regenerated files:
   ```bash
   git add api/resource.pb.go api/resource_json.gen.go
   ```
5. Remove these paths from `CONFLICT_FILES` before proceeding to §5.
   If `CONFLICT_FILES` is now empty, skip to §8 (`rebase --continue`).

If the `.proto` source itself has conflict markers → treat as SEMANTIC
and halt (§6 HALT path).

## 5. Per-file classification

For each path in `CONFLICT_FILES` (none of which is protected after
step 4), gather three views:

```bash
git -C "$REPO" rebase --show-current-patch | head -1
```

That command returns the SHA of the Adobe commit currently being
reapplied (the "stopped-sha"). Record it as `STOPPED_SHA`.

For each conflicting file `<path>`:

1. Read the file with conflict markers (use the **Read** tool, absolute
   path). The `<<<<<<<`, `=======`, `>>>>>>>` blocks delimit hunks.
2. `git -C "$REPO" show "<conflicting_sha>:<path>"` — upstream's
   version of the file.
3. `git -C "$REPO" show "<STOPPED_SHA>:<path>"` — Adobe commit's
   intent for the file (pre-rebase).

For each conflict hunk, classify as **SUPERFICIAL** or **SEMANTIC**:

### SUPERFICIAL (resolve)

Apply the resolution rule that applies. Record which rule fired so it
can go in the PR body.

| Rule | Pattern | Resolution |
|---|---|---|
| **identical-edit** | Both sides apply byte-identical changes | Take either side |
| **whitespace-only** | Sides differ only in indentation / trailing whitespace / newline at EOF | Take upstream |
| **comment-only** | Sides differ only in `//` / `#` comment text | Take upstream comment + Adobe comment if both add new ones; otherwise upstream |
| **import-shuffle** | Both sides add disjoint imports/use statements | Concatenate, sort if the file's existing imports were sorted |
| **adjacent-add** | Each side adds new code on adjacent lines, neither side touches the other's lines | Concatenate, upstream first then Adobe (preserves Adobe-on-top history) |
| **rename-passthrough** | Upstream renames identifier `foo` → `bar`, Adobe code uses `foo` only as a call site | Rewrite Adobe's call sites to `bar` |
| **format-only** | Upstream applies a formatter (rustfmt / gofmt) to lines Adobe also touched, but Adobe's logic is preserved when reformatted | Take the formatted version of Adobe's logic |

If you can articulate which rule fires in **one sentence**, it's
SUPERFICIAL. If you're reasoning about behavior or guessing intent,
it's SEMANTIC.

### SEMANTIC (halt)

Anything not matching a rule above. Concretely:

- Both sides edit the same statement(s) with different semantics.
- Upstream removes code Adobe still depends on (even if Adobe's hunk
  doesn't directly touch the removed code, the removal breaks an
  invariant Adobe assumed).
- Hunks longer than ~20 lines on either side, even if they look
  superficial — proxy for "too much surface to be sure".
- Any hunk whose patch text contains a known critical pattern from
  `classify_commit.py` `PATTERNS_CRITICAL` (`claim_as_millis`,
  `TokenError::Expired`, `HEADER_SESSION_ID`, `mcp-session-id`, etc.)
  even outside protected paths — those markers exist in Adobe code
  for a reason.

## 6. Halt-or-proceed decision

After classifying every hunk in every file:

- **Any SEMANTIC hunk → HALT.** Same cleanup as step 4 (abort rebase,
  switch back, delete branch). Surface both sides of the conflict — user
  merges manually for upstream `<conflicting_sha>` then re-invokes **`sync`**.
  Set the dispatcher loop to STOP.

- **All SUPERFICIAL → proceed to step 7.**

## 7. Apply resolutions

For each conflicting file (in order), use the **Edit** tool to rewrite
the file with the resolved content — no conflict markers, both intents
preserved per the rules above.

After editing, stage:

```bash
git -C "$REPO" add "<path>"
```

One Bash call per file. No chaining.

## 8. Continue the rebase

```bash
git -C "$REPO" rebase --continue
```

The rebase may pause again if a *different* Adobe commit also
conflicts when reapplied (the rebase replays Adobe commits one by
one, each can fail independently). If so, return to step 3 and loop.

When rebase reports "successfully rebased and updated", continue.

## 9. Run tests

```bash
python3 "$SKILL_DIR/scripts/run_tests.py" "$REPO" auto-resolved \
  --log-dir "$TMP_DIR" --retry-once \
  --output "$TMP_DIR/run_tests_auto_resolved.json"
```

Parse JSON: if `all_passed` is false after retry (`retry_passed` is false),
follow **HALT path** below. If `needed_retry` is true and tests passed,
note "(retry)" in the PR body (`retry_note` for `compose_pr_body.py`).

### HALT path (tests fail after retry)

The auto-resolution compiled but broke behavior. Push the branch
anyway and open a PR for human review — don't throw away the
resolution attempt; the diff is useful evidence:

```bash
git push -u origin "sync/auto-resolve-<conflicting_short_sha>"
```

Compose the PR body with **`compose_pr_body.py --merge`** (**`references/push-and-open-pr.md` §3**):
persist **`find_clean_prefix.py`** JSON to **`$TMP_DIR/bisect.json`**, **`resolution_rows.json`**
(**JSON array** of **`{file, rule, notes}`** per conflicted file), **`run_tests`** outputs
with **`--output`**, then:

```bash
python3 "$SKILL_DIR/scripts/compose_pr_body.py" --kind auto-resolve --merge \
  --find-clean-prefix "$TMP_DIR/bisect.json" \
  --prefix-pr-num <prefix_pr_number> \
  --resolution-rows "$TMP_DIR/resolution_rows.json" \
  --baseline-tests "$TMP_DIR/run_tests_baseline.json" \
  --post-tests "$TMP_DIR/run_tests_auto_resolved.json" \
  --adobe-at-creation "$(git -C "$REPO" rev-parse origin/adobe)" \
  --out "$TMP_DIR/agw_pr_body.md"
```

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh pr create \
  --repo Adobe-Apis/agentgateway \
  --base adobe \
  --head "sync/auto-resolve-<conflicting_short_sha>" \
  --title "[TESTS FAILING] Sync upstream: <subject> (auto-resolved superficial conflicts)" \
  --body-file "$TMP_DIR/agw_pr_body.md"
```

Hand off to `references/poll-and-land.md` — maintainer comments `/land`,
then **re-invokes** `land PR #N`. Set dispatcher loop to STOP.

## 10. Push, open PR, auto-land (happy path)

Push:

```bash
git push -u origin "sync/auto-resolve-<conflicting_short_sha>"
```

Capture freshness:

```bash
git -C "$REPO" rev-parse origin/adobe
```

Compose the PR body with **`compose_pr_body.py --merge`** (**`references/push-and-open-pr.md` §3**)
using **`$TMP_DIR/bisect.json`**, **`$TMP_DIR/resolution_rows.json`**, **`$TMP_DIR/run_tests_baseline.json`**
(from the prefix / batch context), and **`$TMP_DIR/run_tests_auto_resolved.json`** from step 9
(**`--output`**). On the happy path the post-run has **`all_passed: true`**; **`--merge`** still sets
**`retry_note`** from **`needed_retry`**.

```bash
python3 "$SKILL_DIR/scripts/compose_pr_body.py" --kind auto-resolve --merge \
  --find-clean-prefix "$TMP_DIR/bisect.json" \
  --prefix-pr-num <prefix_pr_number> \
  --resolution-rows "$TMP_DIR/resolution_rows.json" \
  --baseline-tests "$TMP_DIR/run_tests_baseline.json" \
  --post-tests "$TMP_DIR/run_tests_auto_resolved.json" \
  --adobe-at-creation "$(git -C "$REPO" rev-parse origin/adobe)" \
  --out "$TMP_DIR/agw_pr_body.md"
```

Open the PR:

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh pr create \
  --repo Adobe-Apis/agentgateway \
  --base adobe \
  --head "sync/auto-resolve-<conflicting_short_sha>" \
  --title "Sync upstream: <subject> (auto-resolved superficial conflicts)" \
  --body-file "$TMP_DIR/agw_pr_body.md"
```

Verify freshness then force-update `adobe` via **`scripts/land_pr.py`**
(same as `batch-and-yolo.md` step 9):

```bash
git -C "$REPO" rev-parse HEAD
```

```bash
python3 "$SKILL_DIR/scripts/land_pr.py" "$REPO" \
  --pr <pr_number_from_gh_pr_create> \
  --head-sha <full_sha_from_rev_parse_above> \
  --expected-adobe <full_sha_from_freshness_capture_above> \
  --reason auto-resolve
```

Parse JSON; halt if `landed` is false or blocking errors.

Archive to `$REPORTS_DIR/auto-resolve-<conflicting_short_sha>/`.

### After successful land — checkout `adobe`, delete `sync/auto-resolve-*`

Same housekeeping as **`batch-and-yolo.md` step 10**: capture **`SYNC_BRANCH`**
from **`HEAD`** (must match **`sync/*`**), **`switch adobe`** + **`pull --ff-only origin adobe`**
(with **`inspect-and-prepare.md`** landing-artifact recovery if needed),
**`git push origin --delete "$SYNC_BRANCH"`** (best effort),
**`git branch -d "$SYNC_BRANCH"`**.

## 11. Return to dispatcher

The single conflicting commit landed. Increment dispatcher
`landed_count` by **1** (not `clean_count + 1` — the prefix already
incremented its share when its own batch landed).

If `landed_count >= TARGET_COUNT` or `unsynced_count == 0`, stop.
Otherwise loop back to a fresh `inspect-and-prepare.md` for the next
batch.

## Summary of dispatcher loop control

| Outcome | landed_count delta | Continue loop? |
|---|---|---|
| Full batch clean + tests pass | +batch_count | yes |
| Bisect partial + auto-resolve clean + tests pass | +clean_count then +1 | yes |
| Bisect partial + protected-path conflict | +clean_count (prefix already landed) | NO — manual merge required |
| Bisect partial + semantic hunk | +clean_count | NO — manual merge required |
| Bisect partial + auto-resolve clean + tests fail | +clean_count | NO — open PR with /land for review |
| Tests fail with no conflict | +0 | NO — open PR with /land for review |
| `clean_count == 0` (even commit #1 conflicts) | +0 | NO — manual conflict triage |
