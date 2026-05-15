# Agent conventions (bash / `gh` / scripts)

Loaded once from `SKILL.md`. Keeps approval prompts predictable in Claude Code.

## Git invocation discipline

1. **Never** emit `cd <path> && git …`. Use `git -C "$REPO" …`.
2. **One git porcelain/plumbing operation per Bash call** where practical — no `&&` chaining of git steps the dispatcher could split.
3. Subprocess Python helpers (`inspect_state.py`, `find_clean_prefix.py`, etc.) may run multiple git calls internally — that is intentional (single approval).

## `gh` CLI environment prefix

Every `gh` invocation from Bash **must** use:

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh …
```

so a stale PAT in the shell does not override `hosts.yml`.

## MCP vs `gh`

- Prefer **`cloud-github` MCP** for one-off reads (PR metadata, comments, reviews) where structured JSON helps.
- Prefer **`gh`** for writes (`pr create`, `pr close`, `api` PATCH) and bulk branch queries — fewer round trips.

Both share the same SSO-authorised credential once `GITHUB_TOKEN`/`GH_TOKEN` are stripped from that subprocess env.

## JSON scripts

Scripts that emit JSON (`inspect_state.py`, `pick_sync_branch.py`, `classify_batch.py`, `find_clean_prefix.py`, `land_pr.py`, …) write **stdout only** — parse inline in the tool result.

Exceptions: `run_tests.py` (`--log-dir` log files), `compose_pr_body.py` (`--out` markdown for `gh pr create --body-file`).

## `compose_pr_body.py` inputs

Prefer **`--merge`** (**`references/push-and-open-pr.md` §3**) — pass **`inspect_state`**,
**`classify_*`**, and **`run_tests --output`** JSON paths so **`batch_commits`** and
classification are joined in Python (not by hand). Fallback: **`--print-template`**
then **`--json-file`**. Wrong shapes or leaked placeholders fail validation or
**`render_errors`** (stderr JSON, exit **2**) before markdown is written.
