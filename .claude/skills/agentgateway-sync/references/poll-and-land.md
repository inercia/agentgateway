# `/land` authorisation, then land

This reference is reached when a sync PR needs explicit **`/land`** before
force-updating `adobe`:

1. **Batch / auto-resolve PR with failing tests** — `[TESTS FAILING]` title,
   gate 2 failed after `--retry-once`.
2. **Any open sync PR** where automation deliberately skipped auto-land.
3. **Separate invocation** — the user said **land PR #N** directly — jump to
   section 2.

Happy-path batch+yolo PRs **auto-land** via `scripts/land_pr.py` and skip this
file entirely.

Landing uses **`scripts/land_pr.py`** (REST PATCH + PR comment + conditional
close + local ref reset). Reads still commonly use **`cloud-github` MCP** or
`gh pr view`.

## Who is a "maintainer"?

Anyone whose repository permission on `Adobe-Apis/agentgateway` is `admin`, `maintain`, or `write`. Read-only collaborators commenting `/land` must be ignored.

**There is deliberately no "approved review" check.** The `/land` comment from an authorised maintainer is the sole authorisation. Do NOT re-add an approval gate.

## 1. After opening a `/land`-gated PR (stateless hand-off)

Do **not** run background polling loops (`ScheduleWakeup`, foreground sleeps,
etc.) unless the user explicitly asks you to keep checking.

Default UX:

1. Post the PR URL in chat.
2. Tell the maintainer to comment **`/land`** when ready.
3. Tell them to **re-invoke** **`land PR #N`** after commenting so section 2 re-runs with fresh permission / freshness checks.
4. Stop — this invocation's job is done.

Why: polling burns prompt budget and isn't reliable across dropped sessions.
Explicit `land PR #N` is cheap and replays the full safety checklist.

## 2. One-shot land (`land PR #N`, or continuation after `/land`)

This is the authoritative landing path:

### 2.1 Fetch PR state

```
mcp__cloud-github__get_pull_request(owner="Adobe-Apis", repo="agentgateway", pull_number=<N>)
```

Record:
- `head.sha` — the PR head SHA (what we push into `adobe`)
- `head.ref` — sync branch name
- `base.ref` — must equal `adobe`; stop if not
- `state` — must be `open`; stop if closed or merged
- `body` — for freshness metadata

### 2.2 Parse freshness metadata

Scan the PR body for a line matching `adobe_at_creation:\s*([0-9a-f]{7,40})`. If missing, stop and tell the user the PR was opened before the skill tracked freshness metadata — they need to re-run the sync skill to regenerate the PR body, or confirm manually that `adobe` has not advanced.

If present, record the expected SHA.

### 2.3 Check `/land` commenter

1. **Fetch comments** (MCP or `gh pr view` / `gh api` — your choice).
2. A body matches if its **first non-whitespace token** is exactly `/land`
   (not `/landed`, not `/land-later`, not prose containing `/land` mid-line).
3. For each match, check permission:
   ```bash
   env -u GITHUB_TOKEN -u GH_TOKEN gh api repos/Adobe-Apis/agentgateway/collaborators/<login>/permission --jq .permission
   ```
   If it returns `admin`, `maintain`, or `write`, record that match and proceed.
4. If no authorised match exists, stop — tell the user the PR is not ready
   to land (or `/land` is missing). Record nothing.

Record the chosen commenter's login and comment time for the landing summary.

### 2.4 Freshness check — ask GitHub, not the local checkout

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh api repos/Adobe-Apis/agentgateway/git/refs/heads/adobe --jq .object.sha
```

If that SHA does not match the `adobe_at_creation` value from 2.2, **stop**. The fork has advanced since this PR was prepared — landing the PR head would silently lose whatever was added to `adobe` in between. Tell the user to re-run the sync skill against the current `adobe` tip (which will rebase the sync branch onto a fresh baseline and replace this PR with a new one).

### 2.5 Surface a landing summary and proceed immediately

No confirmation gate — all checks have already passed. Print:

- PR: `#<N>` — "<title>"
- Head SHA to push: `<head_sha>`
- Current `adobe`: `<current_sha>` (matches freshness marker — ok)
- `/land` by: `<commenter_login>` (permission: `<write|maintain|admin>`) at `<timestamp>`

