#!/usr/bin/env python3
"""Prune landed/abandoned sync branches, local and on origin.

The batch+yolo land step deletes its own `sync/*` branch best-effort, so
over a multi-batch burndown leftovers accumulate — interrupted runs,
failed remote deletes, throwaway bisect branches. They make
`pick_sync_branch.py` reach for `-1`/`-2` suffixes and clutter the repo.
This preflight removes only branches that are provably done.

Scope (never touches anything else): branch names matching `sync/*` plus
the throwaway `sync-bisect-tmp`. `adobe`, the current HEAD branch,
`skill/*`, `fix-*`, etc. are always kept.

Prune rules:
  * `sync-bisect-tmp` (local) — always stale; delete.
  * A `sync/*` branch whose Adobe-Apis PR is MERGED or CLOSED — delete.
  * A local `sync/*` branch with no PR that is fully merged into `adobe`
    (`git merge-base --is-ancestor branch adobe`) — landed via
    force-push; delete. A local branch with no PR that is NOT an
    ancestor of `adobe` is kept (possible in-progress work).
  * A remote `sync/*` branch with no PR at all is kept (conservative —
    only PR state authorises a remote delete).

Default is a dry run. Pass `--apply` to actually delete.

Usage:
    python3 prune_stale_branches.py <repo> [--apply] [--base-ref adobe]

Emits JSON: {candidates[], pruned[], kept[], errors[]}. Each candidate is
{name, scope: local|remote, reason, pr, pr_state}. Exit 0 unless setup
fails (exit 1); individual delete failures are recorded in `errors` but
do not fail the run (best-effort cleanup).
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys

REMOTE = "origin"
ADOBE_REPO = "Adobe-Apis/agentgateway"
BISECT_BRANCH = "sync-bisect-tmp"


def run(cmd: list[str]) -> tuple[int, str, str]:
    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}
    p = subprocess.run(cmd, capture_output=True, text=True, env=env)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def git(repo: str, *args: str) -> tuple[int, str, str]:
    return run(["git", "-C", repo, *args])


def is_sync_branch(name: str) -> bool:
    return name == BISECT_BRANCH or name.startswith("sync/")


def local_branches(repo: str) -> list[str]:
    rc, out, _ = git(repo, "for-each-ref", "--format=%(refname:short)", "refs/heads")
    if rc != 0:
        return []
    return [b for b in out.splitlines() if is_sync_branch(b)]


def remote_branches(repo: str) -> list[str]:
    rc, out, _ = git(
        repo, "for-each-ref", "--format=%(refname:short)", f"refs/remotes/{REMOTE}"
    )
    if rc != 0:
        return []
    names = []
    prefix = f"{REMOTE}/"
    for ref in out.splitlines():
        if not ref.startswith(prefix):
            continue
        name = ref[len(prefix):]
        if is_sync_branch(name):
            names.append(name)
    return names


def current_branch(repo: str) -> str | None:
    rc, out, _ = git(repo, "rev-parse", "--abbrev-ref", "HEAD")
    return out if rc == 0 else None


def pr_state_for(branch: str) -> tuple[int | None, str | None]:
    """Return (pr_number, state) for the newest PR with this head, or (None, None)."""
    rc, out, _ = run(
        [
            "gh", "pr", "list", "--repo", ADOBE_REPO,
            "--head", branch, "--state", "all",
            "--json", "number,state", "--limit", "10",
        ]
    )
    if rc != 0 or not out:
        return (None, None)
    try:
        prs = json.loads(out)
    except json.JSONDecodeError:
        return (None, None)
    if not prs:
        return (None, None)
    # Prefer a terminal state if any PR reached one.
    for st in ("MERGED", "CLOSED"):
        for pr in prs:
            if (pr.get("state") or "").upper() == st:
                return (pr.get("number"), st)
    pr = prs[0]
    return (pr.get("number"), (pr.get("state") or "").upper() or None)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo")
    ap.add_argument("--apply", action="store_true", help="Actually delete (default: dry run)")
    ap.add_argument("--base-ref", default="adobe")
    args = ap.parse_args()

    out: dict = {"candidates": [], "pruned": [], "kept": [], "errors": [], "applied": args.apply}

    if not os.path.isdir(os.path.join(args.repo, ".git")):
        out["errors"].append(f"{args.repo} is not a git checkout")
        print(json.dumps(out, indent=2))
        return 1

    cur = current_branch(args.repo)

    # ---- local candidates ----
    for name in local_branches(args.repo):
        if name == cur:
            out["kept"].append({"name": name, "scope": "local", "reason": "current HEAD"})
            continue
        if name == BISECT_BRANCH:
            out["candidates"].append(
                {"name": name, "scope": "local", "reason": "throwaway bisect branch", "pr": None, "pr_state": None}
            )
            continue
        pr, state = pr_state_for(name)
        if state in ("MERGED", "CLOSED"):
            out["candidates"].append(
                {"name": name, "scope": "local", "reason": f"PR {state}", "pr": pr, "pr_state": state}
            )
        elif pr is None:
            merged = git(args.repo, "merge-base", "--is-ancestor", name, args.base_ref)[0] == 0
            if merged:
                out["candidates"].append(
                    {"name": name, "scope": "local", "reason": f"no PR; merged into {args.base_ref}", "pr": None, "pr_state": None}
                )
            else:
                out["kept"].append(
                    {"name": name, "scope": "local", "reason": f"no PR; not an ancestor of {args.base_ref} (possible WIP)"}
                )
        else:
            out["kept"].append({"name": name, "scope": "local", "reason": f"PR {state} (open)"})

    # ---- remote candidates ----
    for name in remote_branches(args.repo):
        pr, state = pr_state_for(name)
        if state in ("MERGED", "CLOSED"):
            out["candidates"].append(
                {"name": name, "scope": "remote", "reason": f"PR {state}", "pr": pr, "pr_state": state}
            )
        else:
            reason = f"PR {state} (open)" if state else "no PR (conservative keep)"
            out["kept"].append({"name": name, "scope": "remote", "reason": reason})

    # ---- apply ----
    if args.apply:
        for c in out["candidates"]:
            if c["scope"] == "local":
                rc, _, err = git(args.repo, "branch", "-D", c["name"])
            else:
                rc, _, err = run(["git", "-C", args.repo, "push", REMOTE, "--delete", c["name"]])
            if rc == 0:
                out["pruned"].append(c)
            else:
                out["errors"].append(f"delete {c['scope']} {c['name']} failed: {err}")

    print(json.dumps(out, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
