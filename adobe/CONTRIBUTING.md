# Contributing to Adobe agentgateway

This is the contribution guide for **Adobe-Apis/agentgateway**, Adobe's
fork of [`agentgateway/agentgateway`](https://github.com/agentgateway/agentgateway),
maintained by the Ethos Gateway team.

For changes that should reach the public project, see upstream's
[`CONTRIBUTION.md`](../CONTRIBUTION.md) and open your PR against
`agentgateway/agentgateway`. It will reach this fork on the next sync.

## Pick the right home for your change

Ask first: **does this belong upstream?**

| Question | Answer |
|---|---|
| Generally useful to all agentgateway users? | Send **upstream**. It will reach `adobe` via the next sync. |
| Adobe-specific (IMS, internal infra, Envoy session conventions, GW signing, internal registry paths, etc.)? | Land it **here**, on `adobe`. |
| Unsure? | Default to upstream — easier to revert and re-apply later than to maintain a private patch indefinitely. |

The Ethos Gateway team owns merges to `adobe`. Upstream PRs follow the
public project's review process.

## Branching model

- **`adobe`** is the integration branch. All Adobe-bound PRs target it.
- Feature work lives on short branches off `adobe`. Name them how you
  like — `sync/*` is reserved for the upstream-sync automation; avoid
  that prefix for normal feature work.
- The fork stays linear: upstream commits sit at the base with their
  verbatim SHAs, Adobe commits stack on top. See
  [`../.claude/skills/agentgateway-sync/references/merge-strategies.md`](../.claude/skills/agentgateway-sync/references/merge-strategies.md)
  for the rationale.
- Keep PRs small and focused. If a change is large enough to want
  multiple commits, prefer `Rebase and merge` or `Create a merge commit`
  over `Squash and merge` so per-commit attribution survives.

## Code conventions

See [`CONVENTIONS.md`](CONVENTIONS.md) for the full set. Essentials:

- **Gate Adobe-only logic** with `#[cfg(feature = "adobe")]`. Tests
  covering Adobe-only behavior go behind the same flag.
- **Protected files** — touch with care, run the `check-patches` skill
  after any non-trivial change:
  - `crates/agentgateway/src/http/jwt.rs` (IMS-aware JWT validation)
  - `crates/agentgateway/src/mcp/sse.rs` (`mcp-session-id` header)
  - everything under `adobe/`
- **Prefer small modules** under `adobe/` for tooling that should never
  compile into upstream.
- Adobe container builds set `CARGO_BUILD_FEATURES=ui,adobe`. Validate
  locally with `cargo test --features ui,adobe`.

## Commit messages

Every Adobe-authored commit carries an `adobe:` prefix so the fork's
contribution is visually distinct from cherry-picked upstream history.

Pattern: `adobe: [<sub-area>:] <subject>`

Examples:

- `adobe: jwt: validate expires_in/created_at`
- `adobe: mcp/sse: add mcp-session-id response header`
- `adobe: makefile: add amd64 release target`
- `adobe: release X.Y.Z`

Lowercase. No trailing period in the subject. Reference Adobe-Apis PR
numbers as `(#N)` when relevant. Wrap bodies at 72–80 characters.

## Build and test locally

```bash
cargo build --features ui,adobe
make test
make -C adobe snapshot   # local docker image, defaults to amd64
```

`make -C adobe snapshot` (and `release`) need `git describe --match 'v*'`
to resolve to an upstream tag. If you're working from a sparse fetch,
run `make -C adobe fetch-tags` first.

## Releases

Releases are author-driven and CI-automated. The PR author cuts the
release; CI tags it on merge.

1. **When your PR is the one that ships a release**, bump
   `adobe/version` to the new `X.Y.Z` and append a `## X.Y.Z` section to
   `adobe/CHANGELOG.md` with the relevant bullets. Use the
   `changelog-entry` skill to draft the bullets.
2. **Don't tag manually.** When the PR merges into `adobe`, CI tags the
   merge commit `adobe-X.Y.Z` (annotated) and publishes a GitHub
   release. The Docker image is published as
   `…/agentgateway:vUPSTREAM-N-gSHA-adobe-X.Y.Z-amd64`.
3. **Non-release PRs leave `adobe/version` and `adobe/CHANGELOG.md`
   alone.** Mid-cycle PRs (bug fixes, refactors, infra) don't bump the
   version. Only the PR that's "the release" does.

### Invariants (CI enforces these)

At any commit on `adobe`:

- `cat adobe/version` equals the most recent `## X.Y.Z` heading in
  `adobe/CHANGELOG.md`.
- For every `## X.Y.Z` heading, there is a corresponding annotated git
  tag `adobe-X.Y.Z`.
- Each `adobe-X.Y.Z` tag points at a commit where `cat adobe/version`
  returns `X.Y.Z`.

### Tag scheme

- Adobe releases use `adobe-X.Y.Z` annotated tags.
- Upstream `v*` tags are preserved verbatim from `public/main`; never
  re-tag or rename them.
- `adobe/Makefile` derives `UPSTREAM_VERSION` from `git describe --match
  'v*'`, so adding Adobe tags never disturbs the upstream-version
  lookup.

## Pull request expectations

- PRs target `adobe`.
- CI runs the upstream test matrix plus Adobe-specific checks (including
  the release invariants above when `adobe/version` or
  `adobe/CHANGELOG.md` changes).
- Reviews require at least one approval from the Ethos Gateway team.
  Changes touching protected files (`jwt.rs`, `mcp/sse.rs`, `adobe/`)
  may need a second reviewer at the team's discretion.
- After approval, the PR author (or a maintainer) merges. The release
  pipeline runs automatically if the merge bumps `adobe/version`.

## Upstream sync (separate workflow)

You shouldn't need to run a sync as part of a regular feature PR. If
your work depends on a recent upstream commit, ping the Ethos Gateway
team and a maintainer will trigger the
[`agentgateway-sync`](../.claude/skills/agentgateway-sync/SKILL.md) skill.

The skill rebases Adobe-only commits onto each new upstream commit,
opens a `sync/*` PR, and waits for a `/land` comment from a maintainer
to force-update `adobe`. It is the only path by which `adobe` should
gain upstream content.

## Questions

Open a GitHub issue, or reach out internally to the Ethos Gateway team.
