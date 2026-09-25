#!/usr/bin/env python3
# post_attachment.py — attach a local file to the active Gitea issue/PR (ADR-0017 §7, ADR-0031).
#
# The post half of the bundled `gitea` skill's attachment surface (listing is
# list_attachments.py, fetch is fetch_attachment.py): it uploads one local file as
# an asset on the issue or PR this run works on, so the agent can hand a human the
# artifact itself — an annotated screenshot, a rendered diff, a report — instead of
# describing it. The file path is a REQUIRED positional argument; --name gives the
# asset an optional display name (default: the file's basename). On an upload
# failure it exits non-zero so the agent surfaces the failure rather than retrying
# blindly.
#
# AUTH & UPLOAD SHAPE. The upload POSTs a `multipart/form-data` body with a single
# file part named `attachment` (Gitea's field) to `/issues/{index}/assets`; the
# display name rides as the `?name=` query parameter (Gitea's documented param),
# not a form part. The token rides the same `Authorization: token …` header as
# every other call. These shapes are pinned to Gitea's documented API, not observed
# against a live server (the builder container cannot reach one) — a wrong
# assumption surfaces as a failed e2e or a non-2xx at runtime, never silently.
#
# It reads the issue/PR number and credentials FROM THE ENVIRONMENT — the env the
# afkd gitea trigger already merges into the worker child (crates/gitea/src/common.rs:
# GITEA_TOKEN / GITEA_BASE_URL / GITEA_REPO / GITEA_PR_NUMBER / GITEA_ISSUE_NUMBER,
# and AFKD_SCRATCH_DIR holding the per-run scratch dir). It does NOT read its own
# path from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses
# — ADR-0017 Revision §5); SKILL.md passes the path on the command line.
#
# Like the sibling helpers it uses only the Python standard library (no curl): the
# stdlib does not build multipart bodies, so the body is assembled by hand (uuid4
# boundary, binary-safe file part). Unlike Trello there is no local size cap —
# Gitea's limit is server-configured, so the server stays the authority and a
# rejected upload surfaces as a non-2xx. It reads GITEA_BASE_URL directly — the
# trigger tests point it at a loopback host, so no separate base override is needed.

import argparse
import http.client
import os
import sys
import urllib.parse
import uuid


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


def multipart_body(filename, content):
    """Build a `multipart/form-data` body by hand; return (content_type, bytes).

    The stdlib does not assemble multipart bodies, so we do: a random boundary
    (`uuid4().hex`, so it cannot collide with the file content) and one file part
    named `attachment` (Gitea's field) carrying `content` VERBATIM as
    application/octet-stream (binary-safe). The display name is not a form part
    here — it rides as the `?name=` query parameter — so there is no text-part loop.
    """
    boundary = uuid.uuid4().hex
    parts = [
        f"--{boundary}".encode(),
        f'Content-Disposition: form-data; name="attachment"; filename="{filename}"'.encode(),
        b"Content-Type: application/octet-stream",
        b"",
        content,
        f"--{boundary}--".encode(),
        b"",
    ]
    body = b"\r\n".join(parts)
    return f"multipart/form-data; boundary={boundary}", body


def post_asset(base, repo, number, name, filename, content, token):
    """POST one asset to the issue/PR; return (status, body-bytes).

    The display `name` rides as the `?name=` query param; the file name and bytes
    travel in the multipart body; the token rides the Authorization header, never a
    query param, so it is not surfaced.
    """
    content_type, body = multipart_body(filename, content)
    query = urllib.parse.urlencode({"name": name})
    conn = connection(base)
    try:
        conn.request(
            "POST",
            f"/api/v1/repos/{repo}/issues/{number}/assets?{query}",
            body,
            {
                "Authorization": f"token {token}",
                "Content-Type": content_type,
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
    parser.add_argument("--name", help="optional display name for the asset")
    args = parser.parse_args()

    # --- The file path: the single positional argument. ---
    if not args.path:
        print(
            "post_attachment.py: a file path is required (pass it as the only positional argument)",
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
            f"post_attachment.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The issue/PR number: GITEA_PR_NUMBER, then GITEA_ISSUE_NUMBER, then
    #     the scratch file fallbacks. ---
    number = issue_number()
    if not number:
        print(
            "post_attachment.py: no issue/PR number (set GITEA_PR_NUMBER or "
            "GITEA_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{pr,issue}/number)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Pre-flight file validation, before opening/reading, so a bad path never
    #     reaches the network. There is no local size cap — Gitea's limit is
    #     server-configured, so a too-large file is rejected by the server. ---
    path = args.path
    if not os.path.exists(path):
        print(f"post_attachment.py: no such file: {path}", file=sys.stderr)
        sys.exit(1)
    if not os.path.isfile(path):
        print(f"post_attachment.py: not a regular file: {path}", file=sys.stderr)
        sys.exit(1)

    # --- Read the file bytes (binary, verbatim) and upload. The issue/PR sees the
    #     basename as the file name and --name (default: basename) as the display
    #     name. A non-2xx is a non-zero exit; the token is never echoed. ---
    filename = os.path.basename(path)
    name = args.name or filename
    with open(path, "rb") as handle:
        content = handle.read()
    status, _ = post_asset(base, repo, number, name, filename, content, token)
    if not 200 <= status < 300:
        print(
            f"post_attachment.py: failed to post {filename} to {repo}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"post_attachment.py: posted {filename} to {repo}#{number}")


if __name__ == "__main__":
    main()
