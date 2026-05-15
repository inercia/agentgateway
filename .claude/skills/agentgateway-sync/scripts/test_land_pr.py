#!/usr/bin/env python3
"""Unit tests for land_pr helpers (run with `python3 test_land_pr.py`)."""

from __future__ import annotations

import unittest

from land_pr import gh_patch_adobe_ref_cmd


class TestGhPatchAdobeRefCmd(unittest.TestCase):
    def test_uses_uppercase_F_for_force_boolean(self) -> None:
        cmd = gh_patch_adobe_ref_cmd("abc123deadbeef")
        self.assertEqual(cmd[-4:], ["-f", "sha=abc123deadbeef", "-F", "force=true"])

    def test_rejects_lowercase_f_force_true_antipattern(self) -> None:
        cmd = gh_patch_adobe_ref_cmd("x")
        joined = " ".join(cmd)
        self.assertNotIn("-f force=true", joined)
        self.assertNotIn("-f=true", joined)


if __name__ == "__main__":
    unittest.main()
