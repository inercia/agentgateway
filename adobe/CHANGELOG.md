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

## 1.0.1

- Vendor the upstream sync skill into `.claude/skills/agentgateway-sync` (`SKILL.md`, `references/`, `scripts/`, `.claude/settings.json` allowlists). See git log for per-change detail.
