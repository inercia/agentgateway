#!/usr/bin/env python3
"""Inspect the agentgateway checkout for the upstream sync skill.

Usage:
    python3 inspect_state.py <repo_path> [--no-fetch] [--fix-remotes] [--skip-public]

--fix-remotes: mutates git config to add/fix `upstream` and optional `public`
remotes only (never `origin`). Stop with JSON errors if `origin` is wrong.

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
from pathlib import Path

_scripts = Path(__file__).resolve().parent
if str(_scripts) not in sys.path:
    sys.path.insert(0, str(_scripts))

from git_remote_norm import (  # noqa: E402
    EXPECTED_ORIGIN_ADOBE_HTTPS,
    EXPECTED_UPSTREAM_HTTPS,
    canonical_origin_adobe,
    canonical_upstream,
    normalize_github_remote,
    remotes_equivalent,
)

EXPECTED_UPSTREAM = EXPECTED_UPSTREAM_HTTPS


def run(cmd: list[str]) -> tuple[int, str, str]:
    """Run a subprocess with GITHUB_TOKEN/GH_TOKEN stripped from env."""
    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}
    p = subprocess.run(cmd, capture_output=True, text=True, env=env)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def git(repo: str, *args: str) -> tuple[int, str, str]:
    return run(["git", "-C", repo, *args])


def gh(*args: str) -> tuple[int, str, str]:
    return run(["gh", *args])


def fix_git_remotes(repo: str, skip_public: bool) -> tuple[list[str], bool]:
    """Idempotent fix for upstream/public only. Origin must be correct (manual).

    Returns (blocking_errors, applied_git_remote_mutations).
    """
    origin_raw = None
    rc, out, _ = git(repo, "remote", "get-url", "origin")
    if rc == 0:
        origin_raw = out

    _ORIGIN_CANON = canonical_origin_adobe()
    _UPSTREAM_CANON = canonical_upstream()
    blocking: list[str] = []
    checks: list[dict] = []

    origin_norm = normalize_github_remote(origin_raw) if origin_raw else None
    origin_ok = origin_norm == _ORIGIN_CANON

    if origin_raw is None:
        blocking.append(
            "wrong_origin: origin remote is missing — clone Adobe-Apis/agentgateway "
            f"(or `git -C {repo} remote add origin {EXPECTED_ORIGIN_ADOBE_HTTPS}`)"
        )
    elif origin_norm == _UPSTREAM_CANON:
        blocking.append(
            "wrong_origin: origin points at public agentgateway/agentgateway — "
            f"expected Adobe-Apis/agentgateway. Clone the fork or "
            f"`git -C {repo} remote set-url origin {EXPECTED_ORIGIN_ADOBE_HTTPS}`"
        )
    elif not origin_ok:
        blocking.append(
            f"wrong_origin: origin is {origin_raw!r} — expected Adobe-Apis/agentgateway "
            "(HTTPS or git@github.com:Adobe-Apis/agentgateway.git). Fix manually."
        )

    if blocking:
        return blocking, False

    upstream_raw = None
    rc, out, _ = git(repo, "remote", "get-url", "upstream")
    if rc == 0:
        upstream_raw = out
    upstream_norm = normalize_github_remote(upstream_raw) if upstream_raw else None
    upstream_ok = upstream_norm == _UPSTREAM_CANON

    if upstream_raw is None:
        checks.append(
            {
                "name": "upstream",
                "ok": False,
                "current": None,
                "fix": f"git -C {repo} remote add upstream {EXPECTED_UPSTREAM_HTTPS}",
            }
        )
    elif not upstream_ok:
        checks.append(
            {
                "name": "upstream",
                "ok": False,
                "current": upstream_raw,
                "fix": f"git -C {repo} remote set-url upstream {EXPECTED_UPSTREAM_HTTPS}",
            }
        )
    else:
        checks.append({"name": "upstream", "ok": True, "current": upstream_raw, "fix": None})

    if not skip_public:
        public_raw = None
        rc, out, _ = git(repo, "remote", "get-url", "public")
        if rc == 0:
            public_raw = out
        public_norm = normalize_github_remote(public_raw) if public_raw else None
        public_ok = public_norm == _UPSTREAM_CANON

        if public_raw is None:
            checks.append(
                {
                    "name": "public",
                    "ok": False,
                    "current": None,
                    "fix": f"git -C {repo} remote add public {EXPECTED_UPSTREAM_HTTPS}",
                }
            )
        elif not public_ok:
            checks.append(
                {
                    "name": "public",
                    "ok": False,
                    "current": public_raw,
                    "fix": f"git -C {repo} remote set-url public {EXPECTED_UPSTREAM_HTTPS}",
                }
            )
        else:
            checks.append({"name": "public", "ok": True, "current": public_raw, "fix": None})

    applied = False
    for c in checks:
        if c["ok"] or not c.get("fix"):
            continue
        if c["name"] not in ("upstream", "public"):
            continue
        if c["name"] == "public" and skip_public:
            continue
        had_url = c["current"]
        if had_url is None:
            rc, _, err = git(repo, "remote", "add", c["name"], EXPECTED_UPSTREAM_HTTPS)
        else:
            rc, _, err = git(repo, "remote", "set-url", c["name"], EXPECTED_UPSTREAM_HTTPS)
        if rc != 0:
            return [f"git failed for remote {c['name']}: {err}"], applied
        applied = True

    return [], applied


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo", help="Path to Adobe-Apis/agentgateway checkout")
    ap.add_argument(
        "--no-fetch",
        action="store_true",
        help="Skip `git fetch` — trust what's cached locally",
    )
    ap.add_argument(
        "--count",
        type=int,
        default=1,
        help="Batch size (default 1). When >1, the JSON includes a `batch` "
             "section with the first N unsynced commits and `batch_end_sha`. "
             "Capped at unsynced_count.",
    )
    ap.add_argument(
        "--fix-remotes",
        action="store_true",
        help="Apply fixable upstream/public remote layout before inspecting",
    )
    ap.add_argument(
        "--skip-public",
        action="store_true",
        help="With --fix-remotes: do not add or repair the public remote",
    )

    args = ap.parse_args()

    if args.count < 1:
        print(json.dumps({"errors": [f"--count must be >= 1, got {args.count}"]}, indent=2))
        return 1

    state: dict = {
        "repo": os.path.abspath(args.repo),
        "errors": [],
    }

    if not os.path.isdir(os.path.join(args.repo, ".git")):
        state["errors"].append(f"{args.repo} is not a git checkout")
        _emit(state)
        return 1

    if args.fix_remotes:
        fatal, applied_fix = fix_git_remotes(os.path.abspath(args.repo), args.skip_public)
        state["remotes_fixed_applied"] = applied_fix
        if fatal:
            state["errors"].extend(fatal)
            _emit(state)
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
    origin_url = state.get("remote_origin_url")
    
    inspect_self = _scripts / "inspect_state.py"
    if origin_url is not None and remotes_equivalent(origin_url, EXPECTED_UPSTREAM):
        state["errors"].append(
            "origin remote points at agentgateway/agentgateway (public upstream) — "
            f"expected Adobe-Apis/agentgateway. Fix manually: "
            f"`git -C {args.repo} remote set-url origin {EXPECTED_ORIGIN_ADOBE_HTTPS}`"
        )

    if upstream_url is None:
        state["errors"].append(
            "upstream remote is missing — run "
            f"`python3 {inspect_self} {args.repo} --fix-remotes` "
            f"or `git -C {args.repo} remote add upstream {EXPECTED_UPSTREAM}` "
            "(or `git remote set-url upstream …` if the name exists but points elsewhere)"
        )
    elif not remotes_equivalent(upstream_url, EXPECTED_UPSTREAM):
        state["errors"].append(
            f"upstream remote is {upstream_url!r}, "
            f"expected public {EXPECTED_UPSTREAM!r} (or SSH equivalent) — run "
            f"`python3 {inspect_self} {args.repo} --fix-remotes` "
            f"or `git -C {args.repo} remote set-url upstream {EXPECTED_UPSTREAM}`"
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

    upstream_ok = upstream_url is not None and remotes_equivalent(upstream_url, EXPECTED_UPSTREAM)

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
            "prev_upstream_sha": None,
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

            rc2, parent, _ = git(args.repo, "rev-parse", f"{oldest}^")
            state["prev_upstream_sha"] = parent if rc2 == 0 and parent else None

    if state["pr_num"]:
        rc, out, err = gh("api", f"repos/agentgateway/agentgateway/pulls/{state['pr_num']}", "--jq", ".head.ref")
        if rc == 0 and out:
            state["source_branch"] = out
        else:
            state["errors"].append(f"gh api failed for upstream PR #{state['pr_num']}: {err}")

    state["batch_requested"] = args.count
    state["batch_count"] = 0
    state["batch_end_sha"] = None
    state["batch_end_short_sha"] = None
    state["batch_commits"] = []
    state["batch_non_standard_commits"] = []
    state["batch_has_non_standard_merge"] = False

    if upstream_ok and state.get("unsynced_count"):
        effective_count = min(args.count, state["unsynced_count"])
        rc, out, _ = git(
            args.repo,
            "log",
            "--reverse",
            "--no-merges",
            "--pretty=%H%x09%h%x09%aI%x09%s",
            f"{base_ref}..upstream/main",
        )
        if rc == 0 and out:
            commits: list[dict] = []
            for line in out.splitlines()[:effective_count]:
                parts = line.split("\t", 3)
                if len(parts) != 4:
                    continue
                full_sha, short_sha, author_date, subject = parts
                m = re.search(r"\(#(\d+)\)\s*$", subject)
                pr_num = m.group(1) if m else None
                commits.append(
                    {
                        "sha": full_sha,
                        "short_sha": short_sha,
                        "author_date": author_date,
                        "subject": subject,
                        "pr_num": pr_num,
                    }
                )
            state["batch_count"] = len(commits)
            state["batch_commits"] = commits
            if commits:
                state["batch_end_sha"] = commits[-1]["sha"]
                state["batch_end_short_sha"] = commits[-1]["short_sha"]

            # Non-standard merge detection: a standard upstream squash-merge
            # ends in (#NNNN). A commit lacking it means upstream did a
            # non-squash / rebase merge — Shape D's verbatim-SHA assumptions
            # are shakier and the batch should be reviewed before landing.
            non_standard = [
                {"sha": c["sha"], "short_sha": c["short_sha"], "subject": c["subject"]}
                for c in commits
                if c["pr_num"] is None
            ]
            state["batch_non_standard_commits"] = non_standard
            state["batch_has_non_standard_merge"] = bool(non_standard)

    rc, out, err = gh("api", "repos/Adobe-Apis/agentgateway", "--jq", ".full_name")
    state["adobe_apis_reachable"] = (rc == 0 and out == "Adobe-Apis/agentgateway")
    if not state["adobe_apis_reachable"]:
        state["errors"].append(
            "Adobe-Apis/agentgateway not reachable via gh — most likely SSO "
            f"authorisation is missing. gh stderr: {err}"
        )

    _emit(state)
    return 0


def _emit(data: dict) -> None:
    print(json.dumps(data, indent=2))


if __name__ == "__main__":
    sys.exit(main())
