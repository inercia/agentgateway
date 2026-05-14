# Poll for `/land`, then land

Two entry points:

- **Automatic continuation** from `push-and-open-pr.md` — the PR was just opened and the skill is now polling for a maintainer `/land` comment.
- **Separate invocation** — the user said "land PR #N" directly. Skip the polling loop; go straight to the one-shot check in section 2.

Landing is fully Claude-driven: `cloud-github` MCP for reads, `gh api` for the force-update. No GitHub Action, no bot actor. The maintainer who comments `/land` must have push access to `adobe` (same prerequisite as if they'd force-pushed manually).

## Who is a "maintainer"?

Anyone whose repository permission on `Adobe-Apis/agentgateway` is `admin`, `maintain`, or `write`. Read-only collaborators commenting `/land` must be ignored.

**There is deliberately no "approved review" check.** The `/land` comment from an authorised maintainer is the sole authorisation. Do NOT re-add an approval gate.

## 1. Polling loop (automatic-continuation entry only)

### Cadence: `ScheduleWakeup` at 1500 s

The v1 skill used a foreground `sleep 300` loop. v2 uses `ScheduleWakeup` in `/loop` dynamic mode at **1500 s (25 min)** cadence:

- 300 s is the literal worst case: past the 5-minute prompt-cache TTL, so every wake pays a cache miss — yet it's too frequent to amortize. ~575 cache-cold wakes over 48 h.
- 1500 s: one cache miss buys a real wait; ~115 wakes over 48 h.
- `ScheduleWakeup` stays local, reuses the user's `gh auth`, and frees the current Claude session — the user can do other work in between wakes.
- Do **not** use the `schedule` skill: that spawns remote agents on Anthropic infra without access to `gh auth`, Adobe SSO, or the local git state.

### Per-poll procedure

On entry (both the immediate first attempt and every subsequent wake):

1. **Fetch comments:**
   ```
   mcp__cloud-github__get_pull_request_comments(owner="Adobe-Apis", repo="agentgateway", pull_number=<N>)
   ```
2. **Scan each comment body.** A body matches if its **first non-whitespace token** is exactly `/land` — not `/landed`, not prose with `/land` mid-text, not a slash-command with a suffix like `/land-later`. Allow `/land` followed by whitespace, a newline, or end-of-string.
3. **For each match, check the commenter's permission:**
   ```bash
   env -u GITHUB_TOKEN -u GH_TOKEN gh api repos/Adobe-Apis/agentgateway/collaborators/<login>/permission --jq .permission
   ```
   If it returns `admin`, `maintain`, or `write`, record the match and **go to section 2**. Exit the polling loop.
4. **Check PR state:**
   ```
   mcp__cloud-github__get_pull_request(owner="Adobe-Apis", repo="agentgateway", pull_number=<N>)
   ```
   If `state == "closed"`, someone closed the PR without landing. **Stop** — do not force-push.
5. **If neither condition fires**, schedule the next wake:
   ```
   ScheduleWakeup(
     delaySeconds: 1500,
     prompt: "continue polling sync PR #<N> for /land comment",
     reason: "polling PR #<N> for /land, no match yet"
   )
   ```
   Print a one-line status the user can see: `[poll #<K>, elapsed <H>h <M>m] no /land yet on PR #<N>; next check in 25 min`.

### Safety ceiling

Track `elapsed_hours` (total wall-clock time since the first poll). After **48 hours**, stop the loop. Report to the user: "48 h without a `/land` — the maintainer may need a nudge via another channel. Re-invoke 'land PR #<N>' when they comment."

This 48 h counter is **not** enforced by the runtime — it is enforced by Claude counting iterations across wakes. Maintain it in the prompt passed to `ScheduleWakeup` (e.g. include "poll #47 of 115") or derive from the PR's `createdAt` on each wake.

### User interrupt

The user can interrupt at any time (Ctrl-C during an active wake, or just dropping the session). Re-invocation of `land PR #<N>` re-enters via section 2 below.

## 2. One-shot check (separate-invocation entry, or post-poll-match)

This is the single path used for:
- Automatic continuation after the polling loop fired.
- User invoking "land PR #N" directly.

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

### 2.3 Check `/land` commenter (separate-invocation entry only)

If you arrived here from the polling loop, you already recorded an authorised commenter — skip this.

If the user invoked landing directly, run the same scan from section 1 step 2–3: find the first `/land` whose commenter has `admin`/`maintain`/`write` permission. If none exists, stop and tell the user there is no maintainer landing request on this PR. Record the commenter's login and the comment's creation time.

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

### 2.6 Force-update `adobe`

Single `gh api` call. The REST endpoint for updating a ref accepts `force: true` to allow non-fast-forward updates.

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh api \
  -X PATCH repos/Adobe-Apis/agentgateway/git/refs/heads/adobe \
  -f sha=<head_sha> \
  -F force=true
```

**Flag gotcha:** `-F force=true` (capital F, JSON boolean) is required; `-f force=true` (lowercase f, string) is interpreted as the string `"true"` and ignored by the API.

**On `422 "Update is not a fast forward"`:** the `-F` flag was wrong. Fix and retry.

**On `403` / branch protection error:** the account running Claude doesn't have push permission to `adobe`, or branch protection blocks non-fast-forward pushes even for this account. Stop and tell the user — this is repo config, not something the skill can work around.

### 2.7 Post confirmation, then close the PR if it's still open

Post the landing comment first (always safe regardless of PR state):

```
mcp__cloud-github__add_pull_request_comment(
  owner="Adobe-Apis",
  repo="agentgateway",
  pull_number=<N>,
  body="Landed as `<head_sha>` on `adobe` via force-push. See https://github.com/Adobe-Apis/agentgateway/commit/<head_sha>"
)
```

Now re-fetch the PR state before deciding whether to close it — the force-update in 2.6 may have caused GitHub to auto-flip the PR to `merged`:

```
mcp__cloud-github__get_pull_request(owner="Adobe-Apis", repo="agentgateway", pull_number=<N>)
```

Branch on `state`:

- **`state == "merged"`** — GitHub detected that the PR head is reachable from `adobe` (exactly what the force-update did) and auto-merged the PR. **Skip the close step.** Tell the user "PR #<N> auto-merged after force-update." This is actually the preferred end state — a stronger audit trail than `closed`.
- **`state == "open"`** — close it:
  ```bash
  env -u GITHUB_TOKEN -u GH_TOKEN gh pr close <N> --repo Adobe-Apis/agentgateway
  ```
- **`state == "closed"`** — someone closed it externally in the brief window between 2.6 and this check. Skip; nothing to do.

**Do NOT** call `gh pr close` unconditionally. It errors with `"can't be closed because it was already merged"` when GitHub auto-merged, and that non-zero exit aborts the rest of the landing flow (steps 2.8 local reset, 2.9 report) — the sync is complete but the skill appears to have failed.

Either way, `adobe`'s tip matching the PR head is the audit trail; `merged` and `closed` are both acceptable terminal states.

### 2.8 Reset local `adobe`

`origin/adobe` now points at `<head_sha>`, but the local branch pointer is still at the old tip (we've been on the sync branch throughout). Without this reset, the next invocation's `pull --ff-only` fails because `origin/adobe` is no longer a fast-forward from the local tip.

Two separate Bash calls — no `&&` chaining:

```bash
git -C "$REPO" fetch origin adobe
```

```bash
git -C "$REPO" update-ref refs/heads/adobe refs/remotes/origin/adobe
```

`update-ref` moves the local pointer without touching the working tree or HEAD — safe to run while on the sync branch.

### 2.9 Report and continue

Report: PR #N landed as `<head_sha>`, `adobe` (local and remote) now points at `<head_sha>`, archive in `$REPORTS_DIR/<stem>/`.

If the dispatcher's loop has `landed_count < TARGET_COUNT` and `unsynced_count > 0`, return to the dispatcher to run the next sync cycle (`inspect-and-prepare.md` again). Otherwise stop.

## If any check in 2.1–2.4 fails

Post a comment on the PR explaining what blocked landing (helpful for the maintainer who typed `/land`), then stop. Do NOT attempt `gh api PATCH` without all checks passing.

### 2.10 Optional internal follow-ups

If Jira, wiki, or similar MCP integrations are configured, update any linked tickets or docs **after** a successful land so downstream teams see the integration landed. **Skip silently** when no such MCP server is available — this step is optional.
