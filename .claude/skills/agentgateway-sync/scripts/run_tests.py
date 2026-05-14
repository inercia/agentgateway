#!/usr/bin/env python3
"""Run `make test` in the agentgateway repo and emit parsed results.

Usage:
    python3 run_tests.py <repo_path> <label> [--log-dir <dir>] [--output <file>]

`<label>` is used to name the log file: <log_dir>/agw_make_test_<label>.log.
Typical labels: `baseline` (before rebase), `synced` (after rebase).
`--log-dir` defaults to /tmp; pass the project's tmp/ dir to keep logs local.

Emits a single JSON object to stdout with aggregate test counts from
cargo test output plus a verdict. The skill compares baseline vs synced
JSON to decide whether to stop (mismatched counts or new warnings).

Exit status:
    0 — tests ran (check `all_passed` field in JSON for pass/fail)
    1 — couldn't run tests at all (bad path, make not found, I/O error)

Why a script: `make test` is slow (minutes) and the natural invocation
(`(cd "$REPO" && make test) > log 2>&1`) uses subshell + redirect
operators that trigger Claude Code's "shell operators require approval"
prompt every run. A single-script entry point collapses that into one
approval rule.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys


COUNT_RE = re.compile(
    r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored"
)
WARNING_RE = re.compile(r"^warning:", re.MULTILINE)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo")
    ap.add_argument("label", help="log file suffix, e.g. baseline or synced")
    ap.add_argument("--log-dir", default="/tmp", help="directory for log files")
    ap.add_argument("--output", default=None, help="write JSON result to this file instead of stdout")
    args = ap.parse_args()

    if not os.path.isdir(args.repo):
        print(json.dumps({"errors": [f"{args.repo} does not exist"]}, indent=2))
        return 1

    os.makedirs(args.log_dir, exist_ok=True)
    log_path = os.path.join(args.log_dir, f"agw_make_test_{args.label}.log")

    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}

    try:
        with open(log_path, "wb") as log:
            proc = subprocess.run(
                ["make", "test"],
                cwd=args.repo,
                stdout=log,
                stderr=subprocess.STDOUT,
                env=env,
            )
    except FileNotFoundError:
        print(json.dumps({"errors": ["make: command not found"]}, indent=2))
        return 1
    except OSError as e:
        print(json.dumps({"errors": [f"failed to run make test: {e}"]}, indent=2))
        return 1

    try:
        with open(log_path) as fh:
            content = fh.read()
    except OSError as e:
        print(json.dumps({"errors": [f"could not read {log_path}: {e}"]}, indent=2))
        return 1

    passed = failed = ignored = 0
    suites_ok = suites_failed = 0
    for m in COUNT_RE.finditer(content):
        verdict = m.group(1)
        passed += int(m.group(2))
        failed += int(m.group(3))
        ignored += int(m.group(4))
        if verdict == "ok":
            suites_ok += 1
        else:
            suites_failed += 1

    warnings = len(WARNING_RE.findall(content))
    all_passed = (proc.returncode == 0 and failed == 0 and suites_failed == 0)

    result = {
        "label": args.label,
        "log_path": log_path,
        "returncode": proc.returncode,
        "suites_ok": suites_ok,
        "suites_failed": suites_failed,
        "passed": passed,
        "failed": failed,
        "ignored": ignored,
        "warnings": warnings,
        "all_passed": all_passed,
    }

    if not all_passed:
        result["tail"] = content.splitlines()[-30:]

    output = json.dumps(result, indent=2)
    if args.output:
        os.makedirs(os.path.dirname(args.output), exist_ok=True)
        with open(args.output, "w") as fh:
            fh.write(output + "\n")
    else:
        print(output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
