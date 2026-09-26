#!/usr/bin/env python3
# open_pr.py — push the current branch and open a GitHub pull request (ADR-0017 §7, ADR-0041).
#
# The PR action of the bundled `github` skill (its read/post actions are
# list_comments.py / post_comment.py): it pushes the branch the run committed on
# and opens a pull request linking back to the originating issue. The agent calls
# it through SKILL.md with the PR title as the sole argument; on a push or create
# failure it exits non-zero so the agent surfaces the failure rather than retrying
# blindly.
#
# It reads the credentials, repo, and originating issue FROM THE ENVIRONMENT — the
# env the afkd github trigger already merges into the worker child
# (crates/github/src/common.rs: GITHUB_TOKEN / GITHUB_HOST / GITHUB_REPO /
# GITHUB_ISSUE_NUMBER, and AFKD_SCRATCH_DIR holding the per-run scratch dir; the
# issue number is also written to <scratch>/issue/number). It does NOT read its own
# path from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses
# — ADR-0017 Revision §5); SKILL.md passes the path on the command line, so this
# helper owns only the push + API mechanics.
#
# This is the gitea PR opener re-pointed at GitHub REST v3, which diverges in four
# ways (mirroring crates/github/src/client.rs): GITHUB_HOST is resolved to the API
# base by github_api_base (cloud → https://api.github.com; any other host → GHES
# <scheme|https>://host/api/v3) rather than used verbatim; endpoints hang off that
# base with NO /api/v1 prefix; auth is `Bearer <token>` (not Gitea's `token <t>`);
# and every request carries a non-empty User-Agent (GitHub REST 403s without one).
# The git *push* host is derived from GITHUB_HOST directly, not the API base: on
# cloud the remote is `github.com` while the API is `api.github.com`. "Pure stdlib"
# constrains HTTP/JSON only — there is no stdlib git, so branch resolution and the
# push still shell out to the `git` CLI via `subprocess`, exactly as gitea does.

import argparse
import http.client
import json
import os
import subprocess
import sys
import urllib.parse

# A non-empty User-Agent is mandatory on GitHub REST — http.client sends none by
# default and real GitHub 403s a UA-less request (crates/github/src/client.rs).
USER_AGENT = "afkd-github-skill"


def github_api_base(host):
    """Resolve GITHUB_HOST to the REST API root every endpoint hangs off, the way
    the Rust client's github_api_base does (crates/github/src/client.rs:308). Cloud
    — empty or (scheme-insensitively) `github.com` — is `https://api.github.com`
    (paths hang directly off it: `/repos/…`); any other host is GitHub Enterprise
    Server, whose API lives under `/api/v3` of the host (the scheme is preserved
    when present, else `https://` is prepended, and a trailing slash is trimmed)."""
    host = host.strip()
    bare = host
    for scheme in ("https://", "http://"):
        if bare.startswith(scheme):
            bare = bare[len(scheme) :]
            break
    bare = bare.rstrip("/")
    if not host or bare == "github.com":
        return "https://api.github.com"
    if host.startswith(("http://", "https://")):
        with_scheme = host.rstrip("/")
    else:
        with_scheme = f"https://{bare}"
    return f"{with_scheme}/api/v3"


def api_path(api_base, suffix):
    """Join the API base's path prefix with an endpoint suffix. Cloud's base has an
    empty path (→ `/repos/…`); a GHES/loopback base's path is `/api/v3`
    (→ `/api/v3/repos/…`), so the version prefix rides the resolved base — never a
    literal `/api/v1`."""
    return urllib.parse.urlsplit(api_base).path + suffix


def connection(api_base):
    """Open an HTTP(S) connection to the API host parsed from `api_base`."""
    parts = urllib.parse.urlsplit(api_base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def issue_number():
    """The originating issue: GITHUB_ISSUE_NUMBER, else <scratch>/issue/number if
    present, else None."""
    issue = os.environ.get("GITHUB_ISSUE_NUMBER")
    if issue:
        return issue
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if scratch:
        path = os.path.join(scratch, "issue", "number")
        if os.path.isfile(path):
            with open(path, encoding="utf-8") as f:
                return f.read().strip()
    return None


def current_branch():
    """The current branch name via `git rev-parse`, or None on a detached HEAD."""
    result = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True,
        text=True,
    )
    head = result.stdout.strip()
    if result.returncode != 0 or not head or head == "HEAD":
        return None
    return head


