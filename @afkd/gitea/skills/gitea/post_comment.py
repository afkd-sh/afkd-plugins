#!/usr/bin/env python3
# post_comment.py — post a comment to the active Gitea issue/PR (ADR-0017 §7, ADR-0031).
#
# The post action of the bundled `gitea` skill (its read action is
# list_comments.py, its PR action open_pr.py): it posts a single comment to the
# issue or PR this run works on, via the Gitea REST API. The agent calls it
# through SKILL.md with the message as the sole argument; on a post failure it
# exits non-zero so the agent surfaces the failure rather than retrying blindly.
# This reply is also what advances the PR-review trigger's watermark, so the round
# is only "addressed" once it lands.
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
# Unlike the sh version it replaces, it builds the JSON body with the `json`
# stdlib module (no jq): the body is arbitrary text, so quotes, newlines, and
# unicode survive. It reads GITEA_BASE_URL directly — Gitea already routes all
# API traffic through it and the trigger tests point it at a loopback host, so no
# separate test-only base override is needed.

import argparse
import http.client
import json
import os
import sys
import urllib.parse

PLACEHOLDER = "<REPLACE WITH YOUR COMMENT>"


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


def post_comment(base, repo, number, text, token):
    """POST one comment to the issue/PR; return (status, body-bytes). The token
    rides the Authorization header, never a query param, so it is not surfaced."""
    payload = json.dumps({"body": text})
    conn = connection(base)
    try:
        conn.request(
            "POST",
            f"/api/v1/repos/{repo}/issues/{number}/comments",
            body=payload,
            headers={
                "Authorization": f"token {token}",
                "Content-Type": "application/json",
            },
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    # nargs="?" so argparse does not emit its own error on a missing message;
    # the hand-rolled guard below preserves the sh's exact wording and exit code.
    parser.add_argument("message", nargs="?")
    parser.add_argument(
        "--issue",
        help="issue/PR number to post to (defaults to the active issue/PR)",
    )
    args = parser.parse_args()

    # --- The message: the single argument. ---
    if not args.message:
        print(
            "post_comment.py: a comment message is required (pass it as the only argument)",
            file=sys.stderr,
        )
        sys.exit(2)
    text = args.message

    # --- Refuse the SKILL.md placeholder: never post the fill-me-in example. ---
    if text == PLACEHOLDER:
        print(
            "post_comment.py: refusing to post the placeholder text; substitute your real comment",
            file=sys.stderr,
        )
        sys.exit(2)

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
            f"post_comment.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The issue/PR number: --issue if given, else GITEA_PR_NUMBER, then
    #     GITEA_ISSUE_NUMBER, then the scratch file fallbacks. ---
    number = args.issue or issue_number()
    if not number:
        print(
            "post_comment.py: no issue/PR number (set GITEA_PR_NUMBER or "
            "GITEA_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{pr,issue}/number)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Post the comment. A non-2xx (e.g. an auth failure or a refused post) is
    #     a non-zero exit the agent surfaces; the token is never echoed. ---
    status, _ = post_comment(base, repo, number, text, token)
    if not 200 <= status < 300:
        print(
            f"post_comment.py: failed to post the comment to {repo}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"post_comment.py: posted a comment to {repo}#{number}")


if __name__ == "__main__":
    main()
