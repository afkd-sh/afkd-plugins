#!/usr/bin/env python3
# post_attachment.py — attach a local file to the active GitLab issue/MR (ADR-0017 §7, ADR-0041).
#
# The post half of the bundled `gitlab` skill's attachment surface (fetch is
# fetch_attachment.py): it uploads one local file to the project, then posts a
# **note** embedding the markdown snippet the upload returns, so the agent can
# hand a human the artifact itself — an annotated screenshot, a rendered diff, a
# report — instead of describing it. The file path is a REQUIRED positional
# argument; everything else defaults from the environment. On any failure it exits
# non-zero so the agent surfaces the failure rather than retrying blindly.
#
# TWO-STEP UPLOAD. GitLab has no single "attach to issue" call: step one POSTs the
# file to `/projects/:id/uploads` (a `multipart/form-data` body carrying the bytes
# in GitLab's `file` field) and gets back a JSON `markdown` snippet
# (e.g. `![shot.png](/uploads/<hash>/shot.png)`); step two posts a note whose body
# IS that snippet, so the file renders on the issue/MR. The token rides the same
# `PRIVATE-TOKEN` header as every other call, never a query param. These shapes are
# pinned to GitLab's documented API, not observed against a live server (the
# builder container cannot reach one) — a wrong field name or an absent `markdown`
# field surfaces as a failed e2e or an explicit non-zero exit, never silently.
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
# Like the sibling helpers it uses only the Python standard library (no curl): the
# stdlib does not build multipart bodies, so the body is assembled by hand (uuid4
# boundary, binary-safe file part). There is no local size cap — GitLab's upload
# limit is server-configured, so the server stays the authority and a rejected
# upload surfaces as a non-2xx.

import argparse
import http.client
import json
import os
import sys
import urllib.parse
import uuid


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


def multipart_body(filename, content):
    """Build a `multipart/form-data` body by hand; return (content_type, bytes).

    The stdlib does not assemble multipart bodies, so we do: a random boundary
    (`uuid4().hex`, so it cannot collide with the file content) and one file part
    named `file` (GitLab's uploads field) carrying `content` VERBATIM as
    application/octet-stream (binary-safe).
    """
    boundary = uuid.uuid4().hex
    parts = [
        f"--{boundary}".encode(),
        f'Content-Disposition: form-data; name="file"; filename="{filename}"'.encode(),
        b"Content-Type: application/octet-stream",
        b"",
        content,
        f"--{boundary}--".encode(),
        b"",
    ]
    body = b"\r\n".join(parts)
    return f"multipart/form-data; boundary={boundary}", body


def upload(api_base, project, filename, content, token):
    """POST the file to `/projects/:id/uploads`; return (status, body-bytes). The
    file bytes travel VERBATIM in the multipart body; the token rides the
    PRIVATE-TOKEN header, never a query param, so it is not surfaced."""
    content_type, body = multipart_body(filename, content)
    conn = connection(api_base)
    try:
        conn.request(
            "POST",
            api_path(api_base, f"/projects/{encode_project(project)}/uploads"),
            body,
            {
                "PRIVATE-TOKEN": token,
                "Content-Type": content_type,
            },
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def post_note(api_base, project, kind, number, text, token):
    """POST one note to the issue/MR; return (status, body-bytes). The token rides
    the PRIVATE-TOKEN header, never a query param, so it is not surfaced."""
    payload = json.dumps({"body": text})
    conn = connection(api_base)
    try:
        conn.request(
            "POST",
            api_path(api_base, f"/projects/{encode_project(project)}/{kind}/{number}/notes"),
            body=payload,
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
    # nargs="?" so argparse does not emit its own error on a missing path; the
    # hand-rolled guard below preserves a clear wording and a dedicated exit code.
    parser.add_argument("path", nargs="?")
    args = parser.parse_args()

    # --- The file path: the single positional argument. ---
    if not args.path:
        print(
            "post_attachment.py: a file path is required (pass it as the only positional argument)",
            file=sys.stderr,
        )
        sys.exit(2)

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
            f"post_attachment.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)
    api_base = gitlab_api_base(base_url)

    # --- The issue/MR number: GITLAB_MR_NUMBER, then GITLAB_ISSUE_NUMBER, then the
    #     scratch file fallbacks. Its origin also selects the path kind. ---
    number, kind = resolve_number()
    if not number:
        print(
            "post_attachment.py: no issue/MR number (set GITLAB_MR_NUMBER or "
            "GITLAB_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{mr,issue}/number)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Pre-flight file validation, before opening/reading, so a bad path never
    #     reaches the network. There is no local size cap — GitLab's limit is
    #     server-configured, so a too-large file is rejected by the server. ---
    path = args.path
    if not os.path.exists(path):
        print(f"post_attachment.py: no such file: {path}", file=sys.stderr)
        sys.exit(1)
    if not os.path.isfile(path):
        print(f"post_attachment.py: not a regular file: {path}", file=sys.stderr)
        sys.exit(1)

    # --- Step one: upload the file bytes (binary, verbatim) to the project. A
    #     non-2xx is a non-zero exit; the token is never echoed. ---
    filename = os.path.basename(path)
    with open(path, "rb") as handle:
        content = handle.read()
    status, body = upload(api_base, project, filename, content, token)
    if not 200 <= status < 300:
        print(
            f"post_attachment.py: failed to upload {filename} to {project}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The upload response carries a `markdown` embed snippet; an absent field is
    #     a loud non-zero exit, not a silently empty note. ---
    markdown = json.loads(body).get("markdown")
    if not markdown:
        print(
            f"post_attachment.py: upload of {filename} returned no markdown embed",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Step two: post a note embedding the snippet, so the file renders on the
    #     issue/MR. A non-2xx is a non-zero exit; the token is never echoed. ---
    status, _ = post_note(api_base, project, kind, number, markdown, token)
    if not 200 <= status < 300:
        print(
            f"post_attachment.py: failed to post {filename} to {project}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"post_attachment.py: posted {filename} to {project}#{number}")


if __name__ == "__main__":
    main()
