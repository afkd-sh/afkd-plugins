#!/usr/bin/env python3
# list_comments.py — read the active Gitea issue/PR's comments (ADR-0017 §7, ADR-0031).
#
# The read action of the bundled `gitea` skill (its post action is
# post_comment.py, its PR action open_pr.py): it lists the comments on the issue
# or PR this run works on, via the Gitea REST API, and prints each comment's
# author, time, and body. The agent calls it through SKILL.md with no argument
# (it reads everything from the environment); on a fetch or auth failure it exits
# non-zero so the agent surfaces the failure rather than retrying blindly.
#
# It reads the issue/PR number and credentials FROM THE ENVIRONMENT — the env the
# afkd gitea trigger already merges into the worker child (crates/gitea/src/common.rs:
# GITEA_TOKEN / GITEA_BASE_URL / GITEA_REPO / GITEA_PR_NUMBER / GITEA_ISSUE_NUMBER,
# and AFKD_SCRATCH_DIR holding the per-run scratch dir; the number is also written
# to <scratch>/pr/number or <scratch>/issue/number). It does NOT read its own path
# from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses —
# ADR-0017 Revision §5); SKILL.md passes the path on the command line, so this
# helper owns only the API mechanics.
#
# Unlike the sh version it replaces, it parses the comments array with the `json`
# stdlib module (no jq): comment bodies are arbitrary (quotes, newlines, unicode),
# so a hand-parse would corrupt them. It reads GITEA_BASE_URL directly — Gitea
# already routes all API traffic through it and the trigger tests point it at a
# loopback host, so no separate test-only base override is needed.

import argparse
import http.client
import json
import os
import sys
import urllib.parse


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def issue_number():
    """The issue/PR number: GITEA_PR_NUMBER, then GITEA_ISSUE_NUMBER, then the
    <scratch>/pr/number → <scratch>/issue/number file fallbacks, else None."""
    number = os.environ.get("GITEA_PR_NUMBER") or os.environ.get("GITEA_ISSUE_NUMBER")
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


def get_comments(base, repo, number, token):
    """GET the issue/PR's comments; return (status, body-bytes). The token rides
    the Authorization header, never a query param, so it is not surfaced."""
    conn = connection(base)
    try:
        conn.request(
            "GET",
            f"/api/v1/repos/{repo}/issues/{number}/comments",
            headers={"Authorization": f"token {token}"},
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def render(comment):
    """One block per comment: author + time, then the body.

    Author degrades to the opaque `"unknown"` when `user.login` is absent,
    mirroring the sh/jq template (`.user.login // "unknown"`); the two-space/
    newline layout mirrors it too.
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
    token = os.environ.get("GITEA_TOKEN")
    base = os.environ.get("GITEA_BASE_URL")
    repo = os.environ.get("GITEA_REPO")
    missing = [
        name
        for name, value in (
            ("GITEA_TOKEN", token),
            ("GITEA_BASE_URL", base),
            ("GITEA_REPO", repo),
        )
        if not value
    ]
    if missing:
        print(
            f"list_comments.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The issue/PR number: --issue if given, else GITEA_PR_NUMBER, then
    #     GITEA_ISSUE_NUMBER, then the scratch file fallbacks. ---
    number = args.issue or issue_number()
    if not number:
        print(
            "list_comments.py: no issue/PR number (set GITEA_PR_NUMBER or "
            "GITEA_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{pr,issue}/number)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Fetch the issue/PR's comments. A non-2xx (e.g. an auth failure) is a
    #     non-zero exit the agent surfaces. ---
    status, body = get_comments(base, repo, number, token)
    if not 200 <= status < 300:
        print(
            f"list_comments.py: failed to read comments for {repo}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Gitea returns a top-level array of comments; an issue/PR with no
    #     comments is success, not an error. ---
    comments = json.loads(body)
    if not comments:
        print(f"no comments on {repo}#{number}")
        sys.exit(0)

    print("\n".join(render(c) for c in comments))


if __name__ == "__main__":
    main()
