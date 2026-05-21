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

Before running the **`agentgateway-sync`** skill (`inspect_state.py`, rebase, PR), run **`inspect_state.py <repo> --fix-remotes`** once per machine or fresh clone so `upstream` and optional `public` match the canonical layout (`origin` must already point at Adobe-Apis). See `.claude/skills/agentgateway-sync/references/preconditions.md` section 0.

## Build / packaging

Adobe container builds set the `CARGO_FEATURES` Docker build-arg (default **`agentgateway/ui,agentgateway/adobe`** in `adobe/Makefile`) so the `cargo build` step in [Dockerfile](../Dockerfile) actually receives the package-qualified feature list. The build-arg name MUST match the Dockerfile's `ARG CARGO_FEATURES` declaration; if it does not, Docker silently drops it and the resulting binary is built without `adobe`, stripping every `#[cfg(feature = "adobe")]` block at compile time. Local testing should include `--features agentgateway/adobe` (or `cargo build -p agentgateway --features adobe`) when validating integration branches.

**CEL / JSON schema (`schema/cel.json`, generated docs):** regenerate with the repo's xtask after rebases; MCPInfo includes `mcp.task` unconditionally for Ethos policy parity.
