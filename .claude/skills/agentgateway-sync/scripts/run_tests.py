#!/usr/bin/env python3
"""Run `make test` in the agentgateway repo and emit parsed results.

Usage:
    python3 run_tests.py <repo_path> <label> [--log-dir <dir>] [--output <file>] [--retry-once]
        [--cache-file <file> --cache-key <sha>]

`<label>` is used to name the log file: <log_dir>/agw_make_test_<label>.log.
Typical labels: `baseline` (before rebase), `synced` (after rebase).
`--log-dir` defaults to /tmp; pass the project's tmp/ dir to keep logs local.

`--retry-once`: if the first run does not satisfy `all_passed`, runs a second
`make test` with log suffix `<label>-retry` and emits combined fields:
`needed_retry`, `retry_passed`, `retry_label`, and top-level counts from the
final attempt (retry when present, else first).

`--cache-file` + `--cache-key`: skip `make test` entirely when a previous
passing run is cached under the same key. The key is a content identity for
the tree under test (the skill passes `git rev-parse adobe` for `baseline`
and `git rev-parse HEAD` for `synced`). Across batch iterations the synced
tip of iteration N becomes `adobe` after landing, so iteration N+1's baseline
key matches the synced cache written by iteration N — eliminating one full
`make test` per batch. A cache HIT emits the stored result with
`"cached": true`; any miss runs normally and, on success (`all_passed`),
rewrites the cache `{key, result}`. Only passing runs are ever cached.

Exit status:
    0 — tests ran or cache hit (check `all_passed` field in JSON for pass/fail)
    1 — couldn't run tests at all (bad path, make not found, I/O error)
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from typing import Any


COUNT_RE = re.compile(
    r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored"
)
WARNING_RE = re.compile(r"^warning:", re.MULTILINE)


def _cache_read(cache_file: str, cache_key: str) -> dict[str, Any] | None:
    """Return a cached passing result for `cache_key`, or None on any miss."""
    try:
        with open(cache_file, encoding="utf-8") as fh:
            cache = json.load(fh)
    except (OSError, json.JSONDecodeError):
        return None
    if not isinstance(cache, dict):
        return None
    if cache.get("key") != cache_key:
        return None
    result = cache.get("result")
    if not isinstance(result, dict) or not result.get("all_passed"):
        return None
    return result


def _cache_write(cache_file: str, cache_key: str, result: dict[str, Any]) -> None:
    """Persist a passing result under `cache_key`. Best-effort; never raises."""
    if not result.get("all_passed"):
        return
    try:
        cd = os.path.dirname(cache_file)
        if cd:
            os.makedirs(cd, exist_ok=True)
        with open(cache_file, "w", encoding="utf-8") as fh:
            json.dump({"key": cache_key, "result": result}, fh, indent=2)
    except OSError:
        pass


def _run_once(repo: str, label: str, log_dir: str, env: dict[str, str]) -> dict[str, Any]:
    os.makedirs(log_dir, exist_ok=True)
    log_path = os.path.join(log_dir, f"agw_make_test_{label}.log")

    try:
        with open(log_path, "wb") as log:
            proc = subprocess.run(
                ["make", "test"],
                cwd=repo,
                stdout=log,
                stderr=subprocess.STDOUT,
                env=env,
            )
    except FileNotFoundError:
        return {"errors": ["make: command not found"]}
    except OSError as e:
        return {"errors": [f"failed to run make test: {e}"]}

    try:
        with open(log_path, encoding="utf-8") as fh:
            content = fh.read()
    except OSError as e:
        return {"errors": [f"could not read {log_path}: {e}"]}

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
    all_passed = proc.returncode == 0 and failed == 0 and suites_failed == 0

    result: dict[str, Any] = {
        "label": label,
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

    return result


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo")
    ap.add_argument("label", help="log file suffix, e.g. baseline or synced")
    ap.add_argument("--log-dir", default="/tmp", help="directory for log files")
    ap.add_argument("--output", default=None, help="write JSON result to this file instead of stdout")
    ap.add_argument(
        "--retry-once",
        action="store_true",
        help="If first run fails all_passed, run once more with <label>-retry log",
    )
    ap.add_argument("--cache-file", default=None, help="JSON cache of the last passing run")
    ap.add_argument(
        "--cache-key",
        default=None,
        help="Content identity for the tree under test (e.g. git rev-parse adobe/HEAD)",
    )
    args = ap.parse_args()

    if not os.path.isdir(args.repo):
        print(json.dumps({"errors": [f"{args.repo} does not exist"]}, indent=2))
        return 1

    cache_enabled = bool(args.cache_file and args.cache_key)

    if cache_enabled:
        hit = _cache_read(args.cache_file, args.cache_key)
        if hit is not None:
            result = dict(hit)
            result["cached"] = True
            result["cache_key"] = args.cache_key
            output = json.dumps(result, indent=2)
            if args.output:
                od = os.path.dirname(args.output)
                if od:
                    os.makedirs(od, exist_ok=True)
                with open(args.output, "w", encoding="utf-8") as fh:
                    fh.write(output + "\n")
            else:
                print(output)
            return 0

    env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_TOKEN", "GH_TOKEN")}

    first = _run_once(args.repo, args.label, args.log_dir, env)
    if "errors" in first:
        print(json.dumps(first, indent=2))
        return 1

    needed_retry = False
    retry_passed: bool | None = None
    retry_label: str | None = None
    second: dict[str, Any] | None = None

    if args.retry_once and not first["all_passed"]:
        needed_retry = True
        retry_label = f"{args.label}-retry"
        second = _run_once(args.repo, retry_label, args.log_dir, env)
        if "errors" in second:
            out = dict(first)
            out["needed_retry"] = True
            out["retry_passed"] = None
            out["retry_label"] = retry_label
            out["retry_errors"] = second["errors"]
            print(json.dumps(out, indent=2))
            return 1
        retry_passed = bool(second["all_passed"])
        final = second
    else:
        final = first

    result = dict(final)
    result["needed_retry"] = needed_retry
    result["retry_passed"] = retry_passed
    result["retry_label"] = retry_label
    result["cached"] = False
    if needed_retry and second is not None:
        result["first_attempt"] = {
            k: first[k]
            for k in (
                "label",
                "log_path",
                "returncode",
                "suites_ok",
                "suites_failed",
                "passed",
                "failed",
                "ignored",
                "warnings",
                "all_passed",
            )
        }
        if not first["all_passed"] and "tail" in first:
            result["first_attempt"]["tail"] = first["tail"]

    if cache_enabled:
        _cache_write(args.cache_file, args.cache_key, result)

    output = json.dumps(result, indent=2)
    if args.output:
        od = os.path.dirname(args.output)
        if od:
            os.makedirs(od, exist_ok=True)
        with open(args.output, "w", encoding="utf-8") as fh:
            fh.write(output + "\n")
    else:
        print(output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
