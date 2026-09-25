#!/usr/bin/env python3
# list_attachments.py — list the active Gitea issue/PR's assets (ADR-0017 §7, ADR-0031).
#
# The read half of the bundled `gitea` skill's attachment surface (fetch is
# fetch_attachment.py, post is post_attachment.py): it lists the assets on the
# issue or PR this run works on AND on each of its comments, one
# `id  name  size  browser_download_url` line each, so the agent can see a
# screenshot or log a human attached — Gitea stores such a file as a structured
# issue/comment ASSET, not as comment text — and pick the id to fetch. The agent
# calls it through SKILL.md with no argument (it reads everything from the
# environment); on a fetch or auth failure it exits non-zero so the agent surfaces
# the failure rather than retrying blindly.
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
# The endpoint shapes are pinned to Gitea's documented assets API, not observed
# against a live server (the builder container cannot reach one): issue/PR assets at
# `/issues/{index}/assets`, comment assets at `/issues/comments/{id}/assets`, each
# asset carrying `id`/`name`/`size`/`browser_download_url`. A wrong assumption
# surfaces as a failed e2e or a non-2xx at runtime, never a silent mis-read. It
# reads GITEA_BASE_URL directly — Gitea routes all API traffic through it and the
# trigger tests point it at a loopback host, so no separate base override is needed.

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


def get_json(base, path, token):
    """GET `path`; return (status, body-bytes). The token rides the Authorization
    header, never a query param, so it is not surfaced."""
    conn = connection(base)
    try:
        conn.request("GET", path, headers={"Authorization": f"token {token}"})
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def collect_assets(base, repo, number, token):
    """Return a flat list of the issue/PR's assets AND each of its comments' assets.

    Three request shapes: (1) the issue/PR's own assets; (2) the issue/PR's comments,
    to learn each comment id; (3) per comment, that comment's assets. A non-2xx on
    the issue-assets or the comments call raises RuntimeError (the caller exits
    non-zero); a comment whose own assets call fails is skipped rather than aborting
    the whole listing, since one bad comment should not blind the agent to the rest.
    """
    assets = []

    status, body = get_json(base, f"/api/v1/repos/{repo}/issues/{number}/assets", token)
    if not 200 <= status < 300:
        raise RuntimeError(f"failed to read assets for {repo}#{number}")
    assets.extend(json.loads(body))

    status, body = get_json(
        base, f"/api/v1/repos/{repo}/issues/{number}/comments", token
    )
    if not 200 <= status < 300:
        raise RuntimeError(f"failed to read comments for {repo}#{number}")
    for comment in json.loads(body):
        comment_id = comment.get("id")
        if comment_id is None:
            continue
        status, body = get_json(
            base, f"/api/v1/repos/{repo}/issues/comments/{comment_id}/assets", token
        )
        if 200 <= status < 300:
            assets.extend(json.loads(body))

    return assets


def main():
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
            f"list_attachments.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The issue/PR number: GITEA_PR_NUMBER, then GITEA_ISSUE_NUMBER, then
    #     the scratch file fallbacks. ---
    number = issue_number()
    if not number:
        print(
            "list_attachments.py: no issue/PR number (set GITEA_PR_NUMBER or "
            "GITEA_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{pr,issue}/number)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Enumerate the issue's and its comments' assets. A non-2xx (e.g. an auth
    #     failure) is a non-zero exit the agent surfaces; the token is never echoed. ---
    try:
        assets = collect_assets(base, repo, number, token)
    except RuntimeError as err:
        print(f"list_attachments.py: {err}", file=sys.stderr)
        sys.exit(1)

    # --- An issue/PR with no assets is success, not an error (most carry none). ---
    if not assets:
        print(f"no attachments on {repo}#{number}")
        sys.exit(0)

    for asset in assets:
        print(
            f"{asset.get('id')}  {asset.get('name')}  "
            f"{asset.get('size')}  {asset.get('browser_download_url')}"
        )


if __name__ == "__main__":
    main()
