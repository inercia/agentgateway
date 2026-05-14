# Adobe conventions (`feature = "adobe"`)

## Goals

- Keep the fork close to **`agentgateway/agentgateway`** while isolating Adobe-only behavior.
- Make Adobe deltas **easy to spot in review** and **safe to rebase**.

## Rules

1. **Gate Adobe-only logic** with `#[cfg(feature = "adobe")]` (and tests behind the same flag when they cover Adobe-only behavior).
2. **Prefer small modules** under `adobe/` for policy/docs Makefiles that are not meant to compile into upstream.
3. **Do not revert** Ethos JWT/SSE patches without explicit security review:
   - `crates/agentgateway/src/jwt.rs`
   - `crates/agentgateway/src/sse.rs`
4. **Automation alignment:** `classify_commit.py` treats the following as **protected**:
   - Paths containing **`/jwt.rs`**
   - Paths containing **`/sse.rs`**
   - Paths under **`/adobe/`**

When touching protected files, run **`check-patches`** after rebases/merges.

## Upstream sync automation

Before running the **`agentgateway-sync`** skill (`inspect_state.py`, rebase, PR), run **`ensure_git_remotes.py`** once per machine or fresh clone so `origin`, `upstream`, and `public` match the canonical layout. See `.claude/skills/agentgateway-sync/references/preconditions.md` section 0.

## Build / packaging

Adobe container builds set `CARGO_BUILD_FEATURES` (default **`ui,adobe`** in `adobe/Makefile`). Local testing should include `--features ui,adobe` when validating integration branches.
