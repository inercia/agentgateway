# Inspect state and prepare `adobe`

This is the step that decides whether there's any work to do. One script call consolidates every read-only question the skill used to ask separately — working-tree cleanliness, remotes, branch state, unsynced count, next upstream commit + PR + source branch, Adobe-Apis reachability, repo-local token hygiene.

## Run `inspect_state.py`

**One Bash call, stdout only** — do not pass `--output`. Parse the JSON in the tool result inline; no `$TMP_DIR/agw_state.json` roundtrip.

```bash
python3 "$SKILL_DIR/scripts/inspect_state.py" "$REPO"
```

(These scripts ship beside this skill. `inspect_state.py` writes JSON to stdout when `--output` is omitted.)

## Parse the JSON result


## Classify oldest commit (risk gate)

After `inspect_state.py` returns successfully and `unsynced_count > 0`, run **one** Bash invocation:

```bash
python3 "$SKILL_DIR/scripts/classify_commit.py" "$REPO" "<oldest_sha>"
```

Parse the JSON:

- **`risk == "critical"`** (or `SECURITY` with any `pattern_hits` from the critical pattern list in `classify_commit.py`): **halt** the sync — surface `reason`, `protected_hits`, and `matched_patterns` to the user. Do not rebase or open a PR until a human explicitly overrides.
- **`risk == "high"`:** pause and obtain **explicit chat acknowledgment** before continuing (even if the label is `NEEDS_REVIEW`).
- Otherwise continue into the switch/ff section below.


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
- **Next commit:** `oldest_short_sha` + `oldest_subject`.
- **Upstream PR:** `https://github.com/agentgateway/agentgateway/pull/<pr_num>` plus the source branch (`source_branch`).

## Worktree isolation (optional, recommended)

If the user has in-progress work on another branch of `REPO` that they don't want disturbed, use a dedicated git worktree for the sync:

```bash
git -C "$REPO" worktree add "../agentgateway-sync-wt-<timestamp>" adobe
```

Then set `REPO` to the worktree path for the remainder of this invocation. After `gh pr create` returns (end of `push-and-open-pr.md`), clean up:

```bash
git -C "$REPO_ORIG" worktree remove "../agentgateway-sync-wt-<timestamp>"
```

The default behaviour is **to sync in-place** — only enter a worktree when the user's state warrants it (dirty working tree on a feature branch they want preserved, or explicit request for isolation).

## Return to the dispatcher

`adobe` is now clean and up-to-date. Proceed to `rebase-and-test.md`.
