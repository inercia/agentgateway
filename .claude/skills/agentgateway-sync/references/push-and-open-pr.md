# Push, open PR, archive

Two callers share **`scripts/compose_pr_body.py`**:

1. **Single-commit PR** — `--kind single` (rare manual paths).
2. **Batch PR** — `--kind batch` (`batch-and-yolo.md` default).

**§3 below** documents the preferred **`--merge`** workflow ( **`inspect_state`**
+ **`classify_*`** + **`run_tests`** JSON on disk). **`--print-template`** +
**`--json-file`** remain a fallback; see **`scripts/compose_pr_body.py`** module docstring.

Assumes tests completed with counts in context:

- `sync_branch`, `suffix`, `superseded_prs`
- Single-commit: `oldest_sha`, `pr_num`, `oldest_subject`, classification JSON
- Batch: `batch_count`, `batch_commits`, `batch_end_sha`, `classify_batch.py`
  per-commit rows (`files`, `label`, `risk`), aggregate banner flags
- Baseline vs. synced test counts (`run_tests.py` JSON), `retry_note`,
  optional `tests_fail_tail`

## 1. Capture `adobe` SHA as freshness metadata

**Before** pushing, capture the current remote `adobe` tip. This goes into the PR body and is what `poll-and-land.md` checks at landing time to refuse if the fork has advanced.

```bash
git -C "$REPO" rev-parse origin/adobe
```

Record the full SHA. You will inline it into the PR body below as `adobe_at_creation: <full_sha>`.

## 2. Push the sync branch

Git consults the `gh auth git-credential` helper and uses the OAuth token from `gh`'s keyring — which has the `workflow` scope required for commits that touch `.github/workflows/*`.

```bash
git push -u origin "sync/telemetry-span-links-1"
```

If you re-push after another round of rebase (e.g. because a conflict resolution landed), use `--force-with-lease` — it protects against clobbering someone else's concurrent push. **Never** use bare `--force`.

```bash
git push --force-with-lease -u origin "sync/telemetry-span-links-1"
```

### Push failure cases

- **`403` with "missing `workflow` scope":** cached gh token lacks that scope. Refresh:
  ```bash
  env -u GITHUB_TOKEN -u GH_TOKEN gh auth refresh -h github.com -s workflow
  ```
  Then retry the push.
- **`401` / `authentication failed` but `gh auth status` shows the token is valid:** a `url.insteadOf` entry in `~/.gitconfig` is likely injecting a different (older) token ahead of the gh helper. Run `grep -nE 'insteadOf|ghp_|github_pat_' ~/.gitconfig`. If any `[url "https://...@github.com/"]` block shows up, remove it (`git config --global --remove-section 'url.<full-url-with-token>'`) and retry.

## 3. Compose the PR body

**Preferred (`--merge`)** — **`compose_pr_body.py`** reads **`inspect_state`** /
**`classify_*`** / **`run_tests`** JSON files and joins them so the agent never
hand-merges **`batch_commits`** rows (the failure mode behind empty-subject PR tables).

**Batch**

```bash
python3 "$SKILL_DIR/scripts/compose_pr_body.py" --kind batch --merge \
  --inspect-state "$TMP_DIR/inspect.json" \
  --classify-batch "$TMP_DIR/classify.json" \
  --baseline-tests "$TMP_DIR/run_tests_baseline.json" \
  --post-tests "$TMP_DIR/run_tests_synced.json" \
  --adobe-at-creation "$(git -C "$REPO" rev-parse origin/adobe)" \
  --out "$TMP_DIR/agw_pr_body.md"
```

**Single-commit**

```bash
python3 "$SKILL_DIR/scripts/compose_pr_body.py" --kind single --merge \
  --inspect-state "$TMP_DIR/inspect.json" \
  --classify-commit "$TMP_DIR/classify_commit.json" \
  --baseline-tests "$TMP_DIR/run_tests_baseline.json" \
  --post-tests "$TMP_DIR/run_tests_synced.json" \
  --adobe-at-creation "$(git -C "$REPO" rev-parse origin/adobe)" \
  --out "$TMP_DIR/agw_pr_body.md"
```

(**`classify_commit.json`** — stdout from **`classify_commit.py`** for **`oldest_sha`**.)

