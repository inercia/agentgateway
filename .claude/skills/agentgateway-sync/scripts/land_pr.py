#!/usr/bin/env python3
"""Force-update Adobe fork branch `adobe` to a sync PR head, then finalize PR state.

Used by batch+yolo auto-land, auto-resolve auto-land, and manual `/land` flows.

Important (REST PATCH refs/heads/adobe): pass force as JSON boolean via gh's -F:
    gh api ... -f sha=<sha> -F force=true
Lowercase -f force=true sends the string "true" and is ignored by the API.

Usage:
    python3 land_pr.py <repo> --pr <N> --head-sha <full_sha> --expected-adobe <full_sha>
        [--reason auto-batch|auto-resolve|manual-land]
        [--comment-body \"...\"] [--skip-comment] [--skip-close] [--skip-local-reset]

Emits JSON on stdout:
    landed, pr_state_after, comment_posted, pr_closed, local_adobe_updated, errors[]
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys


def run(cmd: list[str]) -> tuple[int, str, str]:
    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}
    p = subprocess.run(cmd, capture_output=True, text=True, env=env)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def gh_api(args: list[str]) -> tuple[int, str, str]:
    return run(["gh", "api", *args])


def gh_patch_adobe_ref_cmd(head_sha: str) -> list[str]:
    """CLI tokens for `gh api PATCH …/refs/heads/adobe` with JSON boolean force."""
    return [
        "gh",
        "api",
        "-X",
        "PATCH",
        "repos/Adobe-Apis/agentgateway/git/refs/heads/adobe",
        "-f",
        f"sha={head_sha}",
        "-F",
        "force=true",
    ]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo", help="Path to Adobe-Apis/agentgateway checkout")
    ap.add_argument("--pr", type=int, required=True)
    ap.add_argument("--head-sha", required=True, help="Full SHA of sync branch tip / PR head")
    ap.add_argument("--expected-adobe", required=True, help="Freshness: origin adobe SHA when PR was created")
    ap.add_argument(
        "--reason",
        choices=("auto-batch", "auto-resolve", "manual-land"),
        default="manual-land",
    )
    ap.add_argument("--comment-body", default=None, help="Override default landing comment (markdown)")
    ap.add_argument("--skip-comment", action="store_true")
    ap.add_argument("--skip-close", action="store_true", help="Do not gh pr close when still open")
    ap.add_argument("--skip-local-reset", action="store_true", help="Skip fetch + update-ref for refs/heads/adobe")
    args = ap.parse_args()

    repo = os.path.abspath(args.repo)
    pr = args.pr
    head_sha = args.head_sha.lower()
    expected = args.expected_adobe.lower()

    out: dict = {
        "landed": False,
        "pr_state_after": None,
        "comment_posted": False,
        "pr_closed": False,
        "local_adobe_updated": False,
        "errors": [],
    }

    # Freshness
    rc, current_adobe, err = gh_api(
        ["repos/Adobe-Apis/agentgateway/git/refs/heads/adobe", "--jq", ".object.sha"]
    )
    if rc != 0:
        out["errors"].append(f"could not read refs/heads/adobe: {err}")
        print(json.dumps(out, indent=2))
        return 1

    current_adobe = current_adobe.lower()
    if current_adobe != expected:
        out["errors"].append(
            f"freshness mismatch: refs/heads/adobe is {current_adobe}, "
            f"expected PR marker {expected}"
        )
        print(json.dumps(out, indent=2))
        return 1

    # Force-update adobe (-F force=true is JSON boolean; never use -f force=true)
    rc, _, err = run(gh_patch_adobe_ref_cmd(args.head_sha))
    if rc != 0:
        out["errors"].append(f"PATCH refs/heads/adobe failed: {err}")
        print(json.dumps(out, indent=2))
        return 1

    out["landed"] = True

    # Landing comment
    if not args.skip_comment:
        if args.comment_body:
            body = args.comment_body
        elif args.reason == "auto-batch":
            body = (
                f"Auto-landed as {head_sha} on `adobe` (clean rebase, tests passed).\n"
                f"No /land required — batch+yolo flow.\n"
                f"See https://github.com/Adobe-Apis/agentgateway/commit/{head_sha}"
            )
        elif args.reason == "auto-resolve":
            body = (
                f"Auto-landed as {head_sha} on `adobe` (auto-resolved superficial conflicts, tests passed).\n"
                f"No /land required — auto-resolve flow.\n"
                f"See https://github.com/Adobe-Apis/agentgateway/commit/{head_sha}"
            )
        else:
            body = (
                f"Landed as `{head_sha}` on `adobe` via force-push. "
                f"See https://github.com/Adobe-Apis/agentgateway/commit/{head_sha}"
            )

        rc, _, cerr = run(
            [
                "gh",
                "pr",
                "comment",
                str(pr),
                "--repo",
                "Adobe-Apis/agentgateway",
                "--body",
                body,
            ]
        )
        if rc != 0:
            out["errors"].append(f"gh pr comment failed: {cerr}")
        else:
            out["comment_posted"] = True

    # PR state after land
    rc, state_json, serr = run(
        [
            "gh",
            "pr",
            "view",
            str(pr),
            "--repo",
            "Adobe-Apis/agentgateway",
            "--json",
            "state",
        ]
    )
    if rc != 0:
        out["errors"].append(f"gh pr view failed: {serr}")
        print(json.dumps(out, indent=2))
        return 0 if out["landed"] else 1

    try:
        state_obj = json.loads(state_json)
        state = (state_obj.get("state") or "").upper()
    except json.JSONDecodeError:
        state = ""
    out["pr_state_after"] = state or None

    if not args.skip_close and state == "OPEN":
        rc, _, cerr = run(
            ["gh", "pr", "close", str(pr), "--repo", "Adobe-Apis/agentgateway"]
        )
        if rc != 0:
            out["errors"].append(f"gh pr close failed: {cerr}")
        else:
            out["pr_closed"] = True

    if not args.skip_local_reset:
        rc, _, ferr = run(["git", "-C", repo, "fetch", "origin", "adobe"])
        if rc != 0:
            out["errors"].append(f"git fetch origin adobe failed: {ferr}")
        else:
            rc2, _, uerr = run(
                [
                    "git",
                    "-C",
                    repo,
                    "update-ref",
                    "refs/heads/adobe",
                    "refs/remotes/origin/adobe",
                ]
            )
            if rc2 != 0:
                out["errors"].append(f"git update-ref adobe failed: {uerr}")
            else:
                out["local_adobe_updated"] = True

    print(json.dumps(out, indent=2))
    # Landed but non-fatal follow-up issues (comment/close/local reset) still exit 0.
    return 0 if out["landed"] else 1


if __name__ == "__main__":
    sys.exit(main())
