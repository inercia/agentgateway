# Syncing Adobe agentgateway with upstream

This document explains how Adobe's fork (`Adobe-Apis/agentgateway`) is
kept in sync with upstream `agentgateway/agentgateway`. The day-to-day
mechanics are automated by the
[`agentgateway-sync`](../.claude/skills/agentgateway-sync/SKILL.md)
Claude skill; this page is the operator-facing summary — what it does,
when to invoke it, and how to read the result.

## Why we sync this way

Adobe maintains a small, durable set of patches on top of upstream
(JWT/IMS, MCP SSE session header, Adobe-internal build tooling). To
keep merge cost low and review trivial, we follow two rules:

1. **One upstream commit per sync.** We do not bulk-merge upstream
   `main`. Each upstream commit lands in its own sync PR. That keeps
   blast radius small, classification meaningful, and any future
   bisect on `adobe` interpretable.
2. **Linear branch shape.** The upstream commit is preserved
   **verbatim** (same SHA, author, message, parent), and the Adobe
   patches are rebased on top of it. See
   [`../.claude/skills/agentgateway-sync/references/merge-strategies.md`](../.claude/skills/agentgateway-sync/references/merge-strategies.md)
   for the four shapes we considered and why "Shape D" (linear) wins.

A consequence of rule 2: `git merge-base --is-ancestor <upstream-sha>
adobe` is the durable check for "is upstream commit X integrated?".

## What a sync looks like

```
<adobe-patch-N>'    ← Adobe commits, rebased onto the new upstream commit
<adobe-patch-…>'      (new SHAs — rebase rewrites identity when the
<adobe-patch-1>'       parent changes; this is fine and expected)
<upstream-sha>       ← VERBATIM (same SHA as agentgateway/agentgateway:main)
<prior adobe tip>
```

