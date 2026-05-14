"""Normalize GitHub remote URLs for comparison (HTTPS vs SSH, optional .git)."""

from __future__ import annotations

import re

_SSH = re.compile(r"^git@github\.com:([^/]+)/(.+?)(?:\.git)?/?$", re.IGNORECASE)
_HTTPS = re.compile(r"^https?://github\.com/([^/]+)/([^/?#]+?)(?:\.git)?/?$", re.IGNORECASE)

EXPECTED_UPSTREAM_HTTPS = "https://github.com/agentgateway/agentgateway.git"
EXPECTED_ORIGIN_ADOBE_HTTPS = "https://github.com/Adobe-Apis/agentgateway.git"


def normalize_github_remote(url: str | None) -> str | None:
	"""Return canonical form: https://github.com/owner/repo.git (lowercase owner/repo)."""
	if url is None:
		return None
	u = url.strip()
	if not u:
		return None
	m = _SSH.match(u)
	if m:
		owner, repo = m.group(1), m.group(2)
		return f"https://github.com/{owner.lower()}/{repo.lower()}.git"
	m = _HTTPS.match(u)
	if m:
		owner, repo = m.group(1), m.group(2)
		return f"https://github.com/{owner.lower()}/{repo.lower()}.git"
	return None


def canonical_upstream() -> str:
	out = normalize_github_remote(EXPECTED_UPSTREAM_HTTPS)
	return out if out else EXPECTED_UPSTREAM_HTTPS.lower()


def canonical_origin_adobe() -> str:
	out = normalize_github_remote(EXPECTED_ORIGIN_ADOBE_HTTPS)
	return out if out else EXPECTED_ORIGIN_ADOBE_HTTPS.lower()


def remotes_equivalent(url_a: str | None, url_b: str | None) -> bool:
	na = normalize_github_remote(url_a)
	nb = normalize_github_remote(url_b)
	if na is None or nb is None:
		return False
	return na == nb
