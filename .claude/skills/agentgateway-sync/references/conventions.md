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

## Script I/O cheatsheet (don't improvise args)

Read the exact signature before calling — improvising flags wasted multiple turns in past runs. Authoritative columns: **output** = where results land; **required** = must-pass args; **key fields** = what to parse.

| Script | Output | Required args | Notes / key fields |
|---|---|---|---|
| `inspect_state.py` | **stdout** | `<repo>` | `--count N` for a batch; `--fix-remotes`; **no `--output`**. Fields: `unsynced_count`, `batch_count`, `batch_commits[]`, `batch_end_sha`, `oldest_sha`. |
| `pick_sync_branch.py` | **stdout** | `<repo>` | `--batch --count N --end-short-sha X`. Field: `sync_branch`. |
| `classify_batch.py` | **stdout** | `<repo>` + SHAs **or** `--shas-file <path>` | Prefer `--shas-file` (see SHA-list rule). Fields: `aggregate.worst_risk`, `aggregate.label_counts`, `commits[].risk` (**not** `risk_level`). |
| `find_clean_prefix.py` | **stdout** | `<repo> --count N` | Optional `--base-ref`, `--upstream-ref`. **No `--output`** — redirect stdout. Fields: `clean_count`, `clean_end_sha`, `conflicting_sha`, `conflicting_subject`, `conflicting_pr_num`. |
| `verify_branch_shape.py` | **stdout** | `<repo> --expected-shas <comma-joined>` | SHAs are **comma-separated**, not space. Optional `--head`, `--expected-count`. Field: `ok`, `missing_shas`, `count_match`. |
| `run_tests.py` | **`--output` file** + `--log-dir` | `<repo> <label>` | Prefix with `PATH="$HOME/.cargo/bin:$PATH"`. `--retry-once`, `--cache-file`, `--cache-key`. Fields: `all_passed`, `passed`, `failed`, `warnings`, `cached`. |
| `compose_pr_body.py` | **`--out` file** | `--kind {single,batch,auto-resolve}` (mandatory with `--merge`) | `--merge` joins `--inspect-state` + `--classify-batch` + test JSONs; classify must cover **every** commit in the inspect file (see prefix-land rule). |
| `land_pr.py` | **stdout** | `<repo> --pr N --head-sha X --expected-adobe Y` | `--reason {auto-batch,auto-resolve,manual-land}`. Field: `landed`, `landed_confirmed_by_ancestry`. |

## SHA-list passing (never shell-expand)

Passing a multi-SHA list through a shell `$VAR` to a script (e.g. `classify_batch.py "$REPO" $SHAS`) **silently truncates** under zsh — past runs classified only the first SHA and corrupted the PR body. Always pass SHA lists via a file:

```bash
python3 -c "import json; d=json.load(open('$TMP_DIR/inspect.json')); open('$TMP_DIR/batch_shas.txt','w').write('\n'.join(c['sha'] for c in d['batch_commits']))"
python3 "$SKILL_DIR/scripts/classify_batch.py" "$REPO" --shas-file "$TMP_DIR/batch_shas.txt"
```

If a script does not yet support `--shas-file`, drive it through a Python `subprocess.run([...] + shas)` call instead of shell word-splitting.

## Artifact staleness guard

`$TMP_DIR/*.json` files persist across runs. A **failed** producer (wrong args, exception) leaves the **previous** run's file on disk — reading it then silently acts on stale data (a past bisect read `clean_count` from an unrelated earlier batch). Two rules:

1. `rm -f` the target artifact immediately **before** re-running its producer, so a failed run cannot leave a readable stale file.
2. Never read an artifact whose producer did not exit 0 in the **same** turn. If the producer printed a usage/error to stderr, fix the call and re-run — do not fall back to the file already on disk.

## Committing skill changes

The `.claude/` directory is **gitignored** in this repo. To commit changes to skill files (scripts / references), stage with `-f`:

```bash
git -C "$REPO" add -f .claude/skills/agentgateway-sync/<path>
```

## `compose_pr_body.py` inputs

Prefer **`--merge`** (**`references/push-and-open-pr.md` §3**) — pass **`inspect_state`**,
**`classify_*`**, and **`run_tests --output`** JSON paths so **`batch_commits`** and
classification are joined in Python (not by hand). Fallback: **`--print-template`**
then **`--json-file`**. Wrong shapes or leaked placeholders fail validation or
**`render_errors`** (stderr JSON, exit **2**) before markdown is written.
