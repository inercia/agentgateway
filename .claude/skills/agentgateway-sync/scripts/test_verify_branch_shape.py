#!/usr/bin/env python3
"""Tests for verify_branch_shape (run with `python3 test_verify_branch_shape.py`).

Builds a throwaway git repo shaped like a sync branch: a few "upstream"
commits at the base, then "adobe" commits on top.
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest


def git(repo: str, *args: str) -> str:
    env = dict(os.environ)
    env.update(
        GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t",
        GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t",
    )
    return subprocess.run(
        ["git", "-C", repo, *args], capture_output=True, text=True, env=env
    ).stdout.strip()


def commit(repo: str, name: str) -> str:
    with open(os.path.join(repo, name), "w") as fh:
        fh.write(name)
    git(repo, "add", name)
    git(repo, "commit", "-m", name)
    return git(repo, "rev-parse", "HEAD")


def run_verify(repo: str, head: str, shas: list[str], count: int | None = None) -> dict:
    here = os.path.dirname(os.path.abspath(__file__))
    cmd = [
        "python3", os.path.join(here, "verify_branch_shape.py"), repo,
        "--head", head, "--expected-shas", ",".join(shas),
    ]
    if count is not None:
        cmd += ["--expected-count", str(count)]
    p = subprocess.run(cmd, capture_output=True, text=True)
    return json.loads(p.stdout)


class TestVerifyBranchShape(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = tempfile.mkdtemp()
        git(self.repo, "init", "-q")
        commit(self.repo, "base")
        self.up1 = commit(self.repo, "upstream1")
        self.up2 = commit(self.repo, "upstream2")
        # adobe commits on top
        commit(self.repo, "adobe1")
        commit(self.repo, "adobe2")
        self.head = git(self.repo, "rev-parse", "HEAD")

    def test_ok_when_all_present(self) -> None:
        out = run_verify(self.repo, self.head, [self.up1, self.up2])
        self.assertTrue(out["ok"], out)
        self.assertEqual(out["missing_shas"], [])
        self.assertTrue(out["batch_end_is_ancestor"])
        self.assertEqual(out["adobe_commits_on_top"], 2)

    def test_missing_sha_fails(self) -> None:
        fake = "0" * 40
        out = run_verify(self.repo, self.head, [self.up1, fake])
        self.assertFalse(out["ok"])
        self.assertIn(fake, out["missing_shas"])

    def test_count_drift_fails(self) -> None:
        out = run_verify(self.repo, self.head, [self.up1, self.up2], count=4)
        self.assertFalse(out["ok"])
        self.assertFalse(out["count_match"])

    def test_count_match_passes(self) -> None:
        out = run_verify(self.repo, self.head, [self.up1, self.up2], count=2)
        self.assertTrue(out["ok"], out)


if __name__ == "__main__":
    unittest.main()
