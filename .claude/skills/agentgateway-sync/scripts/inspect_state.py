#!/usr/bin/env python3
"""Inspect the agentgateway checkout for the upstream sync skill.

Usage:
    python3 inspect_state.py <repo_path> [--no-fetch]

Emits a single JSON object to stdout describing everything the
agentgateway-sync skill needs to decide its next move:
cleanliness, remotes, branch state, count of unsynced upstream commits,
the next commit + PR + source branch, Adobe-Apis reachability, and
repo-local token hygiene.

Exit status:
    0 — ran to completion (check `errors` array in output for problems)
    1 — unrecoverable setup error (bad args, repo doesn't exist)

The skill must parse the output, surface any `errors` entries to the
user, and checkpoint before any mutating operation.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys

EXPECTED_UPSTREAM = "https://github.com/agentgateway/agentgateway.git"


def run(cmd: list[str]) -> tuple[int, str, str]:
    """Run a subprocess with GITHUB_TOKEN/GH_TOKEN stripped from env."""
    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}
    p = subprocess.run(cmd, capture_output=True, text=True, env=env)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def git(repo: str, *args: str) -> tuple[int, str, str]:
    return run(["git", "-C", repo, *args])


def gh(*args: str) -> tuple[int, str, str]:
    return run(["gh", *args])


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo", help="Path to Adobe-Apis/agentgateway checkout")
    ap.add_argument(
        "--no-fetch",
        action="store_true",
        help="Skip `git fetch` — trust what's cached locally",
    )
    ap.add_argument("--output", default=None, help="write JSON result to this file instead of stdout")
    args = ap.parse_args()

    state: dict = {
        "repo": os.path.abspath(args.repo),
        "errors": [],
    }

    if not os.path.isdir(os.path.join(args.repo, ".git")):
        state["errors"].append(f"{args.repo} is not a git checkout")
        _emit(state, args.output)
        return 1

    rc, out, err = git(args.repo, "status", "--porcelain")
    state["working_tree_clean"] = (rc == 0 and not out)
    if rc != 0:
        state["errors"].append(f"git status failed: {err}")
    if out:
        state["working_tree_preview"] = out.splitlines()[:10]

    rc, out, _ = git(args.repo, "rev-parse", "--abbrev-ref", "HEAD")
    state["current_branch"] = out if rc == 0 else None

    for remote in ("origin", "upstream"):
        rc, url, _ = git(args.repo, "remote", "get-url", remote)
        state[f"remote_{remote}_url"] = url if rc == 0 else None

    upstream_url = state.get("remote_upstream_url")
    if upstream_url is None:
        state["errors"].append(
            "upstream remote is missing — run "
            f"`git -C {args.repo} remote add upstream {EXPECTED_UPSTREAM}` "
            "(or `git remote set-url upstream …` if the name exists but points elsewhere)"
        )
    elif upstream_url != EXPECTED_UPSTREAM:
        state["errors"].append(
            f"upstream remote is {upstream_url!r}, "
            f"expected {EXPECTED_UPSTREAM!r} — run "
            f"`git -C {args.repo} remote set-url upstream {EXPECTED_UPSTREAM}`"
        )

    repo_gitconfig = os.path.join(args.repo, ".git", "config")
    try:
        with open(repo_gitconfig) as fh:
            content = fh.read()
        state["token_leaks_repo_config"] = len(re.findall(r"\b(?:ghp|gho|ghu|ghs)_|github_pat_", content))
        if state["token_leaks_repo_config"] > 0:
            state["errors"].append(
                f"{repo_gitconfig} contains {state['token_leaks_repo_config']} "
                "embedded token pattern(s) — rotate the tokens and clean the file before pushing"
            )
    except OSError as e:
        state["token_leaks_repo_config"] = None
        state["errors"].append(f"could not read {repo_gitconfig}: {e}")

    rc, _, _ = git(args.repo, "show-ref", "--verify", "--quiet", "refs/heads/adobe")
    state["adobe_local"] = (rc == 0)

    if not args.no_fetch:
        for remote in ("origin", "upstream"):
            rc, _, err = git(args.repo, "fetch", remote)
            if rc != 0:
                state["errors"].append(f"git fetch {remote} failed: {err}")

    base_ref = "adobe" if state["adobe_local"] else "origin/adobe"
    state["base_ref_used"] = base_ref

    upstream_ok = upstream_url == EXPECTED_UPSTREAM

    if upstream_ok:
        rc, out, err = git(args.repo, "rev-list", "--count", f"{base_ref}..upstream/main")
        if rc == 0:
            state["unsynced_count"] = int(out)
        else:
            state["unsynced_count"] = None
            state["errors"].append(f"could not count unsynced commits: {err}")
    else:
        state["unsynced_count"] = None

    state.update(
        {
            "oldest_sha": None,
            "oldest_short_sha": None,
            "oldest_subject": None,
            "oldest_author_date": None,
            "pr_num": None,
            "source_branch": None,
        }
    )

    if upstream_ok and state.get("unsynced_count"):
        rc, out, _ = git(
            args.repo, "log", "--reverse", "--no-merges", "--pretty=%H", f"{base_ref}..upstream/main"
        )
        if rc == 0 and out:
            oldest = out.splitlines()[0]
            state["oldest_sha"] = oldest

            rc2, short, _ = git(args.repo, "rev-parse", "--short", oldest)
            state["oldest_short_sha"] = short if rc2 == 0 else oldest[:8]

            rc2, subject, _ = git(args.repo, "log", "-1", "--pretty=%s", oldest)
            if rc2 == 0:
                state["oldest_subject"] = subject
                m = re.search(r"\(#(\d+)\)\s*$", subject)
                if m:
                    state["pr_num"] = m.group(1)
                else:
                    state["errors"].append(
                        "oldest commit subject has no trailing (#NNNN) — not a "
                        f"standard upstream squash-merge: {subject!r}"
                    )

            rc2, date, _ = git(args.repo, "log", "-1", "--pretty=%aI", oldest)
            if rc2 == 0:
                state["oldest_author_date"] = date

    if state["pr_num"]:
        rc, out, err = gh("api", f"repos/agentgateway/agentgateway/pulls/{state['pr_num']}", "--jq", ".head.ref")
        if rc == 0 and out:
            state["source_branch"] = out
        else:
            state["errors"].append(f"gh api failed for upstream PR #{state['pr_num']}: {err}")

    rc, out, err = gh("api", "repos/Adobe-Apis/agentgateway", "--jq", ".full_name")
    state["adobe_apis_reachable"] = (rc == 0 and out == "Adobe-Apis/agentgateway")
    if not state["adobe_apis_reachable"]:
        state["errors"].append(
            "Adobe-Apis/agentgateway not reachable via gh — most likely SSO "
            f"authorisation is missing. gh stderr: {err}"
        )

    _emit(state, args.output)
    return 0


def _emit(data: dict, output_path: str | None) -> None:
    text = json.dumps(data, indent=2) + "\n"
    if output_path:
        dirname = os.path.dirname(output_path)
        if dirname:
            os.makedirs(dirname, exist_ok=True)
        with open(output_path, "w") as fh:
            fh.write(text)
    else:
        print(text, end="")


if __name__ == "__main__":
    sys.exit(main())