def push_url(host, repo, token):
    """The token-authenticated git push URL for `repo`, derived from GITHUB_HOST
    directly — NOT the /api/v3 API base: on cloud the git remote is `github.com`
    while the API is `api.github.com`. Empty/`github.com` →
    `https://<token>@github.com/<repo>.git`; a GHES host keeps its scheme (else
    `https`). The token rides the URL; it is never echoed."""
    host = host.strip()
    bare = host
    for scheme in ("https://", "http://"):
        if bare.startswith(scheme):
            bare = bare[len(scheme) :]
            break
    bare = bare.rstrip("/")
    if not host or bare == "github.com":
        return f"https://{token}@github.com/{repo}.git"
    scheme = "http" if host.startswith("http://") else "https"
    return f"{scheme}://{token}@{bare}/{repo}.git"


def open_pull_request(api_base, repo, payload, token):
    """POST the pull request; return (status, body-bytes). The token rides the
    Authorization header, never a query param, so it is not surfaced."""
    conn = connection(api_base)
    try:
        conn.request(
            "POST",
            api_path(api_base, f"/repos/{repo}/pulls"),
            body=json.dumps(payload),
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "User-Agent": USER_AGENT,
            },
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    # nargs="?" so argparse does not emit its own error on a missing title;
    # the hand-rolled guard below preserves the gitea wording and exit code.
    parser.add_argument("title", nargs="?")
    args = parser.parse_args()

    # --- The title: the single argument. ---
    if not args.title:
        print(
            "open_pr.py: a PR title is required (pass it as the only argument)",
            file=sys.stderr,
        )
        sys.exit(2)
    title = args.title

    # --- Credentials + repo: required, from the inherited environment. Name what
    #     is missing so the config author knows which var to wire (ADR-0023). ---
    token = os.environ.get("GITHUB_TOKEN")
    host = os.environ.get("GITHUB_HOST")
    repo = os.environ.get("GITHUB_REPO")
    missing = [
        name
        for name, value in (
            ("GITHUB_TOKEN", token),
            ("GITHUB_HOST", host),
            ("GITHUB_REPO", repo),
        )
        if not value
    ]
    if missing:
        print(
            f"open_pr.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)
    api_base = github_api_base(host)

    # --- The branches: the current branch is the head; the base defaults to main.
    #     The github trigger emits no base env (GITHUB_PR_BRANCH is the PR's head),
    #     so mirror gitea and read an optional GITHUB_PR_BASE, defaulting to main. ---
    head = current_branch()
    if not head:
        print(
            "open_pr.py: cannot open a PR from a detached HEAD (check out a branch first)",
            file=sys.stderr,
        )
        sys.exit(1)
    pr_base = os.environ.get("GITHUB_PR_BASE") or "main"

    # --- Push the branch over HTTP(S) with token auth. The token rides the push
    #     URL; --quiet keeps it off the normal output, and the URL is not echoed,
    #     so the token is not surfaced. ---
    push = subprocess.run(
        ["git", "push", "--quiet", push_url(host, repo, token), f"HEAD:{head}"]
    )
    if push.returncode != 0:
        print(f"open_pr.py: failed to push branch {head}", file=sys.stderr)
        sys.exit(1)

    # --- A linked issue gets a `Closes #N` line so merging the PR auto-closes it. ---
    issue = issue_number()
    body = f"Closes #{issue}" if issue else ""

    # --- Open the PR. A non-2xx HTTP status is a non-zero exit the agent surfaces. ---
    payload = {"head": head, "base": pr_base, "title": title, "body": body}
    status, _ = open_pull_request(api_base, repo, payload, token)
    if not 200 <= status < 300:
        print(
            f"open_pr.py: failed to open the pull request for {repo} ({head} → {pr_base})",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"open_pr.py: opened a pull request for {repo} ({head} → {pr_base})")


if __name__ == "__main__":
    main()
