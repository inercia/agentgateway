# Push, open PR, archive

Assumes `rebase-and-test.md` completed with a passing post-rebase run. You have these values in context:

- `sync_branch`, `suffix`, `superseded_prs`
- `oldest_sha`, `pr_num`, `source_branch`, `oldest_subject`
- Baseline vs. synced test counts (and whether synced needed a retry)

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

**Use the Write tool** to create `$TMP_DIR/agw_pr_body.md`. (`gh pr create --body-file` needs a file; inline body via `--body` mangles multi-line markdown.)

Template:

```markdown
## Upstream commit synced

- `<oldest_sha>` — [view commit](https://github.com/agentgateway/agentgateway/commit/<oldest_sha>)
- Upstream PR: [agentgateway/agentgateway#<pr_num>](https://github.com/agentgateway/agentgateway/pull/<pr_num>)
- Subject: `<oldest_subject>`

## Test results

| Run | Passed | Failed | Ignored | Warnings | Suites OK |
|---|---|---|---|---|---|
| Baseline | 794 | 0 | 4 | 0 | 12 |
| Post-rebase | 798 | 0 | 4 | 0 | 12 |

(If synced needed a retry, add a line: "Post-rebase run needed one retry — first attempt had flakes; retry passed.")

## Classification (from `classify_commit.py`)

Paste the JSON summary:

| Field | Value |
|---|---|
| label | `<MERGE_SAFE|NEEDS_REVIEW|SECURITY|SKIP>` |
| risk | `<low|medium|high|critical>` |
| reason | `<short text>` |

If `label` is `SECURITY` or `risk` is `high`/`critical`, repeat the highlights from `matched_patterns` / `protected_hits` so reviewers see them above the fold.

## Suggested CHANGELOG (optional)

Use the `changelog-entry` skill to draft bullets for `adobe/CHANGELOG.md`; paste the proposed text here for reviewer visibility (human still confirms before committing changelog updates on the integration branch).

## Supersedes

(Only include this section if `superseded_prs` is non-empty.)

- #11 (closed) — https://github.com/Adobe-Apis/agentgateway/pull/11

## Landing

A maintainer (anyone with `write`, `maintain`, or `admin` permission on this repo) commenting `/land` on this PR will cause the already-running sync skill to verify freshness and force-update `adobe`. No re-invocation is needed — a Claude session is polling this PR every ~25 minutes and will pick up the comment automatically.

There is deliberately no requirement for a GitHub "Approve" review; the `/land` comment is the sole authorisation.

Do NOT use GitHub's native merge buttons — they either conflict ("Rebase and merge"), produce duplicate history ("Create a merge commit"), or collapse per-commit attribution ("Squash and merge"). All three are wrong for this workflow. See `references/merge-strategies.md` in the skill dir for why.

<!-- sync-metadata
adobe_at_creation: <full_sha_from_step_1>
-->
```

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

If the synced run needed a retry (from `rebase-and-test.md`), prefix the body with `(retry)`: `"✅ Post-rebase `make test` (retry): …"`.

## 6. Archive to `$REPORTS_DIR/<stem>/`

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

**Archive here — before the polling loop in `poll-and-land.md`** — so if polling is interrupted or the maintainer takes days to `/land`, the sync record is still on disk.

## 7. Proceed to landing

Call `poll-and-land.md` with the PR number. That reference uses `ScheduleWakeup` so the current Claude session doesn't have to stay live for the full polling window.
