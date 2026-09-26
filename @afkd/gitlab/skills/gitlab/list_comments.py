#!/usr/bin/env python3
# list_comments.py — read the active GitLab issue/MR's notes (ADR-0017 §7, ADR-0041).
#
# The read action of the bundled `gitlab` skill (its post action is
# post_comment.py, its MR action open_mr.py): it lists the notes on the issue or
# merge request this run works on, via the GitLab REST v4 API, and prints each
# note's author, time, and body. The agent calls it through SKILL.md with no
# argument (it reads everything from the environment); on a fetch or auth failure
# it exits non-zero so the agent surfaces the failure rather than retrying blindly.
#
# It reads the issue/MR number and credentials FROM THE ENVIRONMENT — the env the
# afkd gitlab trigger already merges into the worker child (crates/gitlab/src/common.rs:
# GITLAB_TOKEN / GITLAB_BASE_URL / GITLAB_PROJECT / GITLAB_MR_NUMBER / GITLAB_ISSUE_NUMBER,
# and AFKD_SCRATCH_DIR holding the per-run scratch dir; the number is also written
# to <scratch>/mr/number or <scratch>/issue/number). It does NOT read its own path
# from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses —
# ADR-0017 Revision §5); SKILL.md passes the path on the command line, so this
# helper owns only the API mechanics.
#
# This is the gitea reader re-pointed at GitLab REST v4, which diverges in four
# ways (mirroring crates/gitlab/src/client.rs): GITLAB_BASE_URL is resolved to the
# API base by gitlab_api_base (empty → https://gitlab.com; else the trimmed root)
# with a /api/v4 suffix rather than used verbatim; GITLAB_PROJECT is a numeric id
# or a path-with-namespace percent-encoded by encode_project (group/widgets →
# group%2Fwidgets); auth is `PRIVATE-TOKEN: <token>` (not Gitea's `token <t>`); and
# notes live under `issues` or `merge_requests` depending on which number resolved.

import argparse
import http.client
import json
import os
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


def resolve_number():
    """The active number and its item kind: GITLAB_MR_NUMBER → `merge_requests`,
    then GITLAB_ISSUE_NUMBER → `issues`, then the <scratch>/mr/number → `merge_requests`
    and <scratch>/issue/number → `issues` file fallbacks, else (None, None). MR wins
    over an issue at every tier, mirroring gitea's PR-over-issue precedence."""
    for var, kind in (("GITLAB_MR_NUMBER", "merge_requests"), ("GITLAB_ISSUE_NUMBER", "issues")):
        number = os.environ.get(var)
        if number:
            return number, kind
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if scratch:
        for rel, kind in (("mr", "merge_requests"), ("issue", "issues")):
            path = os.path.join(scratch, rel, "number")
            if os.path.isfile(path):
                with open(path, encoding="utf-8") as f:
                    return f.read().strip(), kind
    return None, None


def get_notes(api_base, project, kind, number, token):
    """GET the issue/MR's notes; return (status, body-bytes). The token rides the
    PRIVATE-TOKEN header, never a query param, so it is not surfaced."""
    conn = connection(api_base)
    try:
        conn.request(
            "GET",
            api_path(api_base, f"/projects/{encode_project(project)}/{kind}/{number}/notes"),
            headers={"PRIVATE-TOKEN": token},
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def render(note):
    """One block per note: author + time, then the body.

    Author degrades to the opaque `"unknown"` when `author.username` is absent,
    mirroring the gitea reader (`.user.login // "unknown"`); GitLab exposes the note
    author at `author.username` and the time at `updated_at`, so the two-space/newline
    layout carries over unchanged.
    """
    author = (note.get("author") or {}).get("username") or "unknown"
    updated_at = note.get("updated_at")
    body = note.get("body", "")
    return f"{author}  {updated_at}\n{body}\n"


def main():
    # --- The cross-kind override: --issue N forces kind=issues, --mr N forces
    #     kind=merge_requests. They are mutually exclusive (argparse errors, exit 2,
    #     when both are given); each sets BOTH the number and the path kind, so an
    #     active-MR run can retarget an issue and vice versa. Neither → today's
    #     active resolution. ---
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--issue", help="issue number to read (kind=issues)")
    group.add_argument("--mr", help="merge request number to read (kind=merge_requests)")
    args = parser.parse_args()

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
            f"list_comments.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)
    api_base = gitlab_api_base(base_url)

    # --- The issue/MR number: an override flag if given (each carries its own
    #     kind), else GITLAB_MR_NUMBER, then GITLAB_ISSUE_NUMBER, then the scratch
    #     file fallbacks. Its origin also selects the path kind. ---
    if args.mr:
        number, kind = args.mr, "merge_requests"
    elif args.issue:
        number, kind = args.issue, "issues"
    else:
        number, kind = resolve_number()
    if not number:
        print(
            "list_comments.py: no issue/MR number (set GITLAB_MR_NUMBER or "
            "GITLAB_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{mr,issue}/number)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Fetch the issue/MR's notes. A non-2xx (e.g. an auth failure) is a non-zero
    #     exit the agent surfaces. ---
    status, body = get_notes(api_base, project, kind, number, token)
    if not 200 <= status < 300:
        print(
            f"list_comments.py: failed to read comments for {project}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- GitLab returns a top-level array of notes; an issue/MR with no notes is
    #     success, not an error. ---
    notes = json.loads(body)
    if not notes:
        print(f"no comments on {project}#{number}")
        sys.exit(0)

    print("\n".join(render(n) for n in notes))


if __name__ == "__main__":
    main()
