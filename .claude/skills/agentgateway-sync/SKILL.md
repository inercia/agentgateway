---
name: agentgateway-sync
description: "Port the next upstream commit from agentgateway/agentgateway:main into Adobe-Apis/agentgateway:adobe by rebasing Adobe-only commits onto it and opening a PR from a `sync/<source-branch>` branch. Also lands an approved sync PR by force-updating `adobe` to the PR head after verifying a maintainer `/land` comment. Use when the user asks to 'sync', 'port', 'pull in', 'catch up with', 'run the weekly sync from', 'rebase on' upstream agentgateway, references a specific upstream PR (e.g. 'port PR #1252'), or asks to advance `adobe`. Also use for 'land sync PR', 'land PR #N', 'check for /land comments', or to finalise an open sync PR. Do NOT use for arbitrary git rebase/cherry-pick questions or unrelated repos."
---

# Adobe agentgateway upstream sync — dispatcher

This is a thin dispatcher. The full workflow lives in `references/*.md`, loaded on demand so a typical invocation only pays for the prose it actually needs.

## Cross-cutting invariants (always apply)

Read these once at the start of every invocation. They govern every Bash call you emit.

### Branch shape

Linear: the one upstream commit sits at the base (verbatim SHA, author, message, parent) and the Adobe-only commits are reapplied on top (new SHAs — rebase rewrites identity when the parent changes, which is fine). This is "Shape D" in `references/merge-strategies.md`. Read that file only if the user asks why a particular merge button is wrong or why the PR diff shows more than two files.

### Branch naming

`sync/<source-branch>` where `<source-branch>` is the upstream PR's `head.ref` with `/` normalised to `-`. On collision with a prior branch (on origin) or a prior PR (Adobe-Apis, any state), append `-1`, `-2`, … The `pick_sync_branch.py` script handles this — you never pick the name by hand.

### `gh` CLI environment prefix

Every `gh` call **must** be prefixed with `env -u GITHUB_TOKEN -u GH_TOKEN` so a stale PAT in the shell environment doesn't mask the `hosts.yml` credential. No exceptions.

### Git invocation discipline

1. **Never** emit a Bash command that starts with `cd <path> && git …`. It triggers the "untrusted hooks from the target directory" approval prompt on every call. Capture the repo path once (`REPO`) and thread it through with `git -C "$REPO" …`.
2. **One git operation per Bash call.** No `&&` chaining. No `$(…)` substitution inside git commands. Each call should match a stable allowlist pattern like `Bash(git -C * rebase *)`.
3. When a script prints JSON to stdout, **parse the Bash tool's result inline** — scripts support optional `--output`, but prefer stdout for `inspect_state.py` / `pick_sync_branch.py` / `classify_commit.py`. The only files that still go through `$TMP_DIR` are the `make test` log (too large for stdout) and the composed PR body (`gh pr create --body-file` needs a file).

### Git remotes layout

`origin` must be **Adobe-Apis/agentgateway**; `upstream` (and optional `public`) must be **agentgateway/agentgateway**. Agents and fresh clones run **`ensure_git_remotes.py`** before **`inspect_state.py`** so URL variants (SSH vs HTTPS, trailing `.git`) and missing `upstream`/`public` are caught or fixed idempotently. See `references/preconditions.md` section 0.

Paths:

- `SKILL_DIR` — the directory containing this `SKILL.md` (`.claude/skills/agentgateway-sync/`).
- `SCRIPTS_DIR` — `"$SKILL_DIR/scripts"` (contains `ensure_git_remotes.py`, `inspect_state.py`, `pick_sync_branch.py`, `run_tests.py`, `classify_commit.py`).
- `REPO` — absolute path of the user's local `Adobe-Apis/agentgateway` checkout. **Default:** the current working directory (`$PWD`) when the user runs from the clone root.
- `TMP_DIR` — `"$REPO/.git/sync-tmp"`. `mkdir -p "$TMP_DIR"` before first use (ignored by git).
- `REPORTS_DIR` — `"$REPO/adobe/sync-reports"`. Versioned summaries land here.

## Intent routing

Classify the user's message into exactly one intent. If ambiguous, ask once — don't guess.