### 2.6 Land via `scripts/land_pr.py`

All force-update + comment + conditional close + local `adobe` reset is
implemented in **`scripts/land_pr.py`** (REST flag semantics documented
there).

```bash
python3 "$SKILL_DIR/scripts/land_pr.py" "$REPO" \
  --pr <N> \
  --head-sha <head_sha> \
  --expected-adobe <adobe_at_creation_from_PR_body> \
  --reason manual-land
```

Parse JSON on stdout: `landed`, `landed_via`,
`landed_confirmed_by_ancestry`, `pr_state_after`, `comment_posted`,
`pr_closed`, `local_adobe_updated`, `errors`.

The authoritative success signal is `landed_confirmed_by_ancestry`
(`adobe` now points at the PR head SHA), not `pr_state_after`. If it is
false, stop — surface `errors` (freshness mismatch, `403`/`422` from
GitHub, etc.).

PR state after landing depends on GitHub: if it auto-detects the merge,
`pr_state_after` is `MERGED` and `pr_closed` stays false; otherwise the
script closes it and `pr_state_after` is `CLOSED`. **Both are expected**
for a force-push land — CLOSED is not a failure.

### 2.7 Report and continue

Report: PR #N landed as `<head_sha>`, `adobe` (local and remote) now points at `<head_sha>`, archive in `$REPORTS_DIR/<stem>/`.

If the dispatcher's loop has `landed_count < TARGET_COUNT` and `unsynced_count > 0`, return to the dispatcher to run the next sync cycle (`inspect-and-prepare.md` again). Otherwise stop.

### 2.8 Branch cleanup (`sync/*` PR head only)

After a successful land the PR’s head ref is disposable. Use **`head.ref`** from **§2.1**
as **`SYNC_BRANCH`** (do **not** rely on the local **`HEAD`** — the operator may have
been on **`adobe`** or another branch when **`land PR #N`** ran).

Only run the delete steps when **`SYNC_BRANCH`** matches **`sync/*`** (Adobe sync workflow).
If it does not, skip deletion and mention the unusual head ref.

1. Update local **`adobe`** and make it **`HEAD`**:

   ```bash
   git -C "$REPO" switch adobe
   ```

   If **`adobe`** is missing locally:

   ```bash
   git -C "$REPO" switch -c adobe --track origin/adobe
   ```

   Then:

   ```bash
   git -C "$REPO" pull --ff-only origin adobe
   ```

   If **`pull --ff-only`** fails, follow **`inspect-and-prepare.md`** § **Landing-artifact auto-recovery**,
   then retry **`pull --ff-only`**.

2. Remove the remote sync branch (**best effort** — protection rules may block):

   ```bash
   env -u GITHUB_TOKEN -u GH_TOKEN git -C "$REPO" push origin --delete "$SYNC_BRANCH"
   ```

   On failure: summarise for the user; land outcome is still valid.

3. Remove the local branch if present:

   ```bash
   git -C "$REPO" branch -d "$SYNC_BRANCH"
   ```

   Prefer **`-d`**; **`-D`** only when **`adobe`** already equals the landed tip but Git still refuses **`-d`**.

This keeps **`HEAD`** on **`adobe`** for the next **`inspect-and-prepare`** cycle or for a quiet working tree after the session ends.

## If any check in 2.1–2.4 fails

Post a comment on the PR explaining what blocked landing (helpful for the maintainer who typed `/land`), then stop. Do NOT attempt `land_pr.py` without all checks passing.

### 2.9 Optional internal follow-ups

If Jira, wiki, or similar MCP integrations are configured, update any linked tickets or docs **after** a successful land so downstream teams see the integration landed. **Skip silently** when no such MCP server is available — this step is optional.
