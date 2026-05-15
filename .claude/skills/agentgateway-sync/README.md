# agentgateway-sync (Adobe)

Claude Code skill for maintaining **Adobe-Apis/agentgateway** against **agentgateway/agentgateway**.

## Quick start

From the **Adobe fork** repository root:

```bash
python3 .claude/skills/agentgateway-sync/scripts/inspect_state.py .
python3 .claude/skills/agentgateway-sync/scripts/pick_sync_branch.py .
```

See `SKILL.md` and `references/` for the full workflow.

## Troubleshooting

If `inspect_state.py` reports missing or wrong `upstream`, or `unsynced_count` is null, run `scripts/inspect_state.py <repo> --fix-remotes` (or fix remotes manually per JSON `errors`).
