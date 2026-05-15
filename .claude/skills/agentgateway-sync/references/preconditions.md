# Preconditions — auth, remotes, token hygiene

Load this on every invocation before doing anything mutating. If any check fails, stop and tell the user exactly what to fix. Do not try to proceed through auth/config problems.

## 0. Git remotes (agents / fresh clones)

Before trusting `inspect_state.py`, normalize `origin`, `upstream`, and optional `public`:

```bash
python3 "$SKILL_DIR/scripts/inspect_state.py" "$REPO" --fix-remotes
```

Parse the JSON on stdout. If `errors` contains `wrong_origin` (or similar non-fixable origin issues), stop — the checkout may be the wrong repo or `origin` must be fixed manually (`inspect_state.py --fix-remotes` never mutates `origin`).

If `upstream`/`public` were wrong but fixable, `remotes_fixed_applied` will be true after a successful `--fix-remotes` run. Use `--skip-public` only if your workflow does not use a `public` remote.

Dry-run behaviour: omit `--fix-remotes` — `inspect_state.py` still reports remote problems in `errors` without mutating anything.

After remotes look correct (no blocking errors), continue with section 1.

## 1. `gh` CLI authentication

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh auth status --hostname github.com
```

Expected: an **Active** account whose `Token scopes` includes `repo` and `workflow`. If the Active account is the wrong one, a `GITHUB_TOKEN` in the shell environment is probably overriding the `hosts.yml` credential — which is exactly why every `gh` invocation in this skill is prefixed with `env -u GITHUB_TOKEN -u GH_TOKEN`.

## 2. Adobe fork reachability (SSO-authorised PAT)

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh api repos/Adobe-Apis/agentgateway --jq .full_name
```

- Returns `Adobe-Apis/agentgateway` -> proceed.
- Returns **403** with `"Resource protected by organization SAML enforcement"` -> the PAT exists but isn't SSO-authorised. The error body contains the exact `https://github.com/enterprises/adobe-prd/sso?authorization_request=…` URL. Give the user that URL, tell them to open it in the browser while signed in as the same account (equivalent to clicking "Authorize" in the dropdown at https://github.com/settings/tokens), and wait for confirmation before retrying.
- Returns **404** -> `GITHUB_TOKEN` is resolving to a different user. Confirm the env-strip is in place.

Upstream `agentgateway/agentgateway` is public — no auth check needed.

## 3. Local checkout of `Adobe-Apis/agentgateway`

Default `REPO` for this skill is the **current working directory** (`$PWD`) when the user runs commands from the clone root, or an explicit path the user provides. Verify it is the Adobe fork:

```bash
git -C "$REPO" remote get-url origin
```

Expect a URL containing `Adobe-Apis/agentgateway`. If the checkout is missing, clone (idempotent):

```bash
env -u GITHUB_TOKEN -u GH_TOKEN gh repo clone Adobe-Apis/agentgateway "$REPO"
```

Stop only if clone fails (SSO, network) or the user points at the wrong directory.

## 4. `upstream` remote points at the public project

Always force the remote; an older setup may have pointed it at a private mirror. Prefer **`inspect_state.py "$REPO" --fix-remotes`** (section 0); manual fallback:

```bash
# Run each as its own Bash call.
git -C "$REPO" remote get-url upstream
```

If that returns anything other than `https://github.com/agentgateway/agentgateway.git` (or an equivalent SSH form the normalize helper accepts):

```bash
git -C "$REPO" remote set-url upstream https://github.com/agentgateway/agentgateway.git
```

Or, if `upstream` doesn't exist at all:

```bash
git -C "$REPO" remote add upstream https://github.com/agentgateway/agentgateway.git
```

## 5. No embedded tokens in `.git/config`

`gh repo clone` has been known to write `https://ghp_...@github.com/...` URLs into repo-local `.git/config`. A leaked token there is a security issue — stop and tell the user to rotate.

```bash
grep -c 'ghp_\|github_pat_' "$REPO/.git/config"
```

- Returns `0` -> proceed.
- Returns a non-zero count -> stop. Tell the user: revoke the token at https://github.com/settings/tokens, remove the offending section (`git -C "$REPO" config --local --remove-section 'url.<full-url-with-token>'`), and re-add a clean remote. Do not push anything until the file is clean.

## 6. Landing prerequisites

No extra checks here beyond the above — landing (force-updating `adobe`) uses the same `gh` credential. Branch protection that blocks non-fast-forward updates is detected at landing time (step `poll-and-land`, section 2.6) and surfaced as an error then, because there is no way to test-push a force-update.

## After all checks pass

Return to the dispatcher (`SKILL.md`) and continue with the next step (new-sync: `inspect-and-prepare`; landing: `poll-and-land`).
