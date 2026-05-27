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

## 1.1.0

- MCP Apps: multiplexed resources, resource templates, tasks, subscribe/unsubscribe, completions, capability cache, and `ui://` URI wrapping for MCP App resources/tools (`#[cfg(feature = "adobe")]`).
- Add `mcp.task` to MCPInfo CEL surface (regenerate `schema/cel.json` / related docs with `make generate-schema` or `cargo xtask schema`).
- Advertise `prompts` capability when multiplexing federated upstreams; the gateway already namespaces prompt names via `target_` so hosts like MCP Inspector can now discover federated prompts.
- Wrap `_meta.ui.resourceUri` in `tools/call` responses so MCP Apps tools that return a dynamic UI resource URI round-trip through `resources/read` (previously they reached the multiplex parser raw and failed with `multiplex URI missing 'u' query param`).
- Also wrap `CallToolResult.content` EmbeddedResource URIs (A2UI sample wire form: `get_basic_app` / `get_editor_app` return `ui://...` in content blocks, not only in `_meta`).
- Also wrap the legacy flat `_meta["ui/resourceUri"]` field alongside nested `_meta.ui.resourceUri` (`registerAppTool` populates both; hosts that read the legacy key no longer hit raw multiplex `resources/read`).
- Advertise a full TasksCapability for merged initialize (list, cancel, requests.tools.call) instead of `{}` under `tasks`, so MCP Inspector enables Tasks when multiplexing (rmcp `TasksCapability::server_default()`).
- Build pipeline: rename `adobe/Makefile`'s Docker build-arg from `CARGO_BUILD_FEATURES` to `CARGO_FEATURES` so it matches `Dockerfile`'s `ARG CARGO_FEATURES`; also switch the default features to the package-qualified form `agentgateway/ui,agentgateway/adobe`. Prior builds silently produced binaries without the `adobe` feature because Docker drops undeclared build-args, which stripped every `#[cfg(feature = "adobe")]` block (including the MCP Apps wiring above) at compile time.

## 1.2.0

- Dev velocity: `make snapshot` defaults to `quick-release` with sccache and Docker buildx local cache; use `make release` (or `PROFILE=release make snapshot`) for the LTO release image.
- MCP rewrite dataplane: `BackendPolicySpec.McpRewrite` (proto field 18) compiled into `McpRewriteSet` and applied in the MCP handler after authorization and before multiplexing (`auth` → `rewrite` → `multiplex`). Supports per-target tool/prompt/resource presentation rules plus federation-scoped `rewrite.server` (`name`, `instructions`, `resourceNaming` Flat/Prefix). Session state keeps a reverse-map from exposed Flat names back to upstream `(target, upstream)` for `tools/call`, `prompts/get`, and related methods.
- **Flat** `resourceNaming` now applies to federated `resources/list`, `resources/templates/list`, and `tasks/list` (not only tools/prompts): flat exposed names with runtime collision omission; URIs and `uriTemplate` values remain multiplex-wrapped for `resources/read` / MCP Apps routing; flat `taskId` resolution fans out across upstream targets with ambiguity errors when multiple backends could own the same id.
- Adobe feature gating (`6469b3ea`): MCP rewrite, IMS JWT `created_at`/`expires_in` validation, and MCP SSE `mcp-session-id` response header compile only with `feature = "adobe"`; default builds use `rewrite_stub` no-ops so upstream-style binaries stay lean.
- Control-plane pairing: configure presentation via agentlink **0.5.1** `AIPolicy.spec.backend.mcp.rewrite` (per-target `sectionName`; federation-only `rewrite.server`). See agentlink `BREAKING_CHANGES.md` 0.5.1 and `docs/MCPRewritePolicy.md`.
