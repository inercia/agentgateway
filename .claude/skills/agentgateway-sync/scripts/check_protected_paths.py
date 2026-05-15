#!/usr/bin/env python3
"""Return whether any paths hit Adobe protected-path patterns.

Uses the same regexes as classify_commit.PROTECTED_PATTERNS.

Usage:
    python3 check_protected_paths.py <path> [<path> ...]

Emits JSON: {\"is_protected\": bool, \"hits\": [{\"path\", \"pattern_label\"}, ...]}
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

_scripts = Path(__file__).resolve().parent
if str(_scripts) not in sys.path:
    sys.path.insert(0, str(_scripts))

from classify_commit import PROTECTED_PATTERNS  # noqa: E402


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: check_protected_paths.py <path> [<path> ...]", file=sys.stderr)
        return 2

    hits: list[dict[str, str]] = []
    for raw in sys.argv[1:]:
        norm = raw.replace("\\", "/")
        for label, rx in PROTECTED_PATTERNS:
            if rx.search(norm):
                hits.append({"path": raw, "pattern_label": label})

    out = {"is_protected": bool(hits), "hits": hits}
    print(json.dumps(out, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
