#!/usr/bin/env python3
"""Unit tests for run_tests cache helpers (run with `python3 test_run_tests.py`)."""

from __future__ import annotations

import json
import os
import tempfile
import unittest

from run_tests import _cache_read, _cache_write


class TestCache(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp()
        self.cache = os.path.join(self.dir, "test_cache.json")

    def test_round_trip_hit(self) -> None:
        result = {"all_passed": True, "passed": 10, "failed": 0}
        _cache_write(self.cache, "sha-abc", result)
        hit = _cache_read(self.cache, "sha-abc")
        self.assertIsNotNone(hit)
        self.assertEqual(hit["passed"], 10)

    def test_key_mismatch_is_miss(self) -> None:
        _cache_write(self.cache, "sha-abc", {"all_passed": True})
        self.assertIsNone(_cache_read(self.cache, "sha-different"))

    def test_failing_run_never_cached(self) -> None:
        _cache_write(self.cache, "sha-abc", {"all_passed": False})
        self.assertFalse(os.path.exists(self.cache))
        self.assertIsNone(_cache_read(self.cache, "sha-abc"))

    def test_missing_file_is_miss(self) -> None:
        self.assertIsNone(_cache_read(os.path.join(self.dir, "nope.json"), "sha"))

    def test_corrupt_cache_is_miss(self) -> None:
        with open(self.cache, "w") as fh:
            fh.write("{not json")
        self.assertIsNone(_cache_read(self.cache, "sha"))

    def test_cached_failing_result_not_returned(self) -> None:
        # A cache file manually holding a failing result must not be trusted.
        with open(self.cache, "w") as fh:
            json.dump({"key": "sha", "result": {"all_passed": False}}, fh)
        self.assertIsNone(_cache_read(self.cache, "sha"))


if __name__ == "__main__":
    unittest.main()