**Auto-resolve** — bisection JSON plus agent-authored conflict rows:

1. Write **`$TMP_DIR/bisect.json`** from **`find_clean_prefix.py`** stdout.
2. Write **`$TMP_DIR/resolution_rows.json`** — a JSON **array** of
   **`{"file": "...", "rule": "RULE_NAME", "notes": ""}`** (one object per
   conflicted file from the auto-resolve triage).

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

If stderr shows **`{"ok": false, "errors": [...]}`** or **`render_errors`**, fix the
inputs — do not patch **`compose_pr_body.py`** blindly.

### Fallback / manual path (`--json-file`)

Use **`--print-template <kind>`** for starter JSON, fill fields by hand (see the
module docstring at the top of **`scripts/compose_pr_body.py`**), then:

```bash
python3 "$SKILL_DIR/scripts/compose_pr_body.py" --kind batch \
  --json-file "$TMP_DIR/agw_pr_body_input.json" \
  --out "$TMP_DIR/agw_pr_body.md"
```

`gh pr create --body-file` requires the rendered markdown file; never pass
multi-line markdown via `--body`.

## 4. Open the PR

One Bash call. Inline the concrete `--head`, `--title`, and `--body-file` values.

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh pr create \
  --repo Adobe-Apis/agentgateway \
  --base adobe \
  --head "sync/telemetry-span-links-1" \
  --title "Sync: telemetry: make spans hierarchical and refine MCP telemetry (#1230)" \
  --body-file "$TMP_DIR/agw_pr_body.md"
```

Record the PR number from the URL it prints.

## 5. Post test-results comment

The PR body already has the table, but a pinned comment is more visible in the review UI.

```
mcp__cloud-github__add_pull_request_comment(
  owner="Adobe-Apis",
  repo="agentgateway",
  pull_number=<N>,
  body="✅ Post-rebase `make test`: 798 passed, 12 suites ok, 4 ignored."
)
```

If `needed_retry` from `run_tests.py --retry-once`, prefix the comment body with `(retry)`: `"✅ Post-rebase \`make test\` (retry): …"`.

## 6. Archive to `$REPORTS_DIR/<stem>/`

**Location:** `$REPORTS_DIR` is **`$REPO/.git/sync-reports`** (see `SKILL.md`) —
same rationale as **`TMP_DIR`**: archives live under **`.git/`** so **`git status`**
does not pick up new **`adobe/`** files after each sync.

Two files written via the **Write** tool (not Bash):

- Directory: `$REPORTS_DIR/<sync_branch_without_prefix>/`
  - `sync_branch = "sync/telemetry-span-links-1"` → `$REPORTS_DIR/telemetry-span-links-1/`
- `$REPORTS_DIR/<stem>/pr-body.md` — identical to the body just shipped (copy).
- `$REPORTS_DIR/<stem>/report.md` — short summary:
  - Sync date
  - Upstream commit + PR link
  - Source branch → sync branch (with suffix + prior superseded PRs if any)
  - Baseline vs. synced test counts
  - Final PR URL from step 4
  - Whether synced needed a retry

Logs (`$TMP_DIR/agw_make_test_*.log`) are intentionally NOT archived — they regenerate on demand, each is 200 KB+, and the project `.gitignore` globs `*.log`.

**Archive before handing off to landing** — so if `/land` is delayed,
the sync record is still on disk.

## 7. Proceed to landing

Tell the maintainer the PR URL. When `/land` is commented,
**re-invoke** `land PR #N` so `references/poll-and-land.md` section 2 runs
(the skill does not poll GitHub in the background).

For the batch happy path, step 7 is **auto-land** — see `batch-and-yolo.md`
step 9 (`scripts/land_pr.py`).

### Batch PR title format

```
Sync upstream main: <count> commits (<oldest_short>..<batch_end_short>)
```

### Batch archive notes

`$REPORTS_DIR/<sync-branch-without-prefix>/report.md` for batch runs
should additionally include:

- The bisection log if `find_clean_prefix.py` ran (subset clean_count,
  conflicting commit, attempts).
- Whether auto-land succeeded or fell back to `/land`.
- Aggregate classification summary.
