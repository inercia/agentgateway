# Adobe Docker builds

## Layout

| File | Role |
|------|------|
| [`Dockerfile`](../Dockerfile) | **Upstream verbatim** — no Adobe-only layers; should `git diff public/main -- Dockerfile` be empty after sync. |
| [`adobe/Dockerfile`](Dockerfile) | Adobe **snapshot/release** image: same as root + prebuilt **sccache** and `/sccache` BuildKit cache mount on `cargo build`. |
| [`.dockerignore`](../.dockerignore) | Upstream block + commented **Adobe** block (extra paths to shrink build context). |
| [`adobe/Makefile`](Makefile) | `make snapshot` / `make release` → `docker buildx build -f <repo>/adobe/Dockerfile <repo>` (paths resolved from Makefile location) |

Upstream CI and `make image` (root `Makefile`) use the root `Dockerfile` unchanged.

## `make snapshot` / `make release`

From `adobe/`:

- **`make snapshot`** — `PROFILE=quick-release` (default), `CARGO_FEATURES=agentgateway/ui,agentgateway/adobe`, musl builder, buildx local cache, **sccache** via `adobe/Dockerfile`.
- **`make release`** — `PROFILE=release` (LTO).
- Override: `PROFILE=release make snapshot`, `VERSION=dev-local make snapshot`.

Build context is always the **repo root** (`REPO_ROOT` in `adobe/Makefile`, whether you run `make -C adobe` or `cd adobe && make`).

## Syncing from upstream

After rebasing onto `public/main` (or `upstream/main`):

1. **Root Dockerfile** — replace with upstream copy:
   ```bash
   git show public/main:Dockerfile > Dockerfile
   ```
2. **`adobe/Dockerfile`** — refresh from the new root file, then re-apply the marked **Adobe sccache** block (between `WORKDIR /app` and `COPY Makefile`):
   - `ARG SCCACHE_VERSION` + `RUN` heredoc (prebuilt musl binary + sha256 pins)
   - `ENV RUSTC_WRAPPER` / `ENV SCCACHE_DIR`
   - add `--mount=type=cache,target=/sccache` on the `cargo build` `RUN`
3. **`.dockerignore`** — keep the upstream lines at the top; merge any upstream edits into that section; preserve the **Adobe** block below the comment unless upstream added equivalent entries.

Verify:

```bash
git diff public/main -- Dockerfile    # expect no diff
git diff public/main -- .dockerignore # expect only Adobe block + comments
```
