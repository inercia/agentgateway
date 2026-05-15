#!/usr/bin/env python3
"""Classify a batch of upstream commits for the agentgateway sync skill.

Usage:
    python3 classify_batch.py <repo> <sha1> [<sha2> ...]

Calls into ``classify_commit.classify`` for each SHA and emits a single
JSON object with:

    - ``commits``: per-SHA classification (same shape as classify_commit)
    - ``aggregate``: worst-risk + counts so the skill can decide whether
      to surface a banner in the PR body. **Aggregate is informational
      only** — the batch+yolo flow does not gate on it. The conflict /
      test gates are the actual gates.

Risk ranking: critical > high > medium > low. The aggregate's risk is
the worst seen across the batch.

Why a separate script: keeps the existing ``classify_commit.py`` single
responsibility (one SHA in, one classification out) and avoids a giant
JSON write inside the Bash tool when the skill needs to classify, say,
50 commits — one subprocess call covers the whole batch.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

_scripts = Path(__file__).resolve().parent
if str(_scripts) not in sys.path:
    sys.path.insert(0, str(_scripts))

from classify_commit import (  # noqa: E402
    RISK_ORDER,
    _files_for_commit,
    _parent_count,
    _patch_for_commit,
    classify,
)


def _max_risk(a: str, b: str) -> str:
    return a if RISK_ORDER.index(a) >= RISK_ORDER.index(b) else b


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: classify_batch.py <repo> <sha> [<sha> ...]", file=sys.stderr)
        return 2

    repo = str(Path(sys.argv[1]).resolve())
    shas = sys.argv[2:]

    if not os.path.isdir(os.path.join(repo, ".git")):
        print(json.dumps({"errors": [f"{repo} is not a git checkout"]}, indent=2))
        return 1

    commits: list[dict] = []
    label_counts: dict[str, int] = {}
    worst_risk = "low"
    any_protected = False
    any_workflow = False

    for sha in shas:
        parents = _parent_count(repo, sha)
        files = _files_for_commit(repo, sha)
        patch = _patch_for_commit(repo, sha)
        result = classify(files, patch, parents)
        result["sha"] = sha
        commits.append(result)

        label_counts[result["label"]] = label_counts.get(result["label"], 0) + 1
        worst_risk = _max_risk(worst_risk, result["risk"])
        if result.get("protected_hits"):
            any_protected = True
        if result.get("workflows"):
            any_workflow = True

    aggregate = {
        "count": len(commits),
        "worst_risk": worst_risk,
        "label_counts": label_counts,
        "any_protected_path": any_protected,
        "any_workflow_change": any_workflow,
    }

    print(json.dumps({"aggregate": aggregate, "commits": commits}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