The sync PR is opened from a `sync/<source-branch>` branch (where
`<source-branch>` is the upstream PR's `head.ref`, normalized) against
`adobe`. GitHub's 3-dot diff for the PR will show the upstream commit
**plus** the rebased Adobe commits (because their SHAs are new) — the
PR body explains this so reviewers focus on the upstream change.

## Who triggers a sync

- **The Ethos Gateway team.** Anyone on the team can invoke the skill;
  the only authorisation gate is at landing time (see "Landing" below).
- **Cadence is on-demand**, typically:
  - Weekly catch-up runs.
  - One-off "I need upstream PR #1252 on `adobe` before my feature
    work" requests.
  - After upstream releases (`v*` tag bumps) when we want a clean
    baseline.

Non-maintainer contributors do **not** trigger syncs directly. If your
feature work depends on an upstream commit that hasn't reached `adobe`
yet, ask the team to run a sync; do not cherry-pick the upstream commit
into your feature branch.

## How a sync runs (skill workflow, abridged)

The full workflow is in
[`../.claude/skills/agentgateway-sync/SKILL.md`](../.claude/skills/agentgateway-sync/SKILL.md);
the operator-relevant steps are:

1. **Inspect state.** The skill checks remotes, working tree, auth,
   token hygiene, and computes the count of unsynced upstream commits
   plus the oldest one. It refuses to proceed if anything is off.
2. **Classify the upcoming batch (informational only by default).**
   [`classify_batch.py`](../.claude/skills/agentgateway-sync/scripts/classify_batch.py)
   labels upstream commits for PR-body banners; **`batch-and-yolo`** does not gate on these labels — clean rebase + tests are the real gates.
3. **Baseline `make test`.** Captures pre-sync test counts to detect
   post-rebase regressions.
4. **Rebase Adobe commits onto the upstream commit.** Conflicts during
   batch sync use **`batch-and-yolo.md`** bisection +
   **`auto-resolve-conflict.md`** for superficial merges; protected-path /
   semantic collisions halt for human review.
5. **Re-run `make test`.** Any new failure or warning vs. baseline
   halts.
6. **Push, open PR.** The PR body includes the upstream commit link,
   classification JSON, baseline-vs-post test deltas, optional
   draft CHANGELOG bullets, and a `adobe_at_creation: <sha>` freshness
   marker.
7. **Wait for `/land`.** Maintainer comments `/land`, then runs **`land PR #N`**
   again so `poll-and-land.md` section 2 executes (stateless — no background
   polling). Auto-land handles the happy batch path without `/land`.

## Landing (`/land`)

Sync PRs do not use GitHub's merge buttons. Native options either
conflict (`Rebase and merge`), produce duplicate history (`Create a
merge commit`), or collapse attribution (`Squash and merge`) — all
three are wrong for this workflow. The skill enforces a different gate:

- A **maintainer** (anyone with `write`, `maintain`, or `admin`
  permission on `Adobe-Apis/agentgateway`) comments `/land` on the PR.
- Someone (same session or later) invokes **`land PR #N`** so
  `poll-and-land.md` rechecks freshness (`adobe_at_creation` vs live
  `refs/heads/adobe`), verifies the `/land` authoriser, then runs
  **`scripts/land_pr.py`** (REST force-update + PR hygiene).
- GitHub typically auto-flips the PR to `merged` (since the head is
  now reachable from `adobe`). If not, `land_pr.py` closes it when safe.

There is deliberately **no GitHub "Approve" gate** on sync PRs. `/land`
from an authorised maintainer is the sole authorisation. Do not re-add
an approval requirement — it breaks the single-step landing flow.

## Conflict resolution

Use **`auto-resolve-conflict.md`** inside the skill directory for automated
superficial resolutions, plus **`batch-and-yolo.md`** for bisection metadata.

The playbook distinguishes:

- **Superficial conflicts** — whitespace/import shuffles/disjoint edits that match the narrow rules table.
- **Semantic conflicts** — behaviour overlap. Automation halts; a human merges manually and re-invokes **`sync`**.

Protected paths get extra care:

- `crates/agentgateway/src/http/jwt.rs` (IMS-aware JWT)
- `crates/agentgateway/src/mcp/sse.rs` (`mcp-session-id` header)
- everything under `adobe/`

After any non-trivial conflict touching these files, run the
[`check-patches`](../.claude/skills/check-patches/SKILL.md) skill to
verify the Adobe semantics still hold.

## When a sync halts

Automation is designed to fail closed. The skill stops (rather than push
through) on any of:

- Baseline `make test` has any failures or new warnings.
- Semantic conflict during auto-resolve / manual-merge halts from `batch-and-yolo.md`.
- Post-rebase test counts show failures or new warnings vs. baseline after `run_tests.py --retry-once`.
- Upstream commit subject has no trailing `(#NNNN)` — possible
  non-squash merge upstream; needs human review.
- `adobe` advanced between PR creation and `/land` (freshness check
  fails).

A halted sync leaves the `sync/*` branch and PR intact so a human can
inspect, fix, and re-run.

## What syncing does NOT do

To avoid surprise interactions with the release pipeline:

- **Sync PRs do not bump `adobe/version` or `adobe/CHANGELOG.md`.**
  Releases are author-driven and CI-tagged on merge — see
  [`CONTRIBUTING.md`](CONTRIBUTING.md#releases). A sync PR landing on
  `adobe` is a regular merge as far as CI is concerned; if it doesn't
  change `adobe/version`, no tag is cut.
- **Sync PRs do not create `adobe-*` tags.** Only the release pipeline
  does. Upstream `v*` tags arrive via `make -C adobe fetch-tags` and
  are never re-tagged or renamed by the sync skill.
- **Syncing is one-way.** Changes flow upstream → `adobe`. If you've
  authored an Adobe patch that should be sent upstream too, open a
  separate PR against `agentgateway/agentgateway` — the sync flow is
  not the mechanism for that.

## Manual fallback

If the Claude skill is unavailable (no Claude session, MCP issues,
broken local Python), a maintainer can run the same flow by hand. The
shape is:

```bash
# Identify the next unsynced upstream commit.
git fetch upstream main
git log --oneline adobe..upstream/main | tail -1   # oldest unsynced

# Create the sync branch from upstream's commit.
git switch -c sync/<source-branch> <upstream-sha>

# Cherry-pick the Adobe patches stacked on adobe but not on
# upstream/main — same order as `git log <merge-base>..adobe`.
git cherry-pick <merge-base>..adobe

# Verify tree matches what a rebase would produce.
make test

# Push and open a PR against adobe, base = adobe.
git push -u origin sync/<source-branch>
gh pr create --repo Adobe-Apis/agentgateway --base adobe \
  --head sync/<source-branch> --title "Sync: <upstream subject>" ...
```

Use this only when the skill is down — the skill enforces several
checks (freshness, classification, test deltas, protected-path
verification) that manual runs invariably forget.

## See also

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — feature-PR workflow against
  `adobe`, release/changelog/version conventions.
- [`CONVENTIONS.md`](CONVENTIONS.md) — Adobe-only code rules
  (feature gating, protected paths).
- [`../.claude/skills/agentgateway-sync/SKILL.md`](../.claude/skills/agentgateway-sync/SKILL.md)
  — the canonical skill definition.
- [`../.claude/skills/agentgateway-sync/references/`](../.claude/skills/agentgateway-sync/references/)
  — per-phase playbooks (preconditions, inspect, rebase, conflict
  triage, push, poll-and-land, merge strategies).
- [`../.claude/skills/check-patches/SKILL.md`](../.claude/skills/check-patches/SKILL.md)
  — post-sync verification of protected files.
