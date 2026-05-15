#!/usr/bin/env python3
"""Bisect the largest clean upstream prefix that can be batch-rebased.

Usage:
    python3 find_clean_prefix.py <repo> --base-ref <ref> \
        --upstream-ref <ref> --count <N>

The skill calls this when a batch rebase of N upstream commits onto
`adobe` produces a conflict. Goal: find the largest K (1 <= K <= N)
such that rebasing Adobe-only commits onto the K-th upstream commit is
clean. Output:

    {
      "clean_count": 47,
      "clean_end_sha": "...",
      "clean_end_short_sha": "...",
      "conflicting_count": 48,
      "conflicting_sha": "...",
      "conflicting_short_sha": "...",
      "conflicting_subject": "...",
      "conflicting_pr_num": "1234",
      "attempts": [
        {"k": 25, "result": "clean"},
        {"k": 37, "result": "conflict"},
        ...
      ]
    }

If even K=1 conflicts (clean_count == 0), the conflicting commit is the
oldest unsynced one and the skill should fall back to single-commit
conflict triage.

Method: binary search on K, testing each candidate by attempting a real
rebase on a throwaway branch and immediately aborting. The repo is left
in the same state it started in: HEAD on `<base-ref>` (typically
`adobe`), no in-progress rebase, no leftover branches.

Why a script: each bisection step is several git invocations
(checkout + branch + rebase + abort + branch -D). Doing them in a
single subprocess avoids the per-Bash approval prompts the dispatcher
would otherwise hit log2(N) times.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys


BISECT_BRANCH = "sync-bisect-tmp"


def run(repo: str, *git_args: str) -> tuple[int, str, str]:
    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}
    p = subprocess.run(
        ["git", "-C", repo, *git_args], capture_output=True, text=True, env=env
    )
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def list_commits(repo: str, base_ref: str, upstream_ref: str, count: int) -> list[dict]:
    rc, out, err = run(
        repo,
        "log",
        "--reverse",
        "--no-merges",
        "--pretty=%H%x09%h%x09%s",
        f"{base_ref}..{upstream_ref}",
    )
    if rc != 0:
        raise RuntimeError(f"git log failed: {err}")
    commits: list[dict] = []
    for line in out.splitlines()[:count]:
        parts = line.split("\t", 2)
        if len(parts) != 3:
            continue
        full, short, subject = parts
        m = re.search(r"\(#(\d+)\)\s*$", subject)
        commits.append(
            {
                "sha": full,
                "short_sha": short,
                "subject": subject,
                "pr_num": m.group(1) if m else None,
            }
        )
    return commits


def cleanup(repo: str, base_ref: str) -> None:
    """Best-effort restore: abort any rebase, switch back to base_ref,
    delete the bisect branch.  Errors are swallowed since the goal is
    to leave the repo usable; the script's caller can re-check `git
    status` if needed."""
    run(repo, "rebase", "--abort")
    run(repo, "switch", base_ref)
    run(repo, "branch", "-D", BISECT_BRANCH)


def attempt_rebase(repo: str, base_ref: str, target_sha: str) -> tuple[bool, str]:
    """Try `git rebase <target_sha>` from a fresh branch off base_ref.
    Returns (clean, info). On failure, the rebase is aborted and the
    branch deleted before returning."""
    rc, _, err = run(repo, "switch", base_ref)
    if rc != 0:
        return (False, f"could not switch to {base_ref}: {err}")

    run(repo, "branch", "-D", BISECT_BRANCH)
    rc, _, err = run(repo, "switch", "-c", BISECT_BRANCH)
    if rc != 0:
        return (False, f"could not create bisect branch: {err}")

    rc, _, err = run(repo, "rebase", target_sha)
    if rc == 0:
        run(repo, "switch", base_ref)
        run(repo, "branch", "-D", BISECT_BRANCH)
        return (True, "ok")

    rc_status, status_out, _ = run(repo, "status", "--porcelain")
    has_conflict_markers = any(
        line.startswith(("UU", "AA", "DD", "AU", "UA", "DU", "UD"))
        for line in status_out.splitlines()
    )
    cleanup(repo, base_ref)
    if has_conflict_markers or rc != 0:
        return (False, "conflict")
    return (False, f"rebase failed without conflict: {err}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo")
    ap.add_argument("--base-ref", default="adobe")
    ap.add_argument("--upstream-ref", default="upstream/main")
    ap.add_argument("--count", type=int, required=True)
    args = ap.parse_args()

    if args.count < 1:
        out = {"errors": [f"--count must be >= 1, got {args.count}"]}
        _emit(out)
        return 1

    if not os.path.isdir(os.path.join(args.repo, ".git")):
        _emit({"errors": [f"{args.repo} is not a git checkout"]})
        return 1

    try:
        commits = list_commits(args.repo, args.base_ref, args.upstream_ref, args.count)
    except RuntimeError as e:
        _emit({"errors": [str(e)]})
        return 1

    if not commits:
        _emit({"errors": ["no unsynced commits found"]})
        return 1

    n = len(commits)
    attempts: list[dict] = []

    lo = 0
    hi = n
    last_clean = 0

    while lo < hi:
        mid = (lo + hi + 1) // 2
        target = commits[mid - 1]["sha"]
        clean, info = attempt_rebase(args.repo, args.base_ref, target)
        attempts.append({"k": mid, "sha": commits[mid - 1]["sha"], "result": "clean" if clean else info})
        if clean:
            last_clean = mid
            lo = mid
        else:
            hi = mid - 1

    cleanup(args.repo, args.base_ref)

    result: dict = {
        "requested_count": args.count,
        "available_count": n,
        "clean_count": last_clean,
        "clean_end_sha": commits[last_clean - 1]["sha"] if last_clean > 0 else None,
        "clean_end_short_sha": commits[last_clean - 1]["short_sha"] if last_clean > 0 else None,
        "attempts": attempts,
    }

    if last_clean < n:
        conflicting = commits[last_clean]
        result["conflicting_count"] = last_clean + 1
        result["conflicting_sha"] = conflicting["sha"]
        result["conflicting_short_sha"] = conflicting["short_sha"]
        result["conflicting_subject"] = conflicting["subject"]
        result["conflicting_pr_num"] = conflicting["pr_num"]
    else:
        result["conflicting_count"] = None
        result["conflicting_sha"] = None
        result["conflicting_short_sha"] = None
        result["conflicting_subject"] = None
        result["conflicting_pr_num"] = None

    _emit(result)
    return 0


def _emit(data: dict) -> None:
    print(json.dumps(data, indent=2))


if __name__ == "__main__":
    sys.exit(main())
