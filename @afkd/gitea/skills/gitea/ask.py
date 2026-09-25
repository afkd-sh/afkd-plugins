#!/usr/bin/env python3
# ask.py — ask the human a clarifying question on the active Gitea issue and park
# the run (ADR-0017 §7, ADR-0031 clarification gate).
#
# The "I need input before I can proceed" action of the bundled `gitea` skill (its
# read action is list_comments.py, its post action post_comment.py, its PR action
# open_pr.py). In one step it (1) posts the agent's questions as a comment on the
# issue this run works on, via the Gitea REST API, and (2) writes the park marker
# `park` into the run's scratch dir ($AFKD_SCRATCH_DIR). The WORKFLOW gates
# on that marker (`if run_cmd "test -f $AFKD_SCRATCH_DIR/park" { fail … }`)
# to stop the run at the ask, and the afkd gitea trigger reads it to PARK the issue
# (drops the claim, adds afkd/awaiting-reply, unassigns) instead of failing it;
# once a human replies, a later poll re-claims it for a fresh run. The marker is
# written only AFTER the comment lands, so a failed post never parks an issue with
# no question on it. Scratch is outside any in_worktree copy and fresh per attempt,
# so agent, gate, and trigger name the same file whether or not the run is
# worktreed, and no marker outlives the attempt that wrote it.
#
# It reads the issue number and credentials FROM THE ENVIRONMENT — the env the afkd
# gitea trigger merges into the worker child (crates/gitea/src/common.rs:
# GITEA_TOKEN / GITEA_BASE_URL / GITEA_REPO / GITEA_ISSUE_NUMBER, and
# AFKD_SCRATCH_DIR holding the per-run scratch dir; the number is also written to
# <scratch>/issue/number). It does NOT read its own path from the env
# (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses — ADR-0017
# Revision §5); SKILL.md passes the path on the command line, so this helper owns
# only the API + marker mechanics. The marker name must match
# common::PARK_FILE.

import argparse
import http.client
import json
import os
import sys
import urllib.parse

PLACEHOLDER = "<REPLACE WITH YOUR QUESTIONS>"

# Scratch-relative park marker, written under $AFKD_SCRATCH_DIR. The workflow gates
# on it (`if run_cmd "test -f $AFKD_SCRATCH_DIR/park" { fail … }`) to stop
# the run at the ask, and the trigger reads it to park the issue. Must match
# crates/gitea/src/common.rs::PARK_FILE.
PARK_FILE = "park"


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def issue_number():
    """The issue number: GITEA_ISSUE_NUMBER, then the <scratch>/issue/number file
    fallback, else None. (Unlike post_comment.py this is issue-only — a park acts
    on the originating issue, not a PR.)"""
    number = os.environ.get("GITEA_ISSUE_NUMBER")
    if number:
        return number
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if scratch:
        path = os.path.join(scratch, "issue", "number")
        if os.path.isfile(path):
            with open(path, encoding="utf-8") as f:
                return f.read().strip()
    return None


def post_comment(base, repo, number, text, token):
    """POST one comment to the issue; return (status, body-bytes). The token rides
    the Authorization header, never a query param, so it is not surfaced."""
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
    parser.add_argument("message", nargs="?")
    parser.add_argument(
        "--issue",
        help="issue number to ask on (defaults to the active issue)",
    )
    args = parser.parse_args()

    # --- The questions: the single argument. ---
    if not args.message:
        print(
            "ask.py: a question message is required (pass it as the only argument)",
            file=sys.stderr,
        )
        sys.exit(2)
    text = args.message

    # --- Refuse the SKILL.md placeholder: never post the fill-me-in example. ---
    if text == PLACEHOLDER:
        print(
            "ask.py: refusing to post the placeholder text; substitute your real questions",
            file=sys.stderr,
        )
        sys.exit(2)

    # --- Credentials + repo + the marker's destination: required, from the inherited
    #     environment. Scratch is checked up front, BEFORE the post: a park we cannot
    #     write would leave a question on the issue that nothing ever parks. ---
    token = os.environ.get("GITEA_TOKEN")
    base = os.environ.get("GITEA_BASE_URL")
    repo = os.environ.get("GITEA_REPO")
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    missing = [
        name
        for name, value in (
            ("GITEA_TOKEN", token),
            ("GITEA_BASE_URL", base),
            ("GITEA_REPO", repo),
            ("AFKD_SCRATCH_DIR", scratch),
        )
        if not value
    ]
    if missing:
        print(
            f"ask.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The issue number: --issue if given, else GITEA_ISSUE_NUMBER, then scratch. ---
    number = args.issue or issue_number()
    if not number:
        print(
            "ask.py: no issue number (set GITEA_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/issue/number)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Post the question first; only park once it has landed. ---
    status, _ = post_comment(base, repo, number, text, token)
    if not 200 <= status < 300:
        print(
            f"ask.py: failed to post the question to {repo}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Write the park marker into the run's scratch dir (must match
    #     common::PARK_FILE), which the trigger already created. Never
    #     cwd-relative: under `in_worktree` the cwd is a copy that is torn down before
    #     the trigger reads the marker.
    marker = os.path.join(scratch, PARK_FILE)
    try:
        with open(marker, "w", encoding="utf-8"):
            pass
    except OSError as e:
        print(
            f"ask.py: posted the question but could not write the park marker {marker}: {e}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"ask.py: asked {repo}#{number} and parked the run awaiting a reply")


if __name__ == "__main__":
    main()
