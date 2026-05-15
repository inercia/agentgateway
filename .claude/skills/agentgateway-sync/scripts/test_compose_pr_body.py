#!/usr/bin/env python3
"""Unit tests for compose_pr_body (run with `python3 test_compose_pr_body.py`)."""

from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

from compose_pr_body import (
    _merge_batch_from_files,
    render_sanity_errors,
    skeleton,
    validate_compose,
)


class TestValidateBatchCommits(unittest.TestCase):
    def test_short_sha_sentinel_rejected(self) -> None:
        sha = "a" * 40
        data = {
            "adobe_at_creation": sha,
            "batch_end_sha": sha,
            "oldest_sha": sha,
            "batch_count": 1,
            "commits": [
                {
                    "sha": sha,
                    "short_sha": "0000000",
                    "pr_num": None,
                    "subject": "hello (#1)",
                    "files_count": 1,
                    "label": "MERGE_SAFE",
                    "risk": "low",
                }
            ],
            "tests": {"baseline": _vz_counts(), "post": _vz_counts()},
        }
        errs, _ = validate_compose("batch", data)
        self.assertTrue(any("0000000" in e or "sentinel" in e for e in errs), errs)

    def test_empty_subject_rejected(self) -> None:
        sha = "a" * 40
        data = {
            "adobe_at_creation": sha,
            "batch_end_sha": sha,
            "oldest_sha": sha,
            "batch_count": 1,
            "commits": [
                {
                    "sha": sha,
                    "short_sha": "aaa",
                    "pr_num": None,
                    "subject": "",
                    "files_count": 1,
                    "label": "MERGE_SAFE",
                    "risk": "low",
                }
            ],
            "tests": {"baseline": _vz_counts(), "post": _vz_counts()},
        }
        errs, _ = validate_compose("batch", data)
        self.assertTrue(any("subject" in e.lower() for e in errs), errs)

    def test_placeholder_subject_rejected(self) -> None:
        sha = "a" * 40
        data = {
            "adobe_at_creation": sha,
            "batch_end_sha": sha,
            "oldest_sha": sha,
            "batch_count": 1,
            "commits": [
                {
                    "sha": sha,
                    "short_sha": "aaa",
                    "pr_num": None,
                    "subject": "REPLACE_ROW_SUBJECT",
                    "files_count": 1,
                    "label": "MERGE_SAFE",
                    "risk": "low",
                }
            ],
            "tests": {"baseline": _vz_counts(), "post": _vz_counts()},
        }
        errs, _ = validate_compose("batch", data)
        self.assertTrue(errs)

    def test_files_count_zero_non_skip_warns(self) -> None:
        sha = "a" * 40
        data = {
            "adobe_at_creation": sha,
            "batch_end_sha": sha,
            "oldest_sha": sha,
            "batch_count": 1,
            "commits": [
                {
                    "sha": sha,
                    "short_sha": "aaa",
                    "pr_num": None,
                    "subject": "hello (#1)",
                    "files_count": 0,
                    "label": "MERGE_SAFE",
                    "risk": "low",
                }
            ],
            "tests": {"baseline": _vz_counts(), "post": _vz_counts()},
        }
        errs, warns = validate_compose("batch", data)
        self.assertEqual(errs, [])
        self.assertTrue(any("files_count is 0" in w for w in warns), warns)


def _vz_counts() -> dict[str, int]:
    return {"passed": 1, "failed": 0, "ignored": 0, "warnings": 0, "suites_ok": 1}


class TestPrintTemplateValidation(unittest.TestCase):
    def test_batch_skeleton_fails_validation(self) -> None:
        data = skeleton("batch")
        errs, _ = validate_compose("batch", data)
        self.assertTrue(errs)


class TestRenderSanityErrors(unittest.TestCase):
    def test_detects_pr50_pattern(self) -> None:
        md = "| 1 | [`0000000`](https://github.com/agentgateway/agentgateway/commit/%s) | (n/a) |  | 0 | NEEDS_REVIEW | low |\n" % (
            "b" * 40,
        )
        e = render_sanity_errors(md)
        self.assertTrue(e)

    def test_detects_replace_placeholder(self) -> None:
        self.assertTrue(render_sanity_errors("hello REPLACE_FOO bar"))


class TestMergeBatchHappyPath(unittest.TestCase):
    def test_merge_writes_expected_subjects_and_short(self) -> None:
        sha = "aa" * 20
        short = "aa" * 7
        self.assertTrue(sha.lower().startswith(short.lower()))
        prev = "11" * 20
        insp = {
            "batch_commits": [
                {
                    "sha": sha,
                    "short_sha": short,
                    "subject": "sample commit (#42)",
                    "pr_num": "42",
                }
            ],
            "batch_count": 1,
            "batch_end_sha": sha,
            "oldest_sha": sha,
            "prev_upstream_sha": prev,
        }
        cls = {
            "commits": [
                {
                    "sha": sha,
                    "label": "MERGE_SAFE",
                    "risk": "low",
                    "reason": "ok",
                    "files": ["crates/foo/src/lib.rs"],
                }
            ]
        }
        rt = {
            "passed": 10,
            "failed": 0,
            "ignored": 0,
            "warnings": 0,
            "suites_ok": 2,
            "all_passed": True,
        }
        ada = "cc" * 20
        with tempfile.TemporaryDirectory() as td:
            t = Path(td)
            (t / "inspect.json").write_text(json.dumps(insp), encoding="utf-8")
            (t / "classify.json").write_text(json.dumps(cls), encoding="utf-8")
            (t / "base.json").write_text(json.dumps(rt), encoding="utf-8")
            (t / "post.json").write_text(json.dumps(rt), encoding="utf-8")
            out_md = t / "out.md"
            data = _merge_batch_from_files(
                str(t / "inspect.json"),
                str(t / "classify.json"),
                str(t / "base.json"),
                str(t / "post.json"),
                ada,
            )
            errs, warns = validate_compose("batch", data)
            self.assertEqual(errs, [], errs)
            self.assertEqual(warns, [], warns)

            proc = subprocess.run(
                [
                    "python3",
                    str(Path(__file__).resolve().parent / "compose_pr_body.py"),
                    "--kind",
                    "batch",
                    "--merge",
                    "--inspect-state",
                    str(t / "inspect.json"),
                    "--classify-batch",
                    str(t / "classify.json"),
                    "--baseline-tests",
                    str(t / "base.json"),
                    "--post-tests",
                    str(t / "post.json"),
                    "--adobe-at-creation",
                    ada,
                    "--out",
                    str(out_md),
                ],
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr + proc.stdout)
            body = out_md.read_text(encoding="utf-8")
            self.assertIn(short, body)
            self.assertIn("sample commit (#42)", body)
            self.assertIn("#42", body)
            self.assertIn(
                f"https://github.com/agentgateway/agentgateway/compare/{prev}...{sha}",
                body,
            )
            self.assertIn("Reviewable upstream-only diff", body)


if __name__ == "__main__":
    unittest.main()
