#!/usr/bin/env python3
"""Tests for prune_stale_branches scope guard (`python3 test_prune_stale_branches.py`)."""

from __future__ import annotations

import unittest

from prune_stale_branches import is_sync_branch


class TestScopeGuard(unittest.TestCase):
    def test_matches_sync_prefix(self) -> None:
        self.assertTrue(is_sync_branch("sync/batch-3-to-abc1234"))
        self.assertTrue(is_sync_branch("sync/auto-resolve-deadbeef"))

    def test_matches_bisect_throwaway(self) -> None:
        self.assertTrue(is_sync_branch("sync-bisect-tmp"))

    def test_never_touches_protected_branches(self) -> None:
        for name in ("adobe", "main", "skill/sync-improvements", "fix-regression", "synchronize"):
            self.assertFalse(is_sync_branch(name), name)


if __name__ == "__main__":
    unittest.main()
