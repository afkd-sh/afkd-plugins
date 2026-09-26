#!/usr/bin/env python3
# list_comments.py — read the active GitHub issue/PR's comments (ADR-0017 §7, ADR-0041).
#
# The read action of the bundled `github` skill (its post action is
# post_comment.py, its PR action open_pr.py): it lists the comments on the issue
# or PR this run works on, via the GitHub REST v3 API, and prints each comment's
# author, time, and body. The agent calls it through SKILL.md with no argument
# (it reads everything from the environment); on a fetch or auth failure it exits
# non-zero so the agent surfaces the failure rather than retrying blindly.
#
# It reads the issue/PR number and credentials FROM THE ENVIRONMENT — the env the
# afkd github trigger already merges into the worker child (crates/github/src/common.rs:
# GITHUB_TOKEN / GITHUB_HOST / GITHUB_REPO / GITHUB_PR_NUMBER / GITHUB_ISSUE_NUMBER,
# and AFKD_SCRATCH_DIR holding the per-run scratch dir; the number is also written
# to <scratch>/pr/number or <scratch>/issue/number). It does NOT read its own path
# from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses —
# ADR-0017 Revision §5); SKILL.md passes the path on the command line, so this
# helper owns only the API mechanics.
#
# This is the gitea reader re-pointed at GitHub REST v3, which diverges in four
# ways (mirroring crates/github/src/client.rs): GITHUB_HOST is resolved to the API
# base by github_api_base (cloud → https://api.github.com; any other host → GHES
# <scheme|https>://host/api/v3) rather than used verbatim; endpoints hang off that
# base with NO /api/v1 prefix; auth is `Bearer <token>` (not Gitea's `token <t>`);
# and every request carries a non-empty User-Agent (GitHub REST 403s without one).

import argparse
import http.client
import json
import os
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
    """The issue/PR number: GITHUB_PR_NUMBER, then GITHUB_ISSUE_NUMBER, then the
    <scratch>/pr/number → <scratch>/issue/number file fallbacks, else None."""
    number = os.environ.get("GITHUB_PR_NUMBER") or os.environ.get("GITHUB_ISSUE_NUMBER")
    if number:
        return number
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if scratch:
        for rel in ("pr", "issue"):
            path = os.path.join(scratch, rel, "number")
            if os.path.isfile(path):
                with open(path, encoding="utf-8") as f:
                    return f.read().strip()
    return None


def get_comments(api_base, repo, number, token):
    """GET the issue/PR's comments; return (status, body-bytes). The token rides
    the Authorization header, never a query param, so it is not surfaced."""
    conn = connection(api_base)
    try:
        conn.request(
            "GET",
            api_path(api_base, f"/repos/{repo}/issues/{number}/comments"),
            headers={
                "Authorization": f"Bearer {token}",
                "User-Agent": USER_AGENT,
            },
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def render(comment):
    """One block per comment: author + time, then the body.

    Author degrades to the opaque `"unknown"` when `user.login` is absent,
    mirroring the gitea reader (`.user.login // "unknown"`); GitHub exposes the
    same `user.login` / `updated_at` / `body` fields on issue comments, so the
    two-space/newline layout carries over unchanged.
    """
    user = comment.get("user") or {}
    author = user.get("login") or "unknown"
    updated_at = comment.get("updated_at")
    body = comment.get("body", "")
    return f"{author}  {updated_at}\n{body}\n"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--issue",
        help="issue/PR number to read (defaults to the active issue/PR)",
    )
    args = parser.parse_args()

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
            f"list_comments.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)
    api_base = github_api_base(host)

    # --- The issue/PR number: --issue if given, else GITHUB_PR_NUMBER, then
    #     GITHUB_ISSUE_NUMBER, then the scratch file fallbacks. ---
    number = args.issue or issue_number()
    if not number:
        print(
            "list_comments.py: no issue/PR number (set GITHUB_PR_NUMBER or "
            "GITHUB_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{pr,issue}/number)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Fetch the issue/PR's comments. A non-2xx (e.g. an auth failure) is a
    #     non-zero exit the agent surfaces. ---
    status, body = get_comments(api_base, repo, number, token)
    if not 200 <= status < 300:
        print(
            f"list_comments.py: failed to read comments for {repo}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- GitHub returns a top-level array of comments; an issue/PR with no
    #     comments is success, not an error. ---
    comments = json.loads(body)
    if not comments:
        print(f"no comments on {repo}#{number}")
        sys.exit(0)

    print("\n".join(render(c) for c in comments))


if __name__ == "__main__":
    main()
