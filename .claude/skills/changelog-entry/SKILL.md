---
name: changelog-entry
description: Draft adobe/CHANGELOG.md bullets for a sync PR without writing files until an engineer confirms.
---

# Changelog entry helper (Adobe agentgateway)

## Collect context

From the PR or local branch, list commits being introduced relative to `adobe`:

```bash
git log --oneline adobe..HEAD
```

Use **`sync/*`** working branches for integration work; do **not** assume names like `upstream-sync/*`.

## Draft bullets

Propose concise bullets under a new `## x.y.z` section in `adobe/CHANGELOG.md`:

- Call out user-visible behavior, rollout risk (`adobe` feature / Docker `CARGO_BUILD_FEATURES`), and IMS/JWT/SSE touchpoints when relevant.

## Important

**Do not** write or commit `adobe/CHANGELOG.md` from automation. Paste the proposed text into the PR / chat and wait for **explicit engineer confirmation** before applying the edit.
