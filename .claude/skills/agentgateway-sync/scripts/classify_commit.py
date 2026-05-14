#!/usr/bin/env python3
"""Classify a single upstream commit for Adobe agentgateway sync risk.

Labels (exactly one): MERGE_SAFE, NEEDS_REVIEW, SECURITY, SKIP
Risk: critical, high, medium, low

Protected paths: jwt.rs, mcp/sse.rs, adobe/
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

PROTECTED_PATHS = ("jwt.rs", "mcp/sse.rs", "adobe/")

PROTECTED_PATTERNS: list[tuple[str, re.Pattern[str]]] = [
    ("path_jwt_rs", re.compile(r"(?:^|/)jwt\.rs$")),
    ("path_mcp_sse_rs", re.compile(r"(?:^|/)mcp/sse\.rs$")),
    ("path_adobe_tree", re.compile(r"^adobe/")),
]

PATTERNS_CRITICAL: list[tuple[str, re.Pattern[str]]] = [
    ("jwt_remove_exp_handling", re.compile(r"remove.*\bexp\b|strip.*\bexp\b|delete.*\bexp\b", re.I)),
    ("jwt_claim_as_millis", re.compile(r"claim_as_millis", re.I)),
    ("jwt_token_error_expired", re.compile(r"TokenError::Expired", re.I)),
    ("sse_header_session_id", re.compile(r"HEADER_SESSION_ID", re.I)),
    ("sse_mcp_session", re.compile(r"mcp-session-id|mcp_session", re.I)),
]

PATTERNS_HIGH: list[tuple[str, re.Pattern[str]]] = [
    ("jwt_jsonwebtoken", re.compile(r"\bjsonwebtoken\b", re.I)),
    ("ims_marker", re.compile(r"\bIMS\b", re.I)),
    ("sse_event_stream", re.compile(r"text/event-stream", re.I)),
]

RISK_ORDER = ["low", "medium", "high", "critical"]


def _run_git(repo: str, *git_args: str) -> tuple[int, str, str]:
    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}
    p = subprocess.run(["git", "-C", repo, *git_args], capture_output=True, text=True, env=env)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def _files_for_commit(repo: str, sha: str) -> list[str]:
    rc, out, _ = _run_git(repo, "diff-tree", "--no-commit-id", "--name-only", "-r", sha)
    if rc != 0:
        return []
    return [ln.strip() for ln in out.splitlines() if ln.strip()]


def _patch_for_commit(repo: str, sha: str) -> str:
    rc, out, _ = _run_git(repo, "show", "--pretty=format:", "-p", sha)
    return out if rc == 0 else ""


def _parent_count(repo: str, sha: str) -> int:
    rc, out, _ = _run_git(repo, "rev-list", "--parents", "-n", "1", sha)
    if rc != 0 or not out:
        return 1
    parts = out.split()
    return max(0, len(parts) - 1)


def _max_risk(a: str, b: str) -> str:
    return a if RISK_ORDER.index(a) >= RISK_ORDER.index(b) else b


def _path_protected(norm_path: str) -> tuple[bool, list[str]]:
    hits: list[str] = []
    for label, rx in PROTECTED_PATTERNS:
        if rx.search(norm_path):
            hits.append(label)
    return (bool(hits), hits)


def classify(files: list[str], patch: str, merge_parents: int) -> dict[str, Any]:
    if merge_parents > 1:
        return {
            "label": "SKIP",
            "risk": "low",
            "reason": "Merge commit (multiple parents) — classification skipped for stacked single-commit syncs.",
            "files": files,
            "protected_hits": [],
            "pattern_hits": [],
            "merge_parents": merge_parents,
        }

    if not files:
        return {
            "label": "SKIP",
            "risk": "low",
            "reason": "No files changed in commit.",
            "files": [],
            "protected_hits": [],
            "pattern_hits": [],
            "merge_parents": merge_parents,
        }

    protected_files: list[str] = []
    path_tags: list[str] = []
    for f in files:
        n = f.replace(chr(92), "/")
        ok, tags = _path_protected(n)
        if ok:
            protected_files.append(f)
            path_tags.extend(tags)

    critical_hits: list[str] = []
    high_hits: list[str] = []
    risk = "low"
    for name, rx in PATTERNS_CRITICAL:
        if rx.search(patch):
            critical_hits.append(name)
            risk = "critical"
    if risk != "critical":
        for name, rx in PATTERNS_HIGH:
            if rx.search(patch):
                high_hits.append(name)
                risk = _max_risk(risk, "high")

    pattern_hits = critical_hits + high_hits

    wf_hits = [f for f in files if f.replace(chr(92), "/").startswith(".github/workflows/")]
    large_touch = len(files) >= 30

    label = "MERGE_SAFE"
    reasons: list[str] = []

    if critical_hits or protected_files:
        label = "SECURITY"
        if protected_files:
            reasons.append("Touches protected paths: " + ", ".join(protected_files))
        if path_tags:
            reasons.append("Protected path patterns: " + ", ".join(sorted(set(path_tags))))
        if critical_hits:
            reasons.append("Critical diff patterns: " + ", ".join(critical_hits))
        if high_hits:
            reasons.append("High-severity diff patterns: " + ", ".join(high_hits))
        if protected_files and not critical_hits and not high_hits:
            risk = _max_risk(risk, "medium")
    elif high_hits:
        label = "NEEDS_REVIEW"
        reasons.append("High-severity diff patterns: " + ", ".join(high_hits))
    elif wf_hits or large_touch:
        label = "NEEDS_REVIEW"
        if wf_hits:
            reasons.append("Touches GitHub Actions workflows — review for workflow scope token issues.")
        if large_touch:
            reasons.append(f"Large change set ({len(files)} files) — manual review recommended.")
    else:
        reasons.append("No protected paths or elevated patterns; routine upstream commit.")

    return {
        "label": label,
        "risk": risk,
        "reason": "; ".join(reasons),
        "files": files,
        "protected_hits": protected_files,
        "pattern_hits": pattern_hits,
        "workflows": wf_hits,
        "merge_parents": merge_parents,
        "protected_paths_decl": list(PROTECTED_PATHS),
    }


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: classify_commit.py <repo> <sha>", file=sys.stderr)
        return 2
    repo = str(Path(sys.argv[1]).resolve())
    sha = sys.argv[2]
    parents = _parent_count(repo, sha)
    files = _files_for_commit(repo, sha)
    patch = _patch_for_commit(repo, sha)
    out = classify(files, patch, parents)
    print(json.dumps(out, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
