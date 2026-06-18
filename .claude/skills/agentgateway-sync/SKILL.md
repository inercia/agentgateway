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

### Bash / `gh` / MCP conventions

Follow **`references/conventions.md`** — `env -u GITHUB_TOKEN -u GH_TOKEN gh …`,
`git -C "$REPO" …`, single-git-op Bash calls where you emit commands yourself,
and when to prefer `cloud-github` MCP reads vs `gh` writes.

Parse JSON from helper scripts on **stdout** (`inspect_state.py`, `pick_sync_branch.py`,
`classify_batch.py`, `find_clean_prefix.py`, `land_pr.py`, …). Larger artifacts use
files: `run_tests.py --log-dir`, `compose_pr_body.py --out` (markdown for
`gh pr create --body-file`).

**`compose_pr_body.py`:** Prefer **`--merge`** (**`references/push-and-open-pr.md` §3**)
— pass **`inspect_state`**, **`classify_*`**, and **`run_tests --output`** JSON paths; the script
joins **`batch_commits`** with classification and refuses placeholder rows **before** writing
markdown. Fallback: **`--print-template`** + **`--json-file`** (see the module docstring atop
**`scripts/compose_pr_body.py`**). Validation / render leak errors print structured JSON to
**stderr** (exit **2**).

### Git remotes layout

`origin` must be **Adobe-Apis/agentgateway**; `upstream` (and optional `public`) must be **agentgateway/agentgateway**. Before inspecting, follow **`references/preconditions.md` section 0** — run `inspect_state.py "$REPO" --fix-remotes` when agents need to auto-repair `upstream`/`public` (never `origin`).

Paths:

- `SKILL_DIR` — the directory containing this `SKILL.md` (`.claude/skills/agentgateway-sync/`).
- `SCRIPTS_DIR` — `"$SKILL_DIR/scripts"` (`inspect_state.py`, `pick_sync_branch.py`, `run_tests.py`, `classify_commit.py`, `classify_batch.py`, `find_clean_prefix.py`, `land_pr.py`, `compose_pr_body.py`, `check_protected_paths.py`).
- `REPO` — absolute path of the user's local `Adobe-Apis/agentgateway` checkout. **Default:** the current working directory (`$PWD`) when the user runs from the clone root.
- `TMP_DIR` — `"$REPO/.git/sync-tmp"`. `mkdir -p "$TMP_DIR"` before first use (ignored by git).
- `REPORTS_DIR` — `"$REPO/.git/sync-reports"`. PR summaries / body copies land here alongside **`TMP_DIR`**, under **`.git/`**, so **`git status`** on the checkout stays clean (nothing under **`adobe/`**). `mkdir -p "$REPORTS_DIR"` before first archive write.

## Intent routing

Classify the user's message into exactly one intent. If ambiguous, ask once — don't guess.

| Intent | Triggers | Action |
|---|---|---|
| **new-sync** | "sync", "sync N", "sync up to date", "sync all", "sync to upstream/main", "port", "port N", "port PR #X", "pull in", "catch up", "run the weekly sync", "rebase on upstream" | Follow the new-sync path below. |
| **landing** | "land PR #N", "land the sync PR", "check for /land", "force-push the approved sync PR", "finalise PR #N" | Follow the landing path below. |
| **status** | "sync status", "what's pending", "any open sync PRs" | Load `references/inspect-and-prepare.md`, run `inspect_state.py`, summarise state. Do not mutate. |

### new-sync path (default: batch + yolo + auto-resolve)

The default flow for any `sync N` (including N=1) is **batch + auto-land + auto-resolve**: each iteration of the loop opens one PR with up to `BATCH_SIZE` upstream commits + Adobe commits on top, force-updates `adobe` immediately when both gates (clean rebase + tests pass) succeed, attempts auto-resolve on superficial conflicts, and continues looping until the target is hit.

