# About

This directory contains details about Adobe's fork of agentgateway, maintained by the Ethos Gateway team.

We maintain a set of patches on top of upstream agentgateway, to bypass different limitations specific to Adobe that the open source project cannot implement.

# Versioning

We'll suffix the oficial agentgateway version with the adobe's version
```
agentgateway:v0.12.0-722-g31bd70fe-dirty-adobe-1.0.0-amd64
```

# Log

## 1.0.0

- Validate expires_in/created_at for tokens.
- Add mcp-session-id response header for SSE, to allow Envoy to create stateful session envelopes.

## 1.0.3

- Vendor the upstream `claude-skill-upstream-sync` toolchain into `.claude/skills/agentgateway-sync` (dispatcher `SKILL.md`, expanded reference playbooks, and first-party `scripts/`).
- Add full `inspect_state.py` parity: `upstream/main` unsynced count vs `adobe` / `origin/adobe`, strict `upstream` remote URL check with actionable add/set-url errors, `gh api` lookup of upstream PR `head.ref`, `GITHUB_TOKEN`/`GH_TOKEN` stripping, optional `--output`.
- Ship complete `pick_sync_branch.py` and `run_tests.py` from upstream; keep `make test` logs under `$TMP_DIR` with JSON summaries on stdout.
- Replace `classify_commit.py` with Adobe labels `MERGE_SAFE`, `NEEDS_REVIEW`, `SECURITY`, and `SKIP`; explicit risk ranks; protected paths `jwt.rs`, `mcp/sse.rs`, and `adobe/`; guarded content patterns; removed the old `upstream-only` label.
- Port v2 reference material (`preconditions`, `conflict-triage`, `mcp-vs-gh`, merge/poll/rebase/push guides) with integration branch `adobe`, `$SKILL_DIR/scripts/...` invocations, `$REPO` defaulting to `$PWD`, `$TMP_DIR=$REPO/.git/sync-tmp`, and `$REPORTS_DIR=$REPO/adobe/sync-reports`.
- Extend `inspect-and-prepare`: after `inspect_state`, run `classify_commit.py` on `oldest_sha` — halt on critical, require explicit chat acknowledgement on high risk.
- Extend `rebase-and-test` with step 3.5 `check-patches` smoke greps before post-rebase tests.
- Extend `push-and-open-pr` with a classification table in the PR body, optional `changelog-entry` text, and `adobe_at_creation` HTML metadata for landing freshness.
- Extend `poll-and-land` with `ScheduleWakeup` at 1500s, `/land` scanning, `gh api` PATCH of `refs/heads/adobe`, freshness checks against `adobe_at_creation`, and optional Jira/wiki updates when MCP is present.
- Update `.claude/settings.json` allowlists: `mcp__cloud-github__*` as its own entry (not under `Bash(`), plus scoped `python3` and `grep` rules for the skill scripts and Adobe patch greps.
- Mark skill Python entrypoints executable for local runs.
