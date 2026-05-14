# Conflict triage

Loaded only when a rebase pauses on a conflict. The goal is to understand *why* the conflict happened before deciding how to resolve it.

## Two classes of conflict

- **Superficial** — whitespace, import order, trailing newlines, comment rewrites, identical logic applied on both sides, function renames where Adobe's call site still works with the new name. The skill can propose concrete resolutions for these.
- **Semantic** — real logic overlap where Adobe's patch and the upstream change touch the same behaviour differently. The skill must NOT auto-resolve these — surface both diffs and let the user decide.

## 1. Identify conflicting files

```bash
git -C "$REPO" status --porcelain
```

Look for `UU`, `AA`, or `DD` prefixes. Those are the conflicting paths.

## 2. Gather three views per file

Prefer `mcp__git__*` tools if the git MCP server is configured in `.mcp.json` — they're schema-validated and don't trip the bash parser. Fall back to `git -C "$REPO" show` via Bash if MCP isn't available.

For each conflicting file:

1. **Read the file with conflict markers** (Read tool, absolute path). The `<<<<<<<`, `=======`, `>>>>>>>` blocks show both sides.
2. **Adobe commit's version** — use `mcp__git__git_show` with the SHA of the Adobe commit currently being reapplied. That SHA is in `.git/rebase-merge/stopped-sha`, or can be printed with `git -C "$REPO" rebase --show-current-patch | head -1`.
3. **Upstream commit's version** — use `mcp__git__git_show` with `<oldest_sha>` (the one the rebase is replaying onto).

## 3. Classify and surface

For each file, write a one-line classification in your reply:

```
src/foo.rs — SUPERFICIAL (import reorder); upstream added `use bar::Baz`, Adobe added `use qux::Quux` below it
src/bar.rs — SEMANTIC (both edit `handle_request`); upstream adds retry logic, Adobe adds telemetry around the same span
```

## 4. Resolve

### Superficial conflicts

Propose a concrete resolution in your reply (e.g. "take both imports, upstream order on top, Adobe order on bottom"), ask the user to confirm, then:

1. Use the **Edit** tool to rewrite the file — remove the conflict markers and leave the agreed merged content.
2. Stage the file with a separate Bash call per path:
   ```bash
   git -C "$REPO" add src/foo.rs
   ```

### Semantic conflicts

**STOP after classification.** Do NOT propose an edit. Show the user both `git show` outputs side by side and ask them to say which side wins, or to rewrite manually. After they resolve, they will tell you to continue.

## 5. Continue the rebase

Once all conflicts are staged:

```bash
git -C "$REPO" rebase --continue
```

If another Adobe commit produces a conflict on the next iteration, return to step 1 of this file and loop.

## Aborting

The user can change their mind at any point. A clean abort:

```bash
git -C "$REPO" rebase --abort
```

If the sync branch was already pushed in a previous attempt and you have to rebase again after resolving, the next push needs `--force-with-lease` (see `push-and-open-pr.md`). Never use a bare `--force`.

## Return to the dispatcher

Rebase is complete. Go back to step 4 of `rebase-and-test.md` (post-rebase `make test`).
