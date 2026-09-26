#!/usr/bin/env python3
# open_mr.py — push the current branch and open a GitLab merge request (ADR-0017 §7, ADR-0041).
#
# The MR action of the bundled `gitlab` skill (its read/post actions are
# list_comments.py / post_comment.py): it pushes the branch the run committed on
# and opens a merge request linking back to the originating issue. The agent calls
# it through SKILL.md with the MR title as the sole argument; on a push or create
# failure it exits non-zero so the agent surfaces the failure rather than retrying
# blindly.
#
# It reads the credentials, project, and originating issue FROM THE ENVIRONMENT —
# the env the afkd gitlab trigger already merges into the worker child
# (crates/gitlab/src/common.rs: GITLAB_TOKEN / GITLAB_BASE_URL / GITLAB_PROJECT /
# GITLAB_ISSUE_NUMBER, and AFKD_SCRATCH_DIR holding the per-run scratch dir; the
# issue number is also written to <scratch>/issue/number). It does NOT read its own
# path from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses
# — ADR-0017 Revision §5); SKILL.md passes the path on the command line, so this
# helper owns only the push + API mechanics.
#
# This is the gitea MR opener re-pointed at GitLab REST v4, which diverges in four
# ways (mirroring crates/gitlab/src/client.rs): GITLAB_BASE_URL is resolved to the
# API base by gitlab_api_base (empty → https://gitlab.com; else the trimmed root)
# with a /api/v4 suffix rather than used verbatim; GITLAB_PROJECT is a numeric id
# or a path-with-namespace percent-encoded by encode_project (group/widgets →
# group%2Fwidgets); auth is `PRIVATE-TOKEN: <token>` (not Gitea's `token <t>`); and
# the create endpoint is `POST /projects/{enc}/merge_requests` with source/target
# branch fields. The git *push* URL is derived from GITLAB_BASE_URL's netloc and the
# *raw* (un-encoded) GITLAB_PROJECT, with `oauth2:<token>@` auth. "Pure stdlib"
# constrains HTTP/JSON only — there is no stdlib git, so branch resolution and the
# push still shell out to the `git` CLI via `subprocess`, exactly as gitea does.

import argparse
import http.client
import json
import os
import subprocess
import sys
import urllib.parse


def gitlab_api_base(base_url):
    """Resolve GITLAB_BASE_URL to the REST v4 root every endpoint hangs off, the way
    the Rust client's gitlab_api_base does (crates/gitlab/src/client.rs:336). Empty
    falls back to `https://gitlab.com`; otherwise the trimmed root keeps its scheme;
    a trailing slash is trimmed. Cloud and self-managed are identical (both under
    `/api/v4`)."""
    b = base_url.strip()
    root = "https://gitlab.com" if not b else b.rstrip("/")
    return f"{root}/api/v4"


