#!/usr/bin/env python3
"""Idempotent git remote layout for Adobe-Apis/agentgateway sync workflow.

Usage:
    python3 ensure_git_remotes.py <repo_path> [--apply] [--skip-public]

Dry-run (default): prints JSON with remotes_ok, checks, errors.
--apply: runs git remote add / set-url for fixable issues only.

Exit status:
    0 — completed (dry-run or successful apply); parse JSON for remotes_ok / errors.
    1 — not a git checkout, or a git command failed during --apply.
"""

from __future__ import annotations

import argparse
import json
import os
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
)

_ORIGIN_CANON = canonical_origin_adobe()
_UPSTREAM_CANON = canonical_upstream()


def run_git(repo: str, *args: str) -> tuple[int, str, str]:
    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}
    p = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, env=env)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def get_remote_url(repo: str, name: str) -> str | None:
    rc, out, _ = run_git(repo, "remote", "get-url", name)
    return out if rc == 0 else None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo", help="Path to Adobe-Apis/agentgateway checkout")
    ap.add_argument(
        "--apply",
        action="store_true",
        help="Apply fixable remote add/set-url operations",
    )
    ap.add_argument(
        "--skip-public",
        action="store_true",
        help="Do not add or fix the public remote",
    )
    args = ap.parse_args()

    repo = str(Path(args.repo).resolve())
    errors: list[str] = []
    checks: list[dict] = []
    applied = False

    if not Path(repo, ".git").is_dir():
        print(
            json.dumps(
                {
                    "remotes_ok": False,
                    "checks": [],
                    "errors": [f"{repo} is not a git checkout"],
                    "applied": False,
                },
                indent=2,
            )
        )
        return 1

    # --- origin (Adobe fork): not auto-fixable if wrong ---
    origin_raw = get_remote_url(repo, "origin")
    origin_norm = normalize_github_remote(origin_raw) if origin_raw else None
    origin_ok = origin_norm == _ORIGIN_CANON
    if origin_raw is None:
        errors.append(
            "wrong_origin: origin remote is missing — clone Adobe-Apis/agentgateway "
            f"(or `git -C {repo} remote add origin {EXPECTED_ORIGIN_ADOBE_HTTPS}`)"
        )
        checks.append(
            {
                "name": "origin",
                "ok": False,
                "current": None,
                "expected": EXPECTED_ORIGIN_ADOBE_HTTPS,
                "fix": None,
            }
        )
    elif origin_norm == _UPSTREAM_CANON:
        errors.append(
            "wrong_origin: origin points at public agentgateway/agentgateway — "
            f"expected Adobe-Apis/agentgateway. Clone the fork or "
            f"`git -C {repo} remote set-url origin {EXPECTED_ORIGIN_ADOBE_HTTPS}`"
        )
        checks.append(
            {
                "name": "origin",
                "ok": False,
                "current": origin_raw,
                "expected": EXPECTED_ORIGIN_ADOBE_HTTPS,
                "fix": None,
            }
        )
    elif not origin_ok:
        errors.append(
            f"wrong_origin: origin is {origin_raw!r} — expected Adobe-Apis/agentgateway "
            "(HTTPS or git@github.com:Adobe-Apis/agentgateway.git). Fix manually."
        )
        checks.append(
            {
                "name": "origin",
                "ok": False,
                "current": origin_raw,
                "expected": EXPECTED_ORIGIN_ADOBE_HTTPS,
                "fix": None,
            }
        )
    else:
        checks.append(
            {
                "name": "origin",
                "ok": True,
                "current": origin_raw,
                "expected": EXPECTED_ORIGIN_ADOBE_HTTPS,
                "fix": None,
            }
        )

    # --- upstream ---
    upstream_raw = get_remote_url(repo, "upstream")
    upstream_norm = normalize_github_remote(upstream_raw) if upstream_raw else None
    upstream_ok = upstream_norm == _UPSTREAM_CANON
    if upstream_raw is None:
        checks.append(
            {
                "name": "upstream",
                "ok": False,
                "current": None,
                "expected": EXPECTED_UPSTREAM_HTTPS,
                "fix": f"git -C {repo} remote add upstream {EXPECTED_UPSTREAM_HTTPS}",
            }
        )
    elif not upstream_ok:
        checks.append(
            {
                "name": "upstream",
                "ok": False,
                "current": upstream_raw,
                "expected": EXPECTED_UPSTREAM_HTTPS,
                "fix": f"git -C {repo} remote set-url upstream {EXPECTED_UPSTREAM_HTTPS}",
            }
        )
    else:
        checks.append(
            {
                "name": "upstream",
                "ok": True,
                "current": upstream_raw,
                "expected": EXPECTED_UPSTREAM_HTTPS,
                "fix": None,
            }
        )

    # --- public (optional alias for upstream OSS URL) ---
    if not args.skip_public:
        public_raw = get_remote_url(repo, "public")
        public_norm = normalize_github_remote(public_raw) if public_raw else None
        public_ok = public_norm == _UPSTREAM_CANON
        if public_raw is None:
            checks.append(
                {
                    "name": "public",
                    "ok": False,
                    "current": None,
                    "expected": EXPECTED_UPSTREAM_HTTPS,
                    "fix": f"git -C {repo} remote add public {EXPECTED_UPSTREAM_HTTPS}",
                }
            )
        elif not public_ok:
            checks.append(
                {
                    "name": "public",
                    "ok": False,
                    "current": public_raw,
                    "expected": EXPECTED_UPSTREAM_HTTPS,
                    "fix": f"git -C {repo} remote set-url public {EXPECTED_UPSTREAM_HTTPS}",
                }
            )
        else:
            checks.append(
                {
                    "name": "public",
                    "ok": True,
                    "current": public_raw,
                    "expected": EXPECTED_UPSTREAM_HTTPS,
                    "fix": None,
                }
            )

    remotes_ok = all(c["ok"] for c in checks) and not errors

    def _apply_remote(name: str, had_url: str | None) -> str | None:
        nonlocal applied
        if had_url is None:
            rc, _, err = run_git(repo, "remote", "add", name, EXPECTED_UPSTREAM_HTTPS)
        else:
            rc, _, err = run_git(repo, "remote", "set-url", name, EXPECTED_UPSTREAM_HTTPS)
        if rc != 0:
            return err
        applied = True
        return None

    if args.apply and not errors:
        for c in checks:
            if c["ok"] or not c.get("fix"):
                continue
            if c["name"] not in ("upstream", "public"):
                continue
            if c["name"] == "public" and args.skip_public:
                continue
            err = _apply_remote(c["name"], c["current"])
            if err:
                print(
                    json.dumps(
                        {
                            "remotes_ok": False,
                            "checks": checks,
                            "errors": errors + [f"git failed for {c['name']}: {err}"],
                            "applied": applied,
                        },
                        indent=2,
                    )
                )
                return 1
            c["ok"] = True
            c["current"] = EXPECTED_UPSTREAM_HTTPS
            c["fix"] = None

        remotes_ok = all(c["ok"] for c in checks) and not errors

    out = {
        "remotes_ok": remotes_ok,
        "checks": checks,
        "errors": errors,
        "applied": applied,
    }
    print(json.dumps(out, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
