# Adobe agentgateway — Claude Code guide

This repository is **Adobe-Apis/agentgateway**, a fork of **`agentgateway/agentgateway` (public)** with Adobe-only patches.

## Two-repo chain

1. **Upstream:** `agentgateway/agentgateway` — fetch via git remote `public`.
2. **Adobe fork:** `Adobe-Apis/agentgateway` — `origin`, integration branch **`adobe`**.

Work happens on short-lived **`sync/*`** branches and lands via PR to **`adobe`**.

## Skills

- **`agentgateway-sync`** — end-to-end public -> Adobe sync workflow (inspect/classify/rebase/test/PR/land).
- **`check-patches`** — verify IMS/JWT + SSE session header patches after conflict resolution.
- **`changelog-entry`** — draft `adobe/CHANGELOG.md` bullets (human confirmation required before writing).

Command wrappers live under `.claude/commands/`.

## Adobe patches (do not drop silently)

- **`jwt.rs`:** Adobe-specific token validation (including work around **`exp`**, **`claim_as_millis`**, and **`TokenError::Expired`** semantics vs upstream).
- **`sse.rs`:** Session correlation header(s), notably **`HEADER_SESSION_ID`** / MCP SSE expectations for Envoy.

Protected paths for automation: `**/jwt.rs`, `**/sse.rs`, **`adobe/`** (see `classify_commit.py` and `adobe/CONVENTIONS.md`).

## Feature flag

Adobe-only code must compile behind **`feature = "adobe"`** on the `agentgateway` crate (and enabled from `agentgateway-app` / Docker builds as documented). See **`adobe/CONVENTIONS.md`**.

## MCP

GitHub MCP (`mcp__cloud-github__*`) is **optional** — workflows must degrade gracefully when it is not configured.

## Guardrails

- **No direct pushes** to `adobe` with unreviewed automation output — use **`sync/*` PRs** and the documented land flow (`/land` / `poll-and-land` playbook).
- When classification reports **`critical`**, **halt** mutations; for **`high`**, require explicit chat acknowledgment before proceeding (`agentgateway-sync` references).