def encode_project(project):
    """Render GITLAB_PROJECT as its `:id` path segment, the way the Rust client's
    encode_project does (crates/gitlab/src/client.rs:350): an all-ASCII-digit id
    passes through; anything else (a path-with-namespace) is percent-encoded, escaping
    every byte outside the unreserved set (`A–Z a–z 0–9 - _ . ~`), so `group/widgets`
    becomes `group%2Fwidgets`."""
    if project and all(0x30 <= b <= 0x39 for b in project.encode("utf-8")):
        return project
    unreserved = set(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~")
    out = []
    for b in project.encode("utf-8"):
        out.append(chr(b) if b in unreserved else f"%{b:02X}")
    return "".join(out)


def api_path(api_base, suffix):
    """Join the API base's path prefix with an endpoint suffix. The base's path is
    `/api/v4` (→ `/api/v4/projects/…`), so the version prefix rides the resolved
    base — never a literal `/api/v1`."""
    return urllib.parse.urlsplit(api_base).path + suffix


def connection(api_base):
    """Open an HTTP(S) connection to the API host parsed from `api_base`."""
    parts = urllib.parse.urlsplit(api_base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def issue_number():
    """The originating issue: GITLAB_ISSUE_NUMBER, else <scratch>/issue/number if
    present, else None."""
    issue = os.environ.get("GITLAB_ISSUE_NUMBER")
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


def push_url(base_url, project, token):
    """The token-authenticated git push URL for `project`, derived from
    GITLAB_BASE_URL's netloc and the *raw* (un-encoded) GITLAB_PROJECT — NOT the
    /api/v4 API base. Empty base → `gitlab.com` over https; otherwise the base's
    scheme and netloc are kept. GitLab tokens push as `oauth2:<token>@…`; the token
    rides the URL and is never echoed."""
    b = base_url.strip()
    root = "https://gitlab.com" if not b else b.rstrip("/")
    parts = urllib.parse.urlsplit(root)
    scheme = parts.scheme or "https"
    return f"{scheme}://oauth2:{token}@{parts.netloc}/{project}.git"


def open_merge_request(api_base, project, payload, token):
    """POST the merge request; return (status, body-bytes). The token rides the
    PRIVATE-TOKEN header, never a query param, so it is not surfaced."""
    conn = connection(api_base)
    try:
        conn.request(
            "POST",
            api_path(api_base, f"/projects/{encode_project(project)}/merge_requests"),
            body=json.dumps(payload),
            headers={
                "PRIVATE-TOKEN": token,
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
    # the hand-rolled guard below preserves the gitea wording and exit code.
    parser.add_argument("title", nargs="?")
    args = parser.parse_args()

    # --- The title: the single argument. ---
    if not args.title:
        print(
            "open_mr.py: an MR title is required (pass it as the only argument)",
            file=sys.stderr,
        )
        sys.exit(2)
    title = args.title

    # --- Credentials + project: required, from the inherited environment. Name what
    #     is missing so the config author knows which var to wire (ADR-0023). ---
    token = os.environ.get("GITLAB_TOKEN")
    base_url = os.environ.get("GITLAB_BASE_URL")
    project = os.environ.get("GITLAB_PROJECT")
    missing = [
        name
        for name, value in (
            ("GITLAB_TOKEN", token),
            ("GITLAB_BASE_URL", base_url),
            ("GITLAB_PROJECT", project),
        )
        if not value
    ]
    if missing:
        print(
            f"open_mr.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)
    api_base = gitlab_api_base(base_url)

    # --- The branches: the current branch is the source; the target defaults to main.
    #     The gitlab trigger emits no target env (GITLAB_MR_BRANCH is the reviewed MR's
    #     source), so read an optional GITLAB_MR_TARGET, defaulting to main. ---
    source = current_branch()
    if not source:
        print(
            "open_mr.py: cannot open an MR from a detached HEAD (check out a branch first)",
            file=sys.stderr,
        )
        sys.exit(1)
    target = os.environ.get("GITLAB_MR_TARGET") or "main"

    # --- Push the branch over HTTP(S) with token auth. The token rides the push URL;
    #     --quiet keeps it off the normal output, and the URL is not echoed, so the
    #     token is not surfaced. ---
    push = subprocess.run(
        ["git", "push", "--quiet", push_url(base_url, project, token), f"HEAD:{source}"]
    )
    if push.returncode != 0:
        print(f"open_mr.py: failed to push branch {source}", file=sys.stderr)
        sys.exit(1)

    # --- A linked issue gets a `Closes #N` line so merging the MR auto-closes it. ---
    issue = issue_number()
    body = f"Closes #{issue}" if issue else ""

    # --- Open the MR. A non-2xx HTTP status is a non-zero exit the agent surfaces. ---
    payload = {
        "source_branch": source,
        "target_branch": target,
        "title": title,
        "description": body,
    }
    status, _ = open_merge_request(api_base, project, payload, token)
    if not 200 <= status < 300:
        print(
            f"open_mr.py: failed to open the merge request for {project} ({source} → {target})",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"open_mr.py: opened the merge request for {project} ({source} → {target})")


if __name__ == "__main__":
    main()
