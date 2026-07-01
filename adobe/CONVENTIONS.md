# Adobe conventions (`feature = "adobe"`)

## Goals

- Keep the fork close to **`agentgateway/agentgateway`** while isolating Adobe-only behavior.
- Make Adobe deltas **easy to spot in review** and **safe to rebase**.

## Rules

1. **Gate Adobe-only logic** with `#[cfg(feature = "adobe")]` (and tests behind the same flag when they cover Adobe-only behavior).
2. **Prefer small modules** under `adobe/` for policy/docs Makefiles that are not meant to compile into upstream.
3. **Do not revert** Ethos JWT/SSE patches without explicit security review:
   - `crates/agentgateway/src/http/jwt.rs`
   - `crates/agentgateway/src/mcp/sse.rs`
4. **Automation alignment:** `classify_commit.py` treats the following as **protected**:
   - Paths containing **`/jwt.rs`**
   - Paths containing **`/sse.rs`**
   - Paths under **`/adobe/`**

When touching protected files, run **`check-patches`** after rebases/merges.

## Upstream sync automation

Before running the **`agentgateway-sync`** skill (`inspect_state.py`, rebase, PR), run **`inspect_state.py <repo> --fix-remotes`** once per machine or fresh clone so `upstream` and optional `public` match the canonical layout (`origin` must already point at Adobe-Apis). See `.claude/skills/agentgateway-sync/references/preconditions.md` section 0.

## Build / packaging

Adobe **snapshot/release** images are built from [`adobe/Dockerfile`](Dockerfile) (see [`DOCKER.md`](DOCKER.md)); the repo-root [`Dockerfile`](../Dockerfile) stays **identical to upstream** to reduce sync conflicts. Adobe-only Docker deltas (sccache, extra `.dockerignore` entries) live under `adobe/` or the commented Adobe section of `.dockerignore`.

Adobe container builds set the `CARGO_FEATURES` Docker build-arg (default **`agentgateway/ui,agentgateway/adobe`** in `adobe/Makefile`) so the `cargo build` step receives the package-qualified feature list. The build-arg name MUST match the Dockerfile's `ARG CARGO_FEATURES` declaration; if it does not, Docker silently drops it and the resulting binary is built without `adobe`, stripping every `#[cfg(feature = "adobe")]` block at compile time. Local testing should include `--features agentgateway/adobe` (or `cargo build -p agentgateway --features adobe`) when validating integration branches.

**CEL / JSON schema (`schema/cel.json`, generated docs):** regenerate with `make generate-schema` (xtask enables `schema` + `adobe` so native `mcpGuardrails` processor types and the dynamic `mcpGuardrails.*` map appear in `schema/cel.json`). Rate-limit quota/status is exposed at `mcpGuardrails.rateLimit.*` (not a separate `guardrail` namespace). MCPInfo includes `mcp.task` unconditionally for Ethos policy parity.

**Snapshot vs release:** `make snapshot` (from `adobe/`) builds with `PROFILE=quick-release`, sccache, and persistent buildx cache for faster iterative images; `make release` uses `PROFILE=release` (LTO). Override with `PROFILE=release make snapshot` when you need a prod-like snapshot build.
