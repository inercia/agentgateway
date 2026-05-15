#!/usr/bin/env python3
"""Pick a free sync branch name for the next agentgateway port.

Usage:
    python3 pick_sync_branch.py <repo_path> <source_branch>

Emits JSON with the chosen branch name and any prior PRs being
superseded. A candidate name is "taken" if any of these holds:
    - a branch with that name exists on origin
    - a PR on Adobe-Apis (open, closed, or merged) has that name as
      its head ref

The chosen name is `sync/<source-branch>`, with `-1`, `-2`, ...
suffixes on collision. Suffixes are capped at 99 as a sanity limit —
getting past that means something else is wrong.

Exit status:
    0 — picked a name successfully
    1 — unrecoverable error (bad args, sanity cap hit)

This script replaces the in-SKILL bash while-loop that used to do the
same thing; consolidating into a single subprocess call removes
multiple Claude Code approval prompts per sync run.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys


SANITY_CAP = 99


def run(cmd: list[str]) -> tuple[int, str, str]:
    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}
    p = subprocess.run(cmd, capture_output=True, text=True, env=env)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def remote_branch_exists(repo: str, name: str) -> bool:
    rc, out, _ = run(["git", "-C", repo, "ls-remote", "--heads", "origin", name])
    return rc == 0 and bool(out.strip())


def existing_prs(name: str) -> list[dict]:
    rc, out, _ = run([
        "gh", "pr", "list",
        "--repo", "Adobe-Apis/agentgateway",
        "--head", name,
        "--state", "all",
        "--json", "number,state,url,title",
    ])
    if rc != 0 or not out:
        return []
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        return []


def normalize(name: str) -> str:
    """Flatten upstream branch names into something the team's convention
    accepts. Upstream often uses slash-separated names (``telemetry/span-links``,
    ``feat/x``); the Adobe fork has always used dashes (``telemetry-span-links``).
    Without normalising, collision detection would miss a closed PR that
    used the dashed form.
    """
    return name.replace("/", "-")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo")
    ap.add_argument(
        "source_branch",
        nargs="?",
        default=None,
        help="Upstream PR's head.ref, e.g. telemetry/span-links. Required "
             "for single-commit syncs. Ignored when --batch is given.",
    )
    ap.add_argument(
        "--batch",
        action="store_true",
        help="Pick a batch sync branch name. Requires --count and --end-short-sha.",
    )
    ap.add_argument("--count", type=int, default=None, help="Number of commits in the batch (used with --batch)")
    ap.add_argument(
        "--end-short-sha",
        default=None,
        help="Short SHA of the last commit in the batch (used with --batch)",
    )
    args = ap.parse_args()

    if args.batch:
        if args.count is None or args.end_short_sha is None:
            _emit({"errors": ["--batch requires both --count and --end-short-sha"]})
            return 1
        base = f"sync/batch-{args.count}-to-{args.end_short_sha}"
    else:
        if not args.source_branch:
            _emit({"errors": ["source_branch is required when --batch is not set"]})
            return 1
        source = normalize(args.source_branch)
        base = f"sync/{source}"

    candidate = base
    suffix = 0
    superseded: list[dict] = []

    while True:
        prs = existing_prs(candidate)
        if not remote_branch_exists(args.repo, candidate) and not prs:
            break
        for pr in prs:
            if pr not in superseded:
                superseded.append(pr)
        suffix += 1
        if suffix > SANITY_CAP:
            _emit({
                "errors": [f"more than {SANITY_CAP} prior attempts for {base} — stop and investigate"],
                "base_name": base,
            })
            return 1
        candidate = f"{base}-{suffix}"

    _emit({
        "sync_branch": candidate,
        "base_name": base,
        "suffix": suffix,
        "superseded_prs": superseded,
    })
    return 0


def _emit(data: dict) -> None:
    print(json.dumps(data, indent=2))


if __name__ == "__main__":
    sys.exit(main())