1. **Parse target count.**
   - Explicit number ("sync 5", "port 3") → `TARGET_COUNT = N`.
   - "sync up to date", "sync all", "sync to upstream/main", or no number at all → `TARGET_COUNT = "all"` (resolved to `unsynced_count` after step 5's first inspect).
   - Default `BATCH_SIZE = 50`. If the user passed `--batch-size N` (or "in batches of N", "50 at a time"), override.
   - Initialise `landed_count = 0`, `pr_log = []`, `halts = []`.

2. **Tell the user** the resolved plan in one line: e.g. `"Syncing up to N commits in batches of 50 with auto-land + auto-resolve."` or `"Syncing all unsynced commits in batches of 50."`.

3. **Load `references/preconditions.md` section 0** — run `inspect_state.py "$REPO" --fix-remotes` when `upstream`/`public` need repair. Stop on non-fixable `origin` errors.

4. **Load and follow** the rest of `references/preconditions.md` — verify `gh` auth, Adobe fork reachability, upstream remote, token hygiene. Stop on any failure. (Run **once per top-level invocation**, not per loop iteration.)

5. **Inspect and classify (per loop iteration).** Load and follow `references/inspect-and-prepare.md` — run `inspect_state.py --count <BATCH_SIZE>`, parse the JSON, handle the `adobe` switch and fast-forward (including landing-artifact auto-recovery), then run `classify_batch.py` for informational classification (no gate).

   On the **first iteration** when `TARGET_COUNT == "all"`, set `TARGET_COUNT = unsynced_count` for the rest of the run (so the loop has a stable bound and the per-iteration dispatcher message is meaningful).

6. **Load and follow** `references/batch-and-yolo.md` — pick branch, rebase the batch, bisect on conflict, run tests (`run_tests.py --retry-once` where referenced), push, compose PR via `compose_pr_body.py` (`references/push-and-open-pr.md` §3), open PR, **auto-land** with `land_pr.py` when both gates pass, then **step 10** (checkout **`adobe`**, delete landed **`sync/*`** — remote delete best effort). This reference may load `references/auto-resolve-conflict.md` mid-flow when bisection finds a partial-clean prefix.

   Map terminal outcomes using **`references/batch-and-yolo.md` § Dispatcher outcomes** (and the summary table at the bottom of `auto-resolve-conflict.md` for auto-resolve-specific rows). Typical branches:

   - **Happy paths** (`landed_count` increases, loop continues) — full batch land; prefix+boundary auto-resolve land.
   - **Manual halts** (`halts[]`, stop loop) — protected-path / semantic conflicts during auto-resolve; `clean_count == 0`; bisection anomaly.
   - **`/land` required** (`halts[]` + open `[TESTS FAILING]` PR if applicable) — gate 2 failed after `--retry-once`; maintainer reviews then **`land PR #N`** per `poll-and-land.md`.

7. **Loop guard.** If `landed_count >= TARGET_COUNT` or `unsynced_count == 0` (re-checked at next iteration's step 5) or any "stop loop" outcome above fired → exit loop.

8. **Final summary** (always, even on halt). Print to the user:
   - `Landed <landed_count> of <TARGET_COUNT> commits across <len(pr_log)> PR(s).`
   - For each entry in `pr_log`: PR number, type (`batch` / `auto-resolve`), commit count, link.
   - For each entry in `halts`: reason, conflicting commit if applicable, PR link, what to do next.
   - Suggest next action: re-invoke after triaging halts, or "done — fork is up to date" when nothing remains.

### landing path

The user invoked landing directly — a PR is already open and they want it landed now (typically after a maintainer commented `/land`). Batch+yolo happy paths auto-land inline and skip this entry.

1. **Load and follow** `references/preconditions.md` — including section 0 (`inspect_state.py "$REPO" --fix-remotes`) when the clone may be fresh or shared, then the usual `gh` auth checks.
2. **Load `references/poll-and-land.md`** section 2 — one-shot `/land` scan + freshness + `land_pr.py`, then **§2.8** (checkout **`adobe`**, delete **`sync/*`** head ref when applicable). If no authorised `/land` exists yet, tell the user and stop.

### status path

1. **Load and follow** `references/inspect-and-prepare.md` for the inspection sequence. Pass `--count 1` since you don't intend to act — you only want the unsynced count and the oldest commit metadata.
2. Additionally list open Adobe-Apis PRs with `head.ref` matching `sync/*`:
   ```bash
   env -u GITHUB_TOKEN -u GH_TOKEN gh pr list --repo Adobe-Apis/agentgateway --state open --search "head:sync/" --json number,title,headRefName,createdAt
   ```
3. Report: unsynced count, oldest commit, any open sync PRs with their age. Do not mutate.

## Flow discipline — never deviate silently

The scripts and reference documents are the authority. Do **not** improvise, shortcut, or reorder steps. If you find yourself considering a deviation — for any reason — **stop and ask the user first**. Common temptations that are explicitly forbidden without user approval:

- Manually resolving a batch-rebase conflict instead of aborting → bisecting → auto-resolving.
- Skipping `verify_branch_shape.py` because the rebase "looks right".
- Treating the risk classifier as a gate (it is **informational only** — `batch-and-yolo.md` §2).
- Halting auto-land because of a `critical` classifier result when both rebase and tests passed.
- Running ad-hoc `git cherry-pick` or `git rebase --onto` instead of the documented commands.
- Composing or editing PR bodies by hand instead of using `compose_pr_body.py`.

If the skill flow produces an unexpected result or a script fails, surface the error to the user and wait for guidance. Do **not** paper over it with manual steps.

## Red flags — stop rather than push through

These apply across all paths. Any one of them means stop and surface to the user; never auto-recover.

- Baseline `make test` has any failures or new warnings.
- Semantic conflict during auto-resolve (see **`references/auto-resolve-conflict.md`** §5 classification rules).
- SSO error on `gh api repos/Adobe-Apis/*` (not recoverable without browser).
- Embedded token in `.git/config`, `~/.gitconfig`, or `git remote -v` output.
- Upstream commit subject has no `(#NNNN)` suffix (non-standard merge).
- Post-rebase test double-failure (after `run_tests.py --retry-once`) — open `[TESTS FAILING]` PR if applicable and hand off to `/land`; do not auto-land.
- Bisection result `clean_count == clean_count == batch_count` (script disagrees with itself) — surface attempts log, stop.
- `adobe..upstream/main` (or `origin/adobe..upstream/main` when local branch is missing) returns unexpectedly empty or much smaller than anticipated.
- Freshness check before force-update shows `adobe` has moved since the PR was created.

## Stopping condition

A full new-sync invocation runs the 8 steps above in order, looping through batches until done or halted. Each iteration lands up to `BATCH_SIZE` commits via batch+yolo and may additionally land 1 more via auto-resolve. The skill exits the loop on:

- `landed_count >= TARGET_COUNT` (target met)
- `unsynced_count == 0` (fork is up to date)
- User interrupt (Ctrl-C / dropped session — repo state is recoverable; next invocation restarts cleanly)
- Any "stop loop" outcome from the table in step 6 (protected-path conflict, semantic hunk, test failure, `clean_count == 0`)

After exiting the loop, step 8 (final summary) always runs — the user gets a single coherent report of what landed, what halted, and what to do next.

For multi-day burndowns: the user can re-invoke `sync up to date` after triaging halts, and the loop picks up from wherever `adobe` is now (the auto-recovery in `inspect-and-prepare.md` handles the case where a halted PR was landed manually in the meantime).
