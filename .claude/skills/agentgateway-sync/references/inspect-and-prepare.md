# Inspect state and prepare `adobe`

This is the step that decides whether there's any work to do. One script call consolidates every read-only question the skill used to ask separately — working-tree cleanliness, remotes, branch state, unsynced count, next upstream commit + PR + source branch, Adobe-Apis reachability, repo-local token hygiene. When the dispatcher requested a count (`sync N`), the same call also returns the batch metadata for the first N unsynced commits.

## Run `inspect_state.py`

**One Bash call, stdout JSON** — parse the JSON in the tool result inline; no `$TMP_DIR/agw_state.json` roundtrip.

```bash
python3 "$SKILL_DIR/scripts/inspect_state.py" "$REPO" --count <TARGET_COUNT>
```

`<TARGET_COUNT>` is the resolved count from the dispatcher (default 1). The script caps internally at `unsynced_count`, so `sync 50` against a fork that's 12 commits behind returns `batch_count: 12`.

## Parse the JSON result

Two new fields drive routing downstream:

- `batch_requested` — what you asked for (echoes `--count`).
- `batch_count` — what you actually got (`min(requested, unsynced_count)`).
- `batch_end_sha` / `batch_end_short_sha` — last commit in the batch.
- `batch_commits` — array of `{sha, short_sha, subject, pr_num, author_date}` for every commit in the batch.

If `batch_count == 0`, fall through to the "nothing to sync" path below — there is no work regardless of intent.

If `batch_count >= 1`, route to **`references/batch-and-yolo.md`** after the
switch/ff section below.

## Classify the batch (informational only)

After `inspect_state.py` returns successfully and `batch_count > 0`, run **one** Bash invocation:

```bash
python3 "$SKILL_DIR/scripts/classify_batch.py" "$REPO" <sha1> <sha2> ... <shaN>
```

Inline every SHA from `batch_commits[*].sha`. Parse the JSON.

**Classification is informational, not gating.** Upstream is trusted; the actual gates are clean rebase + tests passing in `batch-and-yolo.md`. The aggregate just decides whether to surface a banner in the PR body:

- `aggregate.worst_risk == "critical"` or `aggregate.any_protected_path == true` → add a `⚠️ Heads up` banner to the PR body listing which commits triggered the flag, but **continue** the run.
- Otherwise no banner.

Carry the per-commit classification through to `push-and-open-pr.md` for the PR body's commit table.


Expected shape (annotated):

```json
{
  "working_tree_clean": true,
  "working_tree_preview": [],
  "current_branch": "adobe",
  "remote_origin_url": "https://github.com/Adobe-Apis/agentgateway.git",
  "remote_upstream_url": "https://github.com/agentgateway/agentgateway.git",
  "adobe_local": true,
  "unsynced_count": 235,
  "oldest_sha": "772c618820ad7384e435672fde76605f889429ea",
  "oldest_short_sha": "772c618820",
  "oldest_subject": "telemetry: make spans hierarchical and refine MCP telemetry (#1230)",
  "oldest_author_date": "2025-03-12T14:22:00Z",
  "pr_num": "1230",
  "source_branch": "telemetry/span-links",
  "adobe_apis_reachable": true,
  "token_leaks_repo_config": 0,
  "errors": []
}
```

### Error handling

**If `errors` is non-empty, surface every entry to the user and stop.** Common cases:

| Error text | Meaning | Resolution |
|---|---|---|
| `"... is not a git checkout"` | Wrong `REPO` path. | Stop. Ask the user for the correct path. |
| `"upstream remote is ..."` | `upstream` is missing or points somewhere other than `github.com/agentgateway/agentgateway.git`. | Add or fix with `git remote add` / `git remote set-url` to `https://github.com/agentgateway/agentgateway.git` and rerun. |
| `"... contains N embedded token(s)"` | Credential leak in repo-local `.git/config`. | Rotate the token and clean the file before any push. |
| `"Adobe-Apis/agentgateway not reachable via gh"` | SSO auth missing. | Point the user at the authorisation URL from `preconditions.md`. |
| `"oldest commit subject has no trailing (#NNNN)"` | Upstream did a non-squash merge. | Surface the subject, stop, let the user decide. |

**If `working_tree_clean == false`**, surface the `working_tree_preview` lines (up to 10 entries from `git status --porcelain`) and stop. Never auto-stash or auto-commit — the user may have in-progress work.

**If `unsynced_count == 0`**, report "nothing to sync" and stop.

## Switch to `adobe` and fast-forward

Based on `adobe_local`, emit exactly one of these two Bash calls (not both, not a shell `if`):

```bash
# When adobe_local is true:
git -C "$REPO" switch adobe
```

```bash
# When adobe_local is false (branch only exists on origin):
git -C "$REPO" switch -c adobe --track origin/adobe
```

Then a separate Bash call for the fast-forward:

```bash
git -C "$REPO" pull --ff-only origin adobe
```

### Landing-artifact auto-recovery

If the fast-forward fails because local `adobe` has diverged from `origin/adobe`, first check whether it's a **landing artifact** — a previous sync force-updated `origin/adobe` but the local pointer was never moved. The content is already on origin; only the SHA identities differ.

1. Get the subjects of commits unique to local:
   ```bash
   git -C "$REPO" log --format="%s" origin/adobe..adobe
   ```
2. Get all subjects reachable from `origin/adobe`:
   ```bash
   git -C "$REPO" log --format="%s" origin/adobe
   ```
3. **If every subject from step 1 appears in step 2's output**, the local commits are just the Adobe work rebased onto a newer upstream base — content is already on origin with different SHAs. Reset the local pointer without touching the working tree:
   ```bash
   git -C "$REPO" update-ref refs/heads/adobe refs/remotes/origin/adobe
   ```
4. **If any subject from step 1 is NOT in step 2's output**, the user has genuine unpushed work. Stop and surface the divergence. Do not auto-resolve.

## Display sync target and proceed (no confirmation gate)

Show the user, compactly, then continue immediately without waiting:

- **Total unsynced:** `unsynced_count` (e.g. "235 commits behind upstream").
- **Batch size:** `batch_count` (e.g. "syncing 50 of 235").
- **Range:** `batch_commits[0].short_sha` ("...") → `batch_end_short_sha` ("<batch_commits[-1].subject>").
- **Aggregate risk** (info only): `<worst_risk>` — `<label_counts>` (e.g. `MERGE_SAFE: 48, NEEDS_REVIEW: 2`).

## Return to the dispatcher

`adobe` is now clean and up-to-date. Proceed to **`references/batch-and-yolo.md`**
(the default flow for any `sync N`, including N=1).