| Intent | Triggers | Action |
|---|---|---|
| **new-sync** | "sync", "sync N", "port", "port N", "port PR #X", "pull in", "catch up", "run the weekly sync", "rebase on upstream" | Follow the new-sync path below. |
| **landing** | "land PR #N", "land the sync PR", "check for /land", "force-push the approved sync PR", "finalise PR #N" | Follow the landing path below. |
| **status** | "sync status", "what's pending", "any open sync PRs" | Load `references/inspect-and-prepare.md`, run `inspect_state.py`, summarise state. Do not mutate. |

### new-sync path

1. **Parse target count.** Scan the user message for an explicit number ("sync 5", "port 3"). Default to `1`. Store as `TARGET_COUNT`; initialise `landed_count = 0`.
2. **Tell the user** the resolved count ("Syncing up to N commits."). 
3. **Load `references/preconditions.md` section 0** — run `ensure_git_remotes.py` (dry-run, then `--apply` if needed). Stop on non-fixable `origin` errors.
4. **Load and follow** the rest of `references/preconditions.md` — verify `gh` auth, Adobe fork reachability, upstream remote, token hygiene. Stop on any failure.
5. **Load and follow** `references/inspect-and-prepare.md` — run `inspect_state.py`, parse the JSON from stdout, handle the `adobe` switch and fast-forward (including landing-artifact auto-recovery). This is the step that decides whether there's anything to do. Run `classify_commit.py` per the risk gate in that reference.
6. **Load and follow** `references/rebase-and-test.md` — baseline test, pick sync branch, rebase. If rebase stops on a conflict, branch to `references/conflict-triage.md` and return here when the rebase completes.
7. **Load and follow** `references/push-and-open-pr.md` — push, compose PR body, open PR, post test-results comment, archive to `$REPORTS_DIR/<stem>/`.
8. **Load and follow** `references/poll-and-land.md` — poll for `/land` using `ScheduleWakeup` (25-min cadence, 48 h ceiling), then force-update `adobe`.
9. **Loop.** Increment `landed_count`. If `landed_count >= TARGET_COUNT` or `unsynced_count == 0`, stop. Otherwise return to step 5 (re-inspect state) and continue.

### landing path

The user invoked landing directly — a PR is already open and they want it landed now (either `/land` has just been commented, or they want to check whether one has appeared).

1. **Load and follow** `references/preconditions.md` — including section 0 (`ensure_git_remotes.py`) when the clone may be fresh or shared, then the usual `gh` auth checks.
2. **Load `references/poll-and-land.md`** — start at the "separate-invocation entry" section (skip the polling loop, go straight to the one-shot `/land` check and freshness verification). If no authorised `/land` comment exists, tell the user and stop; do not fall into a poll loop unless they explicitly ask for one.

### status path

1. **Load and follow** `references/inspect-and-prepare.md` for the inspection sequence (classification gate applies when `unsynced_count > 0`).
2. Additionally list open Adobe-Apis PRs with `head.ref` matching `sync/*`:
   ```bash
   env -u GITHUB_TOKEN -u GH_TOKEN gh pr list --repo Adobe-Apis/agentgateway --state open --search "head:sync/" --json number,title,headRefName,createdAt
   ```
3. Report: unsynced count, oldest commit, any open sync PRs with their age. Do not mutate.

## Red flags — stop rather than push through

These apply across all paths. Any one of them means stop and surface to the user; never auto-recover.

- Baseline `make test` has any failures or new warnings.
- Semantic rebase conflict (see `references/conflict-triage.md` for the superficial-vs-semantic rule).
- SSO error on `gh api repos/Adobe-Apis/*` (not recoverable without browser).
- Embedded token in `.git/config`, `~/.gitconfig`, or `git remote -v` output.
- Upstream commit subject has no `(#NNNN)` suffix (non-standard merge).
- Post-rebase test counts show failures or new warnings vs. baseline.
- `adobe..upstream/main` (or `origin/adobe..upstream/main` when local branch is missing) returns unexpectedly empty or much smaller than anticipated.
- Freshness check at landing time shows `adobe` has moved since the PR was created.

## Stopping condition

A full new-sync invocation runs the 9 steps above in order, landing one upstream commit per loop iteration. The skill exits after `landed_count >= TARGET_COUNT`, `unsynced_count == 0`, user interrupt, a post-rebase test double-failure (before any push), or the 48 h polling ceiling without a `/land` comment.
