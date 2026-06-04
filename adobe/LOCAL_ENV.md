# Local development environment

The repo ships a Rust devcontainer (`.devcontainer.json` →
`mcr.microsoft.com/devcontainers/rust:latest`). Install VS Code's
**Dev Containers** extension, open the repo, and *Reopen in Container*
— the workspace mounts at `/workspaces/agentgateway` and you run as
`root`.

The base image includes `rustup` and `git`. Node, `gh`, and the extra
git remotes need a one-time bootstrap from inside the container:

```bash
cd /workspaces/agentgateway

# 1. Install the pinned Rust toolchain (rust-toolchain.toml requests 1.95).
rustup show

# 2. Install sccache — .cargo/config.toml sets rustc-wrapper = "sccache"
#    unconditionally, so cargo build fails without it.
cargo install sccache

# 3. Persist CARGO_NET_GIT_FETCH_WITH_CLI — required by the git-based
#    crate patches in Cargo.toml.
echo 'export CARGO_NET_GIT_FETCH_WITH_CLI=true' >> ~/.bashrc

# 4. Cap cargo's parallel link jobs — the default (one per core) makes
#    `cargo test` peak-link enough memory to get `ld` OOM-killed inside
#    the devcontainer VM. `CARGO_BUILD_JOBS=2` is the floor that works.
echo 'export CARGO_BUILD_JOBS=2' >> ~/.bashrc
source ~/.bashrc

# 5. Install Node + npm (Debian trixie ships Node 20, enough for the UI).
sudo apt-get update && sudo apt-get install -y nodejs npm

# 6. Next steps are required only for fork-sync

# 6.1. Install gh (used by the agentgateway-sync skill for PR lookups).
sudo apt-get install -y gh

# 6.2. Add the upstream/public remotes used by the sync skill.
python3 .claude/skills/agentgateway-sync/scripts/inspect_state.py . --fix-remotes
```

After step 6.2, `git remote -v` lists three remotes: `origin`
(`Adobe-Apis/agentgateway`), and `upstream` + `public` (both pointing
at `agentgateway/agentgateway`). The bootstrap is idempotent — safe to
re-run on an existing container.

If you'd rather skip the devcontainer and build on the host, you need
Rust 1.95 (via `rustup`), Node 20+, and `gh`; the same six steps
apply, minus the `apt-get` lines. Step 4 is only needed when the host
is memory-constrained.

## Build and test locally

```bash
cargo build --features ui,adobe
make test
make -C adobe snapshot   # local docker image, defaults to amd64
```

`make -C adobe snapshot` (and `release`) need `git describe --match 'v*'`
to resolve to an upstream tag. If you're working from a sparse fetch,
run `make -C adobe fetch-tags` first.
