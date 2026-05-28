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
- Adobe feature gating (`6469b3ea`): MCP rewrite, IMS JWT `created_at`/`expires_in` validation, and MCP SSE `mcp-session-id` response header compile only with `feature = "adobe"`; default builds use `rewrite_stub` no-ops so upstream-style binaries stay lean.
- Control-plane pairing: configure presentation via agentlink **0.5.1** `AIPolicy.spec.backend.mcp.rewrite` (per-target `sectionName`; federation-only `rewrite.server`). See agentlink `BREAKING_CHANGES.md` 0.5.1 and `docs/MCPRewritePolicy.md`.


## 1.2.1

- **Federated MCP Tasks (multiplex + McpRewrite):** end-to-end Tasks when fronting multiple upstreams with `AIPolicy.spec.backend.mcp.rewrite` — `tasks/list` fanout, `tasks/get` / `tasks/result` / `tasks/cancel` with RBAC, `tools/call` → `CreateTaskResult`, and merged `TasksCapability` on initialize (MCP Inspector–compatible).
- **Task ids honor `resourceNaming`:** Prefix federation exposes `target_<upstreamTaskId>` on every client-visible surface (create, list, get/result/cancel responses, `notifications/tasks/status`, per-upstream GET/SSE). Flat federation keeps bare upstream ids with a `flat_task_routes` index (same first-wins collision rules as tools), populated by `tasks/list` and create.
- **Flat federation routing (tools + tasks):** first-wins route indexes for `tools/call` and task RPCs; lazy internal `tools/list` / `tasks/list` refresh when an in-memory index is empty (multi-replica session resume).
- **Flat catalog presentation:** `resourceNaming: Flat` applies to federated `resources/list`, `resources/templates/list`, and `tasks/list` (not only tools/prompts); exposed names omit on collision; resource URIs / `uriTemplate` stay multiplex-wrapped for `resources/read` and MCP Apps.
- **Implementation layout:** task outbound rewrap in `multiplex_naming`, Adobe task dispatch in `session/tasks.rs`, `federation_outbound::map_mux_outbound_message` for shared stream mapping.
