# Merge strategy decision record

When porting one upstream commit onto `adobe`, four branch shapes are possible. All four produce the same tree content on `adobe` after the PR merges; they differ in what the branch's history looks like and how the PR review experience feels. The team has tried all four and settled on the fourth.

## Shape A — Rebase Adobe commits onto the upstream commit; PR against `adobe`

```
<adobe-1'>  <adobe-2'>  <adobe-3'>  <adobe-4'>   ← rebased, new SHAs
                                          |
                                  <upstream-sha>
                                          |
                             <common-ancestor>
```

- **History**: linear, clean, upstream SHA preserved.
- **PR diff (3-dot vs adobe)**: shows ~7 files — the 2 upstream workflow files plus all the Adobe contributions under *new* SHAs, because the merge base between the branch and adobe is the pre-divergence point. The Adobe work already exists on adobe under old SHAs, so reviewers see "new" changes that are actually already present.
- **Why rejected**: the review experience is bad. Even though the end state is identical to Shape D, reviewers keep asking "why are the Adobe commits in this PR?" and the answer requires a paragraph of git-plumbing exposition.

## Shape B — Cherry-pick the upstream commit onto `adobe`; PR against `adobe`

```
<cherry-pick of upstream-sha>   ← new SHA, same content as upstream
                              |
                       3bf4c55c  (adobe HEAD)
                              |
                       ... adobe history with Adobe commits ...
```

- **History**: linear, one new commit on top of adobe, minimal PR diff (2 files).
- **Problem**: the upstream commit's SHA is lost. Future sync tooling can't use `git merge-base --is-ancestor <upstream-sha> adobe` to check whether upstream commit X has been integrated, because X's SHA isn't reachable. Every sync has to re-derive the integration point from tree state.
- **Why rejected**: traceability matters more than a pretty PR diff. Losing the upstream SHA makes the sync history opaque.

## Shape C — Merge commit: adobe merged with upstream commit

```
    M (merge, 2 parents)
   / \
  |   <upstream-sha>   ← verbatim
  |   |
  3bf4c55c  <common-ancestor>
  (adobe HEAD)
```

- **History**: preserves the upstream SHA as a reachable parent of a merge commit.
- **Problem**: not linear. The Adobe commits are on the parent-1 side and the upstream commit is on the parent-2 side; "upstream commit on top of the common ancestor, Adobe commits on top of that" is *not* the shape you get. The team explicitly described wanting a linear shape with the Adobe contributions stacked on top of the upstream commit.
- **Why rejected**: doesn't match the team's mental model of "add the new upstream commit, then put our work on top".

## Shape D — Linear: upstream commit preserved at base, Adobe commits cherry-picked on top (CHOSEN)

```
<adobe-4'>                       ← cherry-picked
<adobe-3'>
<adobe-2'>
<adobe-1'>
<upstream-sha>                   ← VERBATIM (same SHA, author, message, parent)
<common-ancestor>
```

This is structurally identical to Shape A, but we're explicit that:

- The **branch** has this shape.
- The **PR against `adobe`** will show more than 2 files in the diff (same reason as Shape A).
- The PR body explains that fact so reviewers aren't confused.
- The recommended merge button is **"Create a merge commit"** (default GitHub option). "Rebase and merge" will conflict because `adobe` already has the Adobe content under old SHAs, and replaying them would try to add duplicate content.

**Why this shape is correct:**

- Upstream commit's SHA is reachable via `git merge-base --is-ancestor <upstream-sha> adobe` after the merge commit lands, giving us a durable integration marker.
- Adobe commits appear in the "commits" list of the PR in chronological order on top of the upstream commit — matches the team's requested structure.
- The reviewer can focus on the 2 upstream files in the diff; the additional files are explicitly called out in the PR body as "already on adobe, shown here because 3-dot diff".

**Observed consequence for the user**: `git diff --stat origin/adobe...HEAD` shows 7 files and `git diff --stat origin/adobe..HEAD` (two dots, tree-vs-tree) shows the real delta of 2 files. GitHub displays the 3-dot view. The PR description needs to pre-empt the confusion.

## Checklist for picking the right shape

Always **Shape D** unless the user explicitly asks for one of the alternatives. Red flags that would push you to a different shape:

- User says "I don't care about preserving upstream SHA, just give me a clean diff" → Shape B (cherry-pick).
- User explicitly wants a merge commit in adobe's history (e.g., to match another workflow) → Shape C.
- Never use Shape A (it's Shape D done poorly — same history, same diff, but without the PR-body explanation).
