#!/usr/bin/env python3
"""Compose markdown PR bodies for agentgateway-sync (single, batch, auto-resolve).

Reads structured JSON (--json-file) or merged machine inputs (--merge) and writes
markdown (--out). Use `kind` from CLI or JSON `"kind"` field.

Example (manual JSON — fallback):
    python3 compose_pr_body.py --kind batch --json-file body.json --out agw_pr_body.md

Preferred path (--merge joins inspect_state + classify_batch + run_tests):
    python3 compose_pr_body.py --kind batch --merge \\
      --inspect-state inspect.json --classify-batch classify.json \\
      --baseline-tests baseline.json --post-tests post.json \\
      --adobe-at-creation "$(git rev-parse origin/adobe)" --out agw_pr_body.md

Before hand-authoring JSON, print a minimal skeleton (fill via --merge or manually):

    python3 compose_pr_body.py --print-template batch

The script validates required keys and nested `tests` shape before rendering;
on failure it prints `{"ok": false, "errors": [...]}` to stderr and exits 2.

JSON shapes (for --json-file only; see module docstring in classify_batch / inspect_state):

**single** — adobe_at_creation, oldest_sha, pr_num, oldest_subject,
  classification: {label, risk, reason, highlights?},
  tests: {baseline: counts, post: counts}, retry_note, supersedes, changelog_section

**batch** — adobe_at_creation, batch_count, batch_end_sha, oldest_sha,
  commits: [{sha, short_sha, pr_num|null, subject, files_count, label, risk}],
  show_banner, banner_items, tests, retry_note, tests_fail_tail

**auto-resolve** — adobe_at_creation, conflicting_sha, conflicting_short_sha,
  conflicting_pr_num|null, subject, clean_count, prefix_pr_num, resolution_rows,
  tests_baseline, tests_post, retry_note, tests_failing_pr, tests_fail_tail
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

COUNT_KEYS = ("passed", "failed", "ignored", "warnings", "suites_ok")
_PLACEHOLDER_SHA = "0" * 40

PLACEHOLDER_STRINGS = frozenset(
    {
        "REPLACE_ROW_SUBJECT",
        "REPLACE_WITH_SUBJECT",
        "REPLACE_SUBJECT",
        "REPLACE_WITH_UPSTREAM_PR_NUM",
    }
)

SENTINEL_SHORT_SHA = "0" * 7


def _blank_counts() -> dict[str, int]:
    return {k: 0 for k in COUNT_KEYS}


def _counts_dict_errors(path: str, d: Any) -> list[str]:
    errs: list[str] = []
    if not isinstance(d, dict):
        errs.append(f"{path}: expected an object with keys {COUNT_KEYS}, got {type(d).__name__}")
        return errs
    missing = [k for k in COUNT_KEYS if k not in d]
    if missing:
        errs.append(f"{path}: missing keys {missing}")
    return errs


def _full_sha_field(name: str, val: Any) -> list[str]:
    errs: list[str] = []
    if not isinstance(val, str) or not val.strip():
        errs.append(
            f"missing or empty {name!r} — use full 40-char hex from "
            '`git -C "$REPO" rev-parse origin/adobe` or `inspect_state.py` JSON '
            f"(e.g. batch_end_sha, oldest_sha), not a short SHA alone"
        )
        return errs
    v = val.strip().lower()
    if len(v) < 40:
        errs.append(
            f"{name!r} is only {len(v)} chars — compose_pr_body needs the full SHA "
            "(from `git rev-parse` or inspect_state `batch_end_sha` / commit `sha` fields)"
        )
    elif not all(c in "0123456789abcdef" for c in v):
        errs.append(f"{name!r} does not look like a hexadecimal git SHA")
    return errs


def _validate_nested_tests(prefix: str, tests_obj: Any) -> list[str]:
    errs: list[str] = []
    if tests_obj is None:
        return [f"missing {prefix!r} object"]
    if not isinstance(tests_obj, dict):
        errs.append(
            f"{prefix}: expected object with 'baseline' and 'post', got {type(tests_obj).__name__}"
        )
        return errs
    if "baseline" not in tests_obj and "post" not in tests_obj:
        flat_markers = ("all_passed", "label", "log_path", "returncode", "needed_retry")
        if any(k in tests_obj for k in flat_markers) or (
            "passed" in tests_obj and "suites_ok" in tests_obj
        ):
            errs.append(
                f"{prefix}: looks like raw `run_tests.py` JSON. "
                'Use an object with two children: "baseline" and "post", each copying the flat '
                f"count fields {COUNT_KEYS} from the baseline and post-rebase `run_tests.py` outputs."
            )
            return errs
    if "baseline" not in tests_obj:
        errs.append(f"{prefix}: missing 'baseline' (nest the baseline run's counts here)")
    else:
        errs.extend(_counts_dict_errors(f"{prefix}.baseline", tests_obj["baseline"]))
    if "post" not in tests_obj:
        errs.append(f"{prefix}: missing 'post' (nest the post-rebase / synced run's counts here)")
    else:
        errs.extend(_counts_dict_errors(f"{prefix}.post", tests_obj["post"]))
    return errs


def _subject_field_errors(path: str, subject: Any) -> list[str]:
    errs: list[str] = []
    if subject is None or (isinstance(subject, str) and not subject.strip()):
        errs.append(f"{path}: missing or empty subject")
        return errs
    if not isinstance(subject, str):
        errs.append(f"{path}: subject must be a string, got {type(subject).__name__}")
        return errs
    s = subject.strip()
    if s in PLACEHOLDER_STRINGS:
        errs.append(f"{path}: subject is a placeholder token {s!r} — fill from inspect_state")
    return errs


def _pr_num_errors(path: str, pr_num: Any) -> list[str]:
    errs: list[str] = []
    if pr_num is None or pr_num == "":
        errs.append(f"{path}: missing pr_num (upstream PR number parsed from commit subject)")
        return errs
    if isinstance(pr_num, str) and pr_num.strip() in PLACEHOLDER_STRINGS:
        errs.append(f"{path}: pr_num is a placeholder — fill from inspect_state")
    return errs


def _short_sha_consistency(path: str, full_sha: str, short_sha: Any) -> list[str]:
    errs: list[str] = []
    if short_sha is None or (isinstance(short_sha, str) and not short_sha.strip()):
        errs.append(f"{path}: missing or empty short_sha")
        return errs
    if not isinstance(short_sha, str):
        errs.append(f"{path}: short_sha must be a string, got {type(short_sha).__name__}")
        return errs
    sh = short_sha.strip()
    if sh == SENTINEL_SHORT_SHA:
        errs.append(
            f"{path}: short_sha must not be the skeleton sentinel {SENTINEL_SHORT_SHA!r} "
            "(use inspect_state batch_commits.short_sha)"
        )
    fh = full_sha.strip().lower()
    sh_l = sh.lower()
    if len(fh) >= 40 and not fh.startswith(sh_l):
        errs.append(
            f"{path}: short_sha {sh!r} is not a prefix of full sha {full_sha!r} "
            "(forgot to merge inspect_state.batch_commits?)"
        )
    return errs


def _validate_batch_commits(commits: Any) -> tuple[list[str], list[str]]:
    """Returns (errors, warnings)."""
    errs: list[str] = []
    warns: list[str] = []
    if not isinstance(commits, list):
        return (["commits: expected an array"], warns)
    if not commits:
        return (
            [
                "commits: empty — build one row per batched upstream commit "
                "(merge inspect_state batch_commits with classify_batch.py rows, or use --merge)"
            ],
            warns,
        )
    for i, c in enumerate(commits):
        pfx = f"commits[{i}]"
        if not isinstance(c, dict):
            errs.append(f"{pfx}: expected object")
            continue
        for k in ("sha", "short_sha", "subject"):
            if k not in c:
                errs.append(f"{pfx}: missing {k!r}")
        if "sha" in c and "short_sha" in c and isinstance(c["sha"], str):
            errs.extend(_short_sha_consistency(pfx, c["sha"], c.get("short_sha")))
        errs.extend(_subject_field_errors(f"{pfx}.subject", c.get("subject")))

        fc = c.get("files_count", 0)
        if fc is not None and not isinstance(fc, int):
            errs.append(f"{pfx}: files_count must be an int or omitted, got {type(fc).__name__}")
        elif isinstance(fc, int) and fc < 0:
            errs.append(f"{pfx}: files_count must be >= 0")
        lbl = c.get("label", "")
        if isinstance(fc, int) and fc == 0 and lbl != "SKIP":
            warns.append(
                f"{pfx}: files_count is 0 but label is {lbl!r} (expected SKIP for zero-file commits) "
                "— verify classify_batch rows were merged for this sha"
            )
    return (errs, warns)


def validate_compose(kind: str, data: dict[str, Any]) -> tuple[list[str], list[str]]:
    """Returns (errors, warnings)."""
    errs: list[str] = []
    warns: list[str] = []
    if kind == "single":
        errs.extend(_full_sha_field("adobe_at_creation", data.get("adobe_at_creation")))
        errs.extend(_full_sha_field("oldest_sha", data.get("oldest_sha")))
        errs.extend(_pr_num_errors("pr_num", data.get("pr_num")))
        errs.extend(_subject_field_errors("oldest_subject", data.get("oldest_subject")))
        cls = data.get("classification")
        if not isinstance(cls, dict):
            errs.append("classification: expected object from classify_commit.py")
        else:
            for ck in ("label", "risk", "reason"):
                if ck not in cls:
                    errs.append(f"classification.{ck} missing")
        errs.extend(_validate_nested_tests("tests", data.get("tests")))
    elif kind == "batch":
        errs.extend(_full_sha_field("adobe_at_creation", data.get("adobe_at_creation")))
        errs.extend(_full_sha_field("batch_end_sha", data.get("batch_end_sha")))
        errs.extend(_full_sha_field("oldest_sha", data.get("oldest_sha")))
        bc = data.get("batch_count")
        if not isinstance(bc, int) or bc < 1:
            errs.append("batch_count must be an integer >= 1 (inspect_state batch_count)")
        ce, cw = _validate_batch_commits(data.get("commits"))
        errs.extend(ce)
        warns.extend(cw)
        errs.extend(_validate_nested_tests("tests", data.get("tests")))
    else:
        errs.extend(_full_sha_field("adobe_at_creation", data.get("adobe_at_creation")))
        errs.extend(_full_sha_field("conflicting_sha", data.get("conflicting_sha")))
        cshort = data.get("conflicting_short_sha")
        if not cshort:
            errs.append("missing conflicting_short_sha")
        elif isinstance(cshort, str):
            if cshort.strip() == SENTINEL_SHORT_SHA:
                errs.append(
                    f"conflicting_short_sha must not be the skeleton sentinel {SENTINEL_SHORT_SHA!r}"
                )
            csha = data.get("conflicting_sha")
            if isinstance(csha, str):
                errs.extend(_short_sha_consistency("conflicting_short_sha", csha, cshort))
        errs.extend(_subject_field_errors("subject", data.get("subject")))
        for k in ("clean_count", "prefix_pr_num"):
            if k not in data:
                errs.append(f"missing required field {k!r}")
        tb = data.get("tests_baseline")
        tp = data.get("tests_post")
        if isinstance(tb, dict) and ("baseline" in tb or "post" in tb):
            errs.append(
                "tests_baseline must be a FLAT counts dict for `--kind auto-resolve` "
                f"(keys {COUNT_KEYS} copied from run_tests.py), not nested under baseline/post"
            )
        if isinstance(tp, dict) and ("baseline" in tp or "post" in tp):
            errs.append(
                "tests_post must be a FLAT counts dict for `--kind auto-resolve` "
                f"(keys {COUNT_KEYS}), not nested under baseline/post"
            )
        errs.extend(_counts_dict_errors("tests_baseline", tb))
        errs.extend(_counts_dict_errors("tests_post", tp))
    return (errs, warns)


def _load_json(path: str, label: str) -> dict[str, Any]:
    p = Path(path)
    try:
        raw = p.read_text(encoding="utf-8")
    except OSError as e:
        raise ValueError(f"cannot read {label} ({path}): {e}") from e
    try:
        obj = json.loads(raw)
    except json.JSONDecodeError as e:
        raise ValueError(f"invalid JSON in {label} ({path}): {e}") from e
    if not isinstance(obj, dict):
        raise ValueError(f"{label} ({path}): expected a JSON object at top level")
    return obj


def _load_json_list(path: str, label: str) -> list[Any]:
    p = Path(path)
    try:
        raw = p.read_text(encoding="utf-8")
    except OSError as e:
        raise ValueError(f"cannot read {label} ({path}): {e}") from e
    try:
        obj = json.loads(raw)
    except json.JSONDecodeError as e:
        raise ValueError(f"invalid JSON in {label} ({path}): {e}") from e
    if not isinstance(obj, list):
        raise ValueError(f"{label} ({path}): expected a JSON array at top level")
    return obj


def _counts_from_run_tests(rt: dict[str, Any], label: str) -> dict[str, int]:
    out: dict[str, int] = {}
    for k in COUNT_KEYS:
        if k not in rt:
            raise ValueError(f"{label}: run_tests JSON missing key {k!r}")
        v = rt[k]
        if not isinstance(v, int):
            raise ValueError(f"{label}: key {k!r} must be int, got {type(v).__name__}")
        out[k] = v
    return out


def _merge_batch_from_files(
    inspect_path: str,
    classify_path: str,
    baseline_path: str,
    post_path: str,
    adobe_at_creation: str,
) -> dict[str, Any]:
    insp = _load_json(inspect_path, "inspect-state")
    clsj = _load_json(classify_path, "classify-batch")
    base_rt = _load_json(baseline_path, "baseline-tests")
    post_rt = _load_json(post_path, "post-tests")

    bcommits = insp.get("batch_commits")
    if not isinstance(bcommits, list) or not bcommits:
        raise ValueError("inspect-state: missing or empty batch_commits")

    cls_commits = clsj.get("commits")
    if not isinstance(cls_commits, list) or not cls_commits:
        raise ValueError("classify-batch: missing or empty commits")

    by_sha: dict[str, dict[str, Any]] = {}
    for row in cls_commits:
        if not isinstance(row, dict) or "sha" not in row:
            continue
        sha = row["sha"]
        if isinstance(sha, str):
            by_sha[sha.strip().lower()] = row

    merged_rows: list[dict[str, Any]] = []
    for i, bc in enumerate(bcommits):
        if not isinstance(bc, dict):
            raise ValueError(f"inspect-state batch_commits[{i}]: expected object")
        sha_full = bc.get("sha")
        if not isinstance(sha_full, str) or not sha_full.strip():
            raise ValueError(f"inspect-state batch_commits[{i}]: missing sha")
        sha_k = sha_full.strip().lower()
        crow = by_sha.get(sha_k)
        if crow is None:
            raise ValueError(
                f"classify-batch has no entry for sha {sha_full} "
                "(ensure classify_batch.py was run with the same SHAs as batch_commits)"
            )
        files = crow.get("files")
        if not isinstance(files, list):
            raise ValueError(f"classify-batch commit {sha_full}: missing files list")
        merged_rows.append(
            {
                "sha": sha_full.strip(),
                "short_sha": bc.get("short_sha"),
                "pr_num": bc.get("pr_num"),
                "subject": bc.get("subject"),
                "files_count": len(files),
                "label": crow.get("label", ""),
                "risk": crow.get("risk", ""),
            }
        )

    batch_count = insp.get("batch_count")
    if not isinstance(batch_count, int) or batch_count < 1:
        batch_count = len(merged_rows)

    show_banner = False
    banner_items: list[dict[str, str]] = []
    for row in merged_rows:
        sha_k = row["sha"].strip().lower()
        crow = by_sha.get(sha_k)
        if crow is None:
            continue
        if crow.get("label") == "SECURITY" or crow.get("risk") == "critical":
            show_banner = True
            reason = crow.get("reason", "")
            banner_items.append(
                {
                    "short_sha": str(row.get("short_sha", "")),
                    "subject": str(row.get("subject", "")),
                    "detail": str(reason) if reason else "",
                }
            )

    tests_fail_tail = None
    if post_rt.get("tail") and not post_rt.get("all_passed", True):
        tl = post_rt["tail"]
        if isinstance(tl, list) and all(isinstance(x, str) for x in tl):
            tests_fail_tail = tl

    return {
        "adobe_at_creation": adobe_at_creation.strip(),
        "batch_count": batch_count,
        "batch_end_sha": insp.get("batch_end_sha"),
        "oldest_sha": insp.get("oldest_sha"),
        "prev_upstream_sha": insp.get("prev_upstream_sha"),
        "commits": merged_rows,
        "show_banner": show_banner,
        "banner_items": banner_items,
        "tests": {
            "baseline": _counts_from_run_tests(base_rt, "baseline-tests"),
            "post": _counts_from_run_tests(post_rt, "post-tests"),
        },
        "retry_note": bool(post_rt.get("needed_retry")),
        "tests_fail_tail": tests_fail_tail,
    }


def _merge_single_from_files(
    inspect_path: str,
    classify_path: str,
    baseline_path: str,
    post_path: str,
    adobe_at_creation: str,
) -> dict[str, Any]:
    insp = _load_json(inspect_path, "inspect-state")
    cls_row = _load_json(classify_path, "classify-commit")
    base_rt = _load_json(baseline_path, "baseline-tests")
    post_rt = _load_json(post_path, "post-tests")

    prn = insp.get("pr_num")
    classification = {
        "label": cls_row.get("label"),
        "risk": cls_row.get("risk"),
        "reason": cls_row.get("reason"),
    }
    if cls_row.get("highlights"):
        classification["highlights"] = cls_row["highlights"]

    tests_fail_tail = None
    if post_rt.get("tail") and not post_rt.get("all_passed", True):
        tl = post_rt["tail"]
        if isinstance(tl, list) and all(isinstance(x, str) for x in tl):
            tests_fail_tail = tl

    return {
        "adobe_at_creation": adobe_at_creation.strip(),
        "oldest_sha": insp.get("oldest_sha"),
        "oldest_subject": insp.get("oldest_subject"),
        "pr_num": prn,
        "classification": classification,
        "tests": {
            "baseline": _counts_from_run_tests(base_rt, "baseline-tests"),
            "post": _counts_from_run_tests(post_rt, "post-tests"),
        },
        "retry_note": bool(post_rt.get("needed_retry")),
        "changelog_section": True,
        "supersedes": [],
        "tests_fail_tail": tests_fail_tail,
    }


def _merge_auto_resolve_from_files(
    bisect_path: str,
    resolution_rows_path: str,
    baseline_path: str,
    post_path: str,
    adobe_at_creation: str,
    prefix_pr_num: int,
) -> dict[str, Any]:
    bis = _load_json(bisect_path, "find-clean-prefix")
    res_rows = _load_json_list(resolution_rows_path, "resolution-rows")
    for i, r in enumerate(res_rows):
        if not isinstance(r, dict):
            raise ValueError(f"resolution-rows[{i}]: expected object")
        for k in ("file", "rule"):
            if k not in r:
                raise ValueError(f"resolution-rows[{i}]: missing {k!r}")
    base_rt = _load_json(baseline_path, "baseline-tests")
    post_rt = _load_json(post_path, "post-tests")

    tests_fail_tail = None
    if post_rt.get("tail") and not post_rt.get("all_passed", True):
        tl = post_rt["tail"]
        if isinstance(tl, list) and all(isinstance(x, str) for x in tl):
            tests_fail_tail = tl

    failing = not bool(post_rt.get("all_passed", True))

    return {
        "adobe_at_creation": adobe_at_creation.strip(),
        "conflicting_sha": bis.get("conflicting_sha"),
        "conflicting_short_sha": bis.get("conflicting_short_sha"),
        "conflicting_pr_num": bis.get("conflicting_pr_num"),
        "subject": bis.get("conflicting_subject"),
        "clean_count": bis.get("clean_count"),
        "prefix_pr_num": prefix_pr_num,
        "resolution_rows": res_rows,
        "tests_baseline": _counts_from_run_tests(base_rt, "baseline-tests"),
        "tests_post": _counts_from_run_tests(post_rt, "post-tests"),
        "retry_note": bool(post_rt.get("needed_retry")),
        "tests_failing_pr": failing,
        "tests_fail_tail": tests_fail_tail,
    }


def render_sanity_errors(md: str) -> list[str]:
    """Post-render leak detection; returns human-readable error strings."""
    errs: list[str] = []
    if "`0000000`" in md:
        errs.append("rendered body still contains skeleton short SHA `0000000`")
    if re.search(r"\|\s*\(n/a\)\s*\|\s*\|", md):
        errs.append(
            "rendered body matches empty-subject / no-PR row pattern (|(n/a)||) seen in broken sync PRs"
        )
    if "REPLACE_" in md:
        errs.append("rendered body still contains REPLACE_ placeholder text")
    for line in md.splitlines():
        stripped = line.strip()
        if not stripped.startswith("|") or stripped.startswith("|---"):
            continue
        parts = [p.strip() for p in stripped.split("|")]
        if len(parts) >= 7 and parts[1].isdigit():
            subject_cell = parts[4] if len(parts) > 4 else ""
            if subject_cell == "":
                errs.append(
                    "rendered batch table row has an empty Subject cell — check commit merge inputs"
                )
                break
    return errs


def skeleton(kind: str) -> dict[str, Any]:
    if kind == "single":
        return {
            "adobe_at_creation": _PLACEHOLDER_SHA,
            "oldest_sha": _PLACEHOLDER_SHA,
            "pr_num": None,
            "oldest_subject": None,
            "classification": {"label": "MERGE_SAFE", "risk": "low", "reason": "placeholder"},
            "tests": {"baseline": _blank_counts(), "post": _blank_counts()},
            "retry_note": False,
            "changelog_section": True,
            "supersedes": [],
        }
    if kind == "batch":
        return {
            "adobe_at_creation": _PLACEHOLDER_SHA,
            "batch_count": 1,
            "batch_end_sha": _PLACEHOLDER_SHA,
            "oldest_sha": _PLACEHOLDER_SHA,
            "commits": [
                {
                    "sha": _PLACEHOLDER_SHA,
                    "short_sha": None,
                    "pr_num": None,
                    "subject": None,
                    "files_count": 0,
                    "label": "MERGE_SAFE",
                    "risk": "low",
                }
            ],
            "show_banner": False,
            "banner_items": [],
            "tests": {"baseline": _blank_counts(), "post": _blank_counts()},
            "retry_note": False,
            "tests_fail_tail": None,
        }
    return {
        "adobe_at_creation": _PLACEHOLDER_SHA,
        "conflicting_sha": _PLACEHOLDER_SHA,
        "conflicting_short_sha": None,
        "conflicting_pr_num": None,
        "subject": None,
        "clean_count": 1,
        "prefix_pr_num": 1,
        "resolution_rows": [{"file": "path/to/file", "rule": "RULE_NAME", "notes": ""}],
        "tests_baseline": _blank_counts(),
        "tests_post": _blank_counts(),
        "retry_note": False,
        "tests_failing_pr": False,
        "tests_fail_tail": None,
    }


def _row_cells(prefix: str, d: dict[str, Any]) -> str:
    return (
        f"| {prefix} | {d['passed']} | {d['failed']} | {d['ignored']} | "
        f"{d['warnings']} | {d['suites_ok']} |"
    )


def _render_single(data: dict[str, Any]) -> str:
    osha = data["oldest_sha"]
    pr = data["pr_num"]
    subj = data["oldest_subject"]
    cls = data["classification"]
    tb = data["tests"]["baseline"]
    tp = data["tests"]["post"]
    ada = data["adobe_at_creation"]
    retry = data.get("retry_note", False)
    tail = data.get("tests_fail_tail")
    lines = [
        "## Upstream commit synced",
        "",
        f"- `{osha}` — [view commit](https://github.com/agentgateway/agentgateway/commit/{osha})",
        f"- Upstream PR: [agentgateway/agentgateway#{pr}](https://github.com/agentgateway/agentgateway/pull/{pr})",
        f"- Subject: `{subj}`",
        "",
        "## Test results",
        "",
        "| Run | Passed | Failed | Ignored | Warnings | Suites OK |",
        "|---|---|---|---|---|---|",
        _row_cells("Baseline", tb),
        _row_cells("Post-rebase", tp),
        "",
    ]
    if retry:
        lines.extend(
            [
                "Post-rebase run needed one retry — first attempt had flakes; retry passed.",
                "",
            ]
        )
    if tail:
        lines.extend(
            [
                "<details>",
                "<summary>Failing test log (tail)</summary>",
                "",
                "```",
                *tail,
                "```",
                "",
                "</details>",
                "",
            ]
        )
    lines.extend(
        [
            "## Classification (from `classify_commit.py`)",
            "",
            "Paste the JSON summary:",
            "",
            "| Field | Value |",
            "|---|---|",
            f"| label | `{cls['label']}` |",
            f"| risk | `{cls['risk']}` |",
            f"| reason | `{cls['reason']}` |",
            "",
        ]
    )
    if cls.get("highlights"):
        lines.extend(["", cls["highlights"], ""])
    if data.get("changelog_section", True):
        lines.extend(
            [
                "## Suggested CHANGELOG (optional)",
                "",
                "Use the `changelog-entry` skill to draft bullets for `adobe/CHANGELOG.md`; "
                "paste the proposed text here for reviewer visibility (human still confirms "
                "before committing changelog updates on the integration branch).",
                "",
            ]
        )
    sup = data.get("supersedes") or []
    if sup:
        lines.extend(["## Supersedes", ""])
        for p in sup:
            lines.append(f"- #{p['number']} ({p['state'].lower()}) — {p['url']}")
        lines.append("")
    lines.extend(
        [
            "## Landing",
            "",
            "A maintainer (anyone with `write`, `maintain`, or `admin` permission on this repo) "
            "commenting `/land` on this PR authorises landing. After commenting, **re-invoke** "
            "`land PR #N` with this PR number so the sync skill runs `references/poll-and-land.md` "
            "section 2. The skill does **not** poll GitHub in the background.",
            "",
            'There is deliberately no requirement for a GitHub "Approve" review; the `/land` '
            "comment is the sole authorisation.",
            "",
            'Do NOT use GitHub\'s native merge buttons — they either conflict ("Rebase and merge"), '
            'produce duplicate history ("Create a merge commit"), or collapse per-commit attribution '
            '("Squash and merge"). All three are wrong for this workflow. See '
            "`references/merge-strategies.md` in the skill dir for why.",
            "",
            "<!-- sync-metadata",
            f"adobe_at_creation: {ada}",
            "-->",
            "",
        ]
    )
    return "\n".join(lines)


def _pr_cell(pr_num: str | None) -> str:
    if not pr_num:
        return "(n/a)"
    return f"[#{pr_num}](https://github.com/agentgateway/agentgateway/pull/{pr_num})"


def _render_batch(data: dict[str, Any]) -> str:
    ada = data["adobe_at_creation"]
    n = data["batch_count"]
    commits = data["commits"]
    tb = data["tests"]["baseline"]
    tp = data["tests"]["post"]
    retry = data.get("retry_note", False)
    tail = data.get("tests_fail_tail")

    lines = [
        "## Upstream commits synced",
        "",
        f"This PR batches **{n}** upstream commits. The diff against `adobe` "
        "will look large — that is expected (Shape D, see "
        "`references/merge-strategies.md`). The actual upstream delta is the "
        "commits listed below; everything else is Adobe commits being "
        "reapplied.",
        "",
    ]
    prev = data.get("prev_upstream_sha")
    end = data.get("batch_end_sha")
    if isinstance(prev, str) and prev.strip() and isinstance(end, str) and end.strip():
        compare_url = (
            f"https://github.com/agentgateway/agentgateway/compare/"
            f"{prev.strip()}...{end.strip()}"
        )
        lines.extend(
            [
                f"**Reviewable upstream-only diff** (just the {n} upstream commit(s), "
                f"no Adobe noise): [`{prev[:8]}...{end[:8]}`]({compare_url})",
                "",
            ]
        )
    lines.extend(
        [
            "| # | Commit | PR | Subject | Files | Class | Risk |",
            "|---|--------|-----|---------|-------|-------|------|",
        ]
    )
    for i, c in enumerate(commits, start=1):
        sha = c["sha"]
        sh = c["short_sha"]
        pr = c.get("pr_num")
        subj = c["subject"].replace("|", "\\|")
        fc = c.get("files_count", "")
        lab = c.get("label", "")
        risk = c.get("risk", "")
        lines.append(
            f"| {i} | [`{sh}`](https://github.com/agentgateway/agentgateway/commit/{sha}) | "
            f"{_pr_cell(pr)} | {subj} | {fc} | {lab} | {risk} |"
        )
    lines.append("")

    if data.get("show_banner"):
        lines.extend(
            [
                "## ⚠️ Heads up — review these commits in particular",
                "",
            ]
        )
        for b in data.get("banner_items") or []:
            det = b.get("detail", "").replace("|", "\\|")
            lines.append(f"- `{b['short_sha']}` — `{b['subject']}` — {det}")
        lines.extend(
            [
                "",
                "These were classified as elevated risk by `classify_commit.py`. The "
                "batch+yolo flow does NOT gate on classification — these commits landed "
                "because the rebase was clean and tests passed. Review retroactively "
                "and revert/patch if anything is wrong.",
                "",
            ]
        )

    lines.extend(
        [
            "## Test results",
            "",
            "| Run | Passed | Failed | Ignored | Warnings | Suites OK |",
            "|---|---|---|---|---|---|",
            _row_cells("Baseline", tb),
            _row_cells("Post-rebase", tp),
            "",
        ]
    )
    if retry:
        lines.extend(
            [
                "Post-rebase run needed one retry — first attempt had flakes; retry passed.",
                "",
            ]
        )
    if tail:
        lines.extend(
            [
                "<details>",
                "<summary>Failing test log (tail)</summary>",
                "",
                "```",
                *tail,
                "```",
                "",
                "</details>",
                "",
            ]
        )

    lines.extend(
        [
            "## Landing",
            "",
            "This PR was created by the **batch+yolo** flow. When both gates pass "
            "(clean rebase + tests pass), the skill auto-lands immediately by "
            "force-updating `adobe` to the PR head — no `/land` comment required, "
            "no review required. Classification labels are informational (see the "
            "table above).",
            "",
            "If you are reading this PR after it has already been auto-landed "
            "(state: `merged`, or `closed` with `adobe` pointing at the PR head), "
            "that's the expected outcome. Use the table to retroactively focus on "
            "any commits flagged with elevated risk.",
            "",
            "If this PR is still **open**, it means gate 2 (tests) failed and "
            "landing was held for explicit `/land` authorisation from a maintainer "
            "(`admin`/`maintain`/`write` permission). Inspect the test results "
            "section before commenting `/land`. After commenting, **re-invoke** "
            "`land PR #N` so `references/poll-and-land.md` section 2 can complete.",
            "",
            "Do NOT use GitHub's native merge buttons. See "
            "`references/merge-strategies.md` for why.",
            "",
            "<!-- sync-metadata",
            f"adobe_at_creation: {ada}",
            f"batch_count: {n}",
            f"batch_end_sha: {data['batch_end_sha']}",
            f"oldest_sha: {data['oldest_sha']}",
            "flow: batch-and-yolo",
            "-->",
            "",
        ]
    )
    return "\n".join(lines)


def _render_auto_resolve(data: dict[str, Any]) -> str:
    ada = data["adobe_at_creation"]
    csha = data["conflicting_sha"]
    cshort = data["conflicting_short_sha"]
    prn = data.get("conflicting_pr_num")
    subj = data["subject"].replace("|", "\\|")
    tb = data["tests_baseline"]
    tp = data["tests_post"]
    failing = data.get("tests_failing_pr", False)
    tail = data.get("tests_fail_tail")
    retry = data.get("retry_note", False)

    intro = (
        "This PR contains **one** upstream commit that conflicted with Adobe "
        "code on rebase. The conflict was classified as superficial and "
        "auto-resolved by the sync skill."
    )
    if failing:
        intro = (
            "This PR contains **one** upstream commit that conflicted with Adobe "
            "code on rebase. The conflict was classified as superficial and "
            "auto-resolved by the sync skill, but tests failed — landing requires "
            "explicit `/land` review."
        )

    line_pr = _pr_cell(prn)

    lines = [
        "## Auto-resolved upstream commit",
        "",
        intro,
        "",
        "| Field | Value |",
        "|---|---|",
        f"| Upstream commit | [`{cshort}`](https://github.com/agentgateway/agentgateway/commit/{csha}) |",
        f"| Upstream PR | {line_pr} |",
        f"| Subject | `{subj}` |",
        f"| Landed in batch run | After `{data['clean_count']}` clean commits in PR #{data['prefix_pr_num']} |",
        "",
        "## Auto-resolution detail",
        "",
        "The following files had conflicts; the rule applied to each is "
        "listed. None of these files are in protected paths (`jwt.rs`, "
        "`mcp/sse.rs`, `adobe/`).",
        "",
        "| File | Rule | Notes |",
        "|---|---|---|",
    ]
    for r in data.get("resolution_rows") or []:
        notes = str(r.get("notes", "")).replace("|", "\\|")
        lines.append(f"| `{r['file']}` | {r['rule']} | {notes} |")
    lines.extend(
        [
            "",
            "The agent's classification policy is in "
            "`references/auto-resolve-conflict.md` (rules table). Anything more "
            "ambiguous than these would have halted.",
            "",
            "## Test results",
            "",
            "| Run | Passed | Failed | Ignored | Warnings | Suites OK |",
            "|---|---|---|---|---|---|",
            _row_cells("Baseline (start of batch run)", tb),
            _row_cells("Post auto-resolve", tp),
            "",
        ]
    )
    if retry:
        lines.extend(
            [
                "Post auto-resolve needed one retry — first attempt had flakes; retry passed.",
                "",
            ]
        )
    if tail:
        lines.extend(
            [
                "<details>",
                "<summary>Failing test log (tail)</summary>",
                "",
                "```",
                *tail,
                "```",
                "",
                "</details>",
                "",
            ]
        )

    if failing:
        lines.extend(
            [
                "## Landing",
                "",
                "Auto-resolution compiled but tests failed — landing requires explicit `/land` "
                "from a maintainer. After commenting `/land`, **re-invoke** `land PR #N`.",
                "",
                "<!-- sync-metadata",
                f"adobe_at_creation: {ada}",
                "auto_resolved: true",
                f"conflicting_sha: {csha}",
                f"prefix_pr_num: {data['prefix_pr_num']}",
                "flow: auto-resolve-conflict",
                "-->",
                "",
            ]
        )
    else:
        lines.extend(
            [
                "## Landing",
                "",
                "This PR was created by the **auto-resolve** sub-flow of batch+yolo. "
                "Both gates (auto-resolve classification + tests) passed, so it "
                "auto-lands immediately via `scripts/land_pr.py`. No `/land` required.",
                "",
                "If you are reading this PR after it has already auto-landed (state "
                "`merged` or `closed` with `adobe` pointing at PR head), that's the "
                "expected outcome. Use the auto-resolution detail table above to spot "
                "check whether the resolution rules look right; revert/patch if "
                "anything is wrong.",
                "",
                "<!-- sync-metadata",
                f"adobe_at_creation: {ada}",
                "auto_resolved: true",
                f"conflicting_sha: {csha}",
                f"prefix_pr_num: {data['prefix_pr_num']}",
                "flow: auto-resolve-conflict",
                "-->",
                "",
            ]
        )

    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--kind", choices=("single", "batch", "auto-resolve"))
    ap.add_argument(
        "--print-template",
        metavar="KIND",
        choices=("single", "batch", "auto-resolve"),
        help="Emit a starter JSON document for KIND to stdout and exit (stderr has mapping hints)",
    )
    ap.add_argument(
        "--merge",
        action="store_true",
        help="Build PR body JSON from inspect_state/classify_batch/run_tests outputs (preferred)",
    )
    ap.add_argument("--json-file")
    ap.add_argument("--out")
    ap.add_argument("--inspect-state", help="inspect_state.py JSON (--merge batch/single)")
    ap.add_argument("--classify-batch", help="classify_batch.py JSON (--merge batch)")
    ap.add_argument("--classify-commit", help="classify_commit.py JSON (--merge single)")
    ap.add_argument("--baseline-tests", help="run_tests.py --output JSON for baseline run")
    ap.add_argument("--post-tests", help="run_tests.py --output JSON for post-rebase/synced run")
    ap.add_argument(
        "--adobe-at-creation",
        help="Full 40-char SHA of origin/adobe at PR open time (--merge)",
    )
    ap.add_argument("--find-clean-prefix", help="find_clean_prefix.py JSON (--merge auto-resolve)")
    ap.add_argument("--prefix-pr-num", type=int, help="Landed prefix PR number (--merge auto-resolve)")
    ap.add_argument(
        "--resolution-rows",
        help="JSON array of {file, rule, notes?} (--merge auto-resolve)",
    )
    args = ap.parse_args()

    if args.print_template:
        print(json.dumps(skeleton(args.print_template), indent=2))
        print(
            "Hints: preferred path is --merge (joins inspect_state + classify_* + run_tests); "
            "for manual JSON replace nulls; adobe_at_creation = git rev-parse origin/adobe "
            "before gh pr create; batch_end_sha = inspect_state.batch_end_sha (full 40-char); "
            "single/batch tests = {baseline: {...}, post: {...}} each with "
            f"{COUNT_KEYS} from respective run_tests.py JSON; "
            "auto-resolve uses flat tests_baseline / tests_post (same keys).",
            file=sys.stderr,
        )
        return 0

    if args.merge:
        if not args.kind:
            print('error: --merge requires --kind ("single", "batch", or "auto-resolve")', file=sys.stderr)
            return 1
        if not args.out:
            ap.error("--merge requires --out")
        if not args.adobe_at_creation:
            ap.error("--merge requires --adobe-at-creation")

        try:
            ada_errs = _full_sha_field("adobe_at_creation", args.adobe_at_creation)
            if ada_errs:
                print(json.dumps({"ok": False, "errors": ada_errs}, indent=2), file=sys.stderr)
                return 2
            if args.kind == "batch":
                if not all(
                    (args.inspect_state, args.classify_batch, args.baseline_tests, args.post_tests)
                ):
                    ap.error(
                        "--merge --kind batch requires --inspect-state, --classify-batch, "
                        "--baseline-tests, and --post-tests"
                    )
                data = _merge_batch_from_files(
                    args.inspect_state,
                    args.classify_batch,
                    args.baseline_tests,
                    args.post_tests,
                    args.adobe_at_creation,
                )
            elif args.kind == "single":
                if not all(
                    (args.inspect_state, args.classify_commit, args.baseline_tests, args.post_tests)
                ):
                    ap.error(
                        "--merge --kind single requires --inspect-state, --classify-commit, "
                        "--baseline-tests, and --post-tests"
                    )
                data = _merge_single_from_files(
                    args.inspect_state,
                    args.classify_commit,
                    args.baseline_tests,
                    args.post_tests,
                    args.adobe_at_creation,
                )
            else:
                if not all(
                    (
                        args.find_clean_prefix,
                        args.resolution_rows,
                        args.baseline_tests,
                        args.post_tests,
                    )
                ):
                    ap.error(
                        "--merge --kind auto-resolve requires --find-clean-prefix, "
                        "--resolution-rows, --baseline-tests, and --post-tests"
                    )
                if args.prefix_pr_num is None:
                    ap.error("--merge --kind auto-resolve requires --prefix-pr-num")
                data = _merge_auto_resolve_from_files(
                    args.find_clean_prefix,
                    args.resolution_rows,
                    args.baseline_tests,
                    args.post_tests,
                    args.adobe_at_creation,
                    args.prefix_pr_num,
                )
        except ValueError as e:
            print(json.dumps({"ok": False, "errors": [str(e)]}, indent=2), file=sys.stderr)
            return 2

        val_errs, warns = validate_compose(args.kind, data)
        for w in warns:
            print(f"warning: {w}", file=sys.stderr)
        if val_errs:
            print(json.dumps({"ok": False, "kind": args.kind, "errors": val_errs}, indent=2), file=sys.stderr)
            return 2

        if args.kind == "single":
            body = _render_single(data)
        elif args.kind == "batch":
            body = _render_batch(data)
        else:
            body = _render_auto_resolve(data)

        leak = render_sanity_errors(body)
        if leak:
            print(
                json.dumps({"ok": False, "kind": args.kind, "render_errors": leak}, indent=2),
                file=sys.stderr,
            )
            return 2

        outp = Path(args.out)
        outp.parent.mkdir(parents=True, exist_ok=True)
        outp.write_text(body, encoding="utf-8")
        print(json.dumps({"ok": True, "out": str(outp.resolve()), "kind": args.kind}))
        return 0

    if not args.json_file or not args.out:
        ap.error("--json-file and --out are required unless --print-template or --merge is used")

    raw = Path(args.json_file).read_text(encoding="utf-8")
    data = json.loads(raw)
    kind = args.kind or data.get("kind")
    if kind not in ("single", "batch", "auto-resolve"):
        print('error: kind missing or invalid (use --kind or JSON "kind")', file=sys.stderr)
        return 1

    val_errs, warns = validate_compose(kind, data)
    for w in warns:
        print(f"warning: {w}", file=sys.stderr)
    if val_errs:
        print(json.dumps({"ok": False, "kind": kind, "errors": val_errs}, indent=2), file=sys.stderr)
        return 2

    if kind == "single":
        body = _render_single(data)
    elif kind == "batch":
        body = _render_batch(data)
    else:
        body = _render_auto_resolve(data)

    leak = render_sanity_errors(body)
    if leak:
        print(
            json.dumps({"ok": False, "kind": kind, "render_errors": leak}, indent=2),
            file=sys.stderr,
        )
        return 2

    outp = Path(args.out)
    outp.parent.mkdir(parents=True, exist_ok=True)
    outp.write_text(body, encoding="utf-8")
    print(json.dumps({"ok": True, "out": str(outp.resolve()), "kind": kind}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
