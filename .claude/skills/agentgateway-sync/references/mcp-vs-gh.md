# When to use `cloud-github` MCP vs the `gh` CLI

Both can reach the Adobe EMU org `Adobe-Apis` once the PAT has been SSO-authorised (they share the same token under the hood), and both reach public `agentgateway/agentgateway` without auth. Choose by what you're doing.

## `cloud-github` MCP — prefer when:

- You need **one specific piece of PR metadata** (title, body, files, comments, reviews). One tool call, no shell plumbing, parameter validation is handled, output is structured JSON.
- The user is interactively reading your tool outputs (MCP tool results are laid out more cleanly than `gh api` dumps).
- You're reading **public** upstream data (`agentgateway/agentgateway`) — no auth concerns at all.

Typical calls:

- `mcp__cloud-github__get_pull_request` — header + body + head.ref + changed_files count.
- `mcp__cloud-github__get_pull_request_files` — per-file patches.
- `mcp__cloud-github__get_pull_request_comments` — inline review comments.
- `mcp__cloud-github__get_pull_request_reviews` — review actions (approved, changes requested).
- `mcp__cloud-github__get_file_contents` — reading a file at a ref, no clone required.

## `gh` CLI — prefer when:

- You're doing **bulk or parallelised** fetches (e.g., pulling metadata for dozens of PRs in a `xargs -P` loop). One MCP call per PR adds up.
- You're **writing** to the repo (open PR, close PR, push, fetch). `gh pr create`, `git push`, `git fetch` are the workflow tools; MCP has equivalents but the shell tooling is more ergonomic and composable.
- You need **branch data** (`gh api repos/{owner}/{repo}/branches/{branch}`) or other repo-wide API endpoints that MCP doesn't surface as first-class tools.

Always invoke as `env -u GITHUB_TOKEN -u GH_TOKEN gh ...` to avoid a stale env-var token masking the `hosts.yml` credential.

## Auth footgun — applies to both

If `gh auth status` shows the right account but API calls return 404 or 403:

- **404**: probably a `GITHUB_TOKEN` environment variable is overriding `hosts.yml` and resolving to a different user. Unset it for the call.
- **403 "Resource protected by organization SAML enforcement"**: the PAT exists but hasn't been SSO-authorised for this org. The error body contains the exact `https://github.com/enterprises/adobe-prd/sso?authorization_request=...` URL — send the user to it and wait for confirmation.
- **403 "without workflow scope"** on a push: the token missing the `workflow` scope, which is required when a commit touches `.github/workflows/*`. Usually means a global `url.insteadOf` rewrite in `~/.gitconfig` is injecting an older token that doesn't have that scope. Push with `https://x-access-token:$(gh auth token --hostname github.com)@github.com/...` to override.
