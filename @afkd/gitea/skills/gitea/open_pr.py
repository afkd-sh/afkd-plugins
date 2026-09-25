#!/usr/bin/env python3
# open_pr.py — push the current branch and open a Gitea pull request (ADR-0017 §7, ADR-0031).
#
# The PR action of the bundled `gitea` skill (its read/post actions are
# list_comments.py / post_comment.py; ADR-0031 Capability 2): it pushes the branch
# the run committed on and opens a pull request linking back to the originating
# issue. The agent calls it through SKILL.md with the PR title as the sole
# argument; on a push or create failure it exits non-zero so the agent surfaces
# the failure rather than retrying blindly.
#
# It reads the credentials, repo, and originating issue FROM THE ENVIRONMENT — the
# env the afkd gitea trigger already merges into the worker child
# (crates/gitea/src/common.rs: GITEA_TOKEN / GITEA_BASE_URL / GITEA_REPO /
# GITEA_ISSUE_NUMBER, and AFKD_SCRATCH_DIR holding the per-run scratch dir; the
# issue number is also written to <scratch>/issue/number). It does NOT read its own
# path from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses
# — ADR-0017 Revision §5); SKILL.md passes the path on the command line, so this
# helper owns only the push + API mechanics.
#
# Unlike the sh version it replaces, it builds the JSON body with the `json`
# stdlib module and opens the PR over `http.client` (no jq, no curl). "Pure
# stdlib" constrains HTTP/JSON only: there is no stdlib git, so branch resolution
# and the push still shell out to the `git` CLI via `subprocess`, exactly as the
# sh did. It reads GITEA_BASE_URL directly for both the API and the push URL.

import argparse
import http.client
import json
import os
import subprocess
import sys
import urllib.parse


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def issue_number():
    """The originating issue: GITEA_ISSUE_NUMBER, else <scratch>/issue/number if
    present, else None."""
    issue = os.environ.get("GITEA_ISSUE_NUMBER")
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


def commits_over_base(base):
    """How many commits HEAD is ahead of `origin/<base>`, or None if that cannot be
    determined (the base ref is not present locally). Used to refuse an empty PR."""
    result = subprocess.run(
        ["git", "rev-list", "--count", f"origin/{base}..HEAD"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    out = result.stdout.strip()
    return int(out) if out.isdigit() else None


def push_url(base, repo, token):
    """The token-authenticated push URL for `repo`. The token rides the URL; it is
    never echoed, and the scheme is taken from `base` (http vs https)."""
    parts = urllib.parse.urlsplit(base)
    return f"{parts.scheme}://{token}@{parts.netloc}/{repo}.git"


def open_pull_request(base, repo, payload, token):
    """POST the pull request; return (status, body-bytes). The token rides the
    Authorization header, never a query param, so it is not surfaced."""
    conn = connection(base)
    try:
        conn.request(
            "POST",
            f"/api/v1/repos/{repo}/pulls",
            body=json.dumps(payload),
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
    # nargs="?" so argparse does not emit its own error on a missing title;
    # the hand-rolled guard below preserves the sh's exact wording and exit code.
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

    # Reject a placeholder title. The agent must compose a real title for the change;
    # the SKILL.md example is a fill-in, and copying it verbatim (or passing a generic
    # stand-in / an un-replaced `‹…›` guillemet form) yields a uselessly-titled PR.
    # Match case-insensitively on the stripped title so trivial variants are caught.
    normalized = title.strip().casefold()
    placeholder = (
        normalized in {"your pr title", "pr title", "title", "your title", "<title>"}
        or (normalized.startswith("‹") and normalized.endswith("›"))
    )
    if placeholder:
        print(
            f"open_pr.py: {title!r} is a placeholder, not a real PR title — pass a "
            "concise summary of the actual change",
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
            f"open_pr.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The branches: the current branch is the head; the base defaults to main. ---
    head = current_branch()
    if not head:
        print(
            "open_pr.py: cannot open a PR from a detached HEAD (check out a branch first)",
            file=sys.stderr,
        )
        sys.exit(1)
    pr_base = os.environ.get("GITEA_PR_BASE") or "main"

    # Refuse an empty PR: if HEAD carries no commits over the base, opening a PR only
    # creates a 0-commit, un-mergeable placeholder — the symptom of commits landing on
    # a branch that is not HEAD. Only refuse when we can positively determine 0; an
    # unresolvable base ref returns None and is left to the API to judge.
    if commits_over_base(pr_base) == 0:
        print(
            f"open_pr.py: HEAD has no commits over origin/{pr_base} — refusing to open "
            "an empty pull request (commit the change on this branch first)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Push the branch over HTTP(S) with token auth. The token rides the push
    #     URL; --quiet keeps it off the normal output, and the URL is not echoed,
    #     so the token is not surfaced. ---
    push = subprocess.run(
        ["git", "push", "--quiet", push_url(base, repo, token), f"HEAD:{head}"]
    )
    if push.returncode != 0:
        print(f"open_pr.py: failed to push branch {head}", file=sys.stderr)
        sys.exit(1)

    # --- A linked issue gets a `Closes #N` line so merging the PR auto-closes it. ---
    issue = issue_number()
    body = f"Closes #{issue}" if issue else ""

    # --- Open the PR. A non-2xx HTTP status is a non-zero exit the agent surfaces. ---
    payload = {"head": head, "base": pr_base, "title": title, "body": body}
    status, raw = open_pull_request(base, repo, payload, token)
    if not 200 <= status < 300:
        print(
            f"open_pr.py: failed to open the pull request for {repo} ({head} → {pr_base})",
            file=sys.stderr,
        )
        sys.exit(1)

    # The created PR's number and web URL, parsed from the API response, so the agent
    # can link the originating issue back to the PR in its follow-up comment (a plain
    # `Closes #N` in the body only cross-references the issue → PR direction). Defensive:
    # a body that does not parse still succeeds — the PR is open — just without the ref.
    number, html_url = None, None
    try:
        created = json.loads(raw)
        number = created.get("number")
        html_url = created.get("html_url")
    except (ValueError, AttributeError):
        pass

    ref = f" #{number}" if number else ""
    url = f": {html_url}" if html_url else ""
    print(f"open_pr.py: opened pull request{ref} for {repo} ({head} → {pr_base}){url}")


if __name__ == "__main__":
    main()
