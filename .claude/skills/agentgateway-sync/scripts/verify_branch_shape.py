#!/usr/bin/env python3
"""Validate Shape D on a sync branch before pushing / opening a PR.

A sync branch must carry the upstream batch commits **verbatim** at its
base (same SHA = same content, author, message), with the Adobe-only
commits reapplied on top. This script asserts that invariant and that
the branch matches the commit count the PR body will claim — catching
two real incident classes:

  * verbatim corruption — a stray `--rebase-merges`, an editing hook, or
    a bad conflict resolution rewrote an upstream commit (would ship
    altered upstream history under a fork SHA). The fix-up that followed
    PR #75 was this class.
  * count drift — the branch carries a different number of upstream
    commits than the PR body lists (PR #81 said 4, body listed 12).

Usage:
    python3 verify_branch_shape.py <repo> \
        --head <ref> \
        --expected-shas <sha1,sha2,...> \
        [--expected-count <N>]

`--expected-shas` is the ordered list of upstream batch commit SHAs
(`batch_commits[*].sha`). `--expected-count` defaults to the number of
expected SHAs; pass the count the PR body will claim (from the *same*
inspect.json that feeds compose_pr_body.py) to assert they agree.

Emits JSON on stdout:
    {
      "ok": true,
      "head_sha": "...",
      "expected_count": 4,
      "present_count": 4,
      "missing_shas": [],
      "count_match": true,
      "batch_end_is_ancestor": true,
      "adobe_commits_on_top": 7,
      "errors": []
    }

Exit status:
    0 — invariant holds (`ok: true`)
    1 — invariant violated (`ok: false`) or setup error; do NOT push.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys


def run(repo: str, *git_args: str) -> tuple[int, str, str]:
    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}
    p = subprocess.run(
        ["git", "-C", repo, *git_args], capture_output=True, text=True, env=env
    )
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def is_ancestor(repo: str, ancestor: str, descendant: str) -> bool:
    rc, _, _ = run(repo, "merge-base", "--is-ancestor", ancestor, descendant)
    return rc == 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo")
    ap.add_argument("--head", default="HEAD", help="Sync branch ref to validate")
    ap.add_argument(
        "--expected-shas",
        required=True,
        help="Comma-separated ordered upstream batch SHAs (batch_commits[*].sha)",
    )
    ap.add_argument(
        "--expected-count",
        type=int,
        default=None,
        help="Count the PR body will claim; defaults to len(expected-shas)",
    )
    args = ap.parse_args()

    out: dict = {
        "ok": False,
        "head_sha": None,
        "expected_count": None,
        "present_count": 0,
        "missing_shas": [],
        "count_match": False,
        "batch_end_is_ancestor": False,
        "adobe_commits_on_top": None,
        "errors": [],
    }

    if not os.path.isdir(os.path.join(args.repo, ".git")):
        out["errors"].append(f"{args.repo} is not a git checkout")
        print(json.dumps(out, indent=2))
        return 1

    expected = [s.strip() for s in args.expected_shas.split(",") if s.strip()]
    if not expected:
        out["errors"].append("--expected-shas is empty")
        print(json.dumps(out, indent=2))
        return 1

    expected_count = args.expected_count if args.expected_count is not None else len(expected)
    out["expected_count"] = expected_count

    rc, head_sha, err = run(args.repo, "rev-parse", args.head)
    if rc != 0:
        out["errors"].append(f"could not resolve {args.head}: {err}")
        print(json.dumps(out, indent=2))
        return 1
    out["head_sha"] = head_sha

    # Verbatim integrity: every expected upstream SHA must be present as an
    # ancestor of HEAD. Same SHA == byte-identical content/author/message, so
    # an ancestor hit proves the commit was carried verbatim, not rewritten.
    missing = [sha for sha in expected if not is_ancestor(args.repo, sha, head_sha)]
    out["missing_shas"] = missing
    out["present_count"] = len(expected) - len(missing)

    # The last batch commit is the rebase fence; it must be an ancestor too.
    batch_end = expected[-1]
    out["batch_end_is_ancestor"] = is_ancestor(args.repo, batch_end, head_sha)

    # Adobe commits reapplied on top = commits between batch_end and HEAD.
    rc, cnt, _ = run(args.repo, "rev-list", "--count", f"{batch_end}..{head_sha}")
    if rc == 0 and cnt.isdigit():
        out["adobe_commits_on_top"] = int(cnt)

    out["count_match"] = len(expected) == expected_count

    if missing:
        out["errors"].append(
            "verbatim integrity violated — these upstream SHAs are not ancestors "
            f"of {args.head} (rebase rewrote them?): {', '.join(missing)}"
        )
    if not out["batch_end_is_ancestor"]:
        out["errors"].append(
            f"batch end {batch_end} is not an ancestor of {args.head}"
        )
    if not out["count_match"]:
        out["errors"].append(
            f"commit-count drift — branch carries {len(expected)} upstream commits "
            f"but PR body will claim {expected_count}; align the inspect.json that "
            "feeds compose_pr_body.py with the rebased batch before opening the PR"
        )

    out["ok"] = not out["errors"]
    print(json.dumps(out, indent=2))
    return 0 if out["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
